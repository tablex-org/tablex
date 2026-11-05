// Script runs and parameters, through the real SQLite driver (built on demand, see `sql_driver.rs`)
// and the real Run / Cancel buttons, result tabs, Statistics table and parameter dialog. The live
// PostgreSQL / MySQL / MariaDB runs at the end are `#[ignore]`d like the other live tests.

use crate::grid::GridSource as _;
use crate::script_run::ScriptTab;

/// A three-column table with 5 rows, enough to page through.
const SCRIPT_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); \
     INSERT INTO items(name) VALUES ('a'),('b'),('c'),('d'),('e');";

/// SELECT / UPDATE / DDL / SELECT: two result sets and two commands.
const MIXED_SCRIPT: &str = "SELECT id, name FROM items ORDER BY id;\n\
     UPDATE items SET name = 'z' WHERE id <= 2;\n\
     CREATE TABLE extra(a INTEGER);\n\
     SELECT count(*) AS n FROM items";

impl SqlRig<'_> {
    /// Puts `sql` in the tab and its editor (a programmatic `set_value` does not reach the tab
    /// the way typing does) and presses Run.
    fn run_script(&mut self, sql: &str) {
        let (tab, text) = (self.tab, sql.to_owned());
        let editor = self.app.read_with(self.visual, |this, _| this.query_editors.by_tab.get(&tab).cloned());
        if let Some(editor) = editor {
            self.visual.update(|window, cx| {
                editor.update(cx, |state, cx| state.set_value(text.clone(), window, cx));
            });
        }
        self.app.update(self.visual, |this, _| this.workspace.tab_mut(tab).unwrap().sql = text);
        settle(self.visual);
        self.click("run-query");
    }

    fn wait_script_done(&mut self) {
        self.wait_not_running("the script to finish");
    }

    /// `(status label, rows text)` of every statement of the last script.
    fn statements(&self) -> Vec<(String, String)> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            let script = this.workspace.runtime(tab).unwrap().script.as_ref().expect("a script ran");
            script.reports.iter().map(|r| (r.status.label().to_owned(), r.rows_text())).collect()
        })
    }

    fn script_shown(&self) -> Option<ScriptTab> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().script.as_ref().map(|script| script.shown)
        })
    }

    fn result_tabs(&self) -> usize {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().script.as_ref().map_or(0, |script| script.results.len())
        })
    }

    /// First column of every row of the grid on screen.
    fn column_cells(&self, column: usize) -> Vec<String> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            let grid = &this.workspace.runtime(tab).unwrap().grid;
            (0..grid.row_count())
                .map(|row| {
                    grid.cell_text(crate::grid::CellPosition { row, column })
                        .map(|text| text.to_string())
                        .unwrap_or_default()
                })
                .collect()
        })
    }

    fn set_settings(&mut self, edit: impl FnOnce(&mut crate::settings::Settings)) {
        self.app.update(self.visual, |this, _| {
            let mut next = (*this.settings).clone();
            edit(&mut next);
            this.settings = std::sync::Arc::new(next);
        });
    }

    fn editor_selection(&self) -> String {
        let tab = self.tab;
        let editor = self.app.read_with(self.visual, |this, _| this.query_editors.by_tab[&tab].clone());
        self.app.read_with(self.visual, |_, cx| editor.read(cx).selected_value().to_string())
    }
}

#[gpui::test]
fn a_script_shows_a_tab_per_result_set_and_a_statistics_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script(MIXED_SCRIPT);
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");

    // The statements ran in order on the one session, each once.
    assert_eq!(db.sql("SELECT name FROM items WHERE id = 1"), "z");
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'extra'"), "1");
    assert_eq!(
        rig.statements(),
        [
            ("ok".into(), "5".into()),
            ("ok".into(), "2 affected".into()),
            ("ok".into(), "0 affected".into()),
            ("ok".into(), "1".into()),
        ]
    );
    // Two statements returned rows: Result 1 and Result 2, then Statistics.
    assert_eq!(rig.result_tabs(), 2);
    for selector in ["script-tab-0", "script-tab-1", "script-tab-stats", "script-summary"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector} is drawn");
    }
    assert!(rig.visual.debug_bounds("script-tab-2").is_none(), "no tab for the commands");
    // Result 1 is on screen, as a normal result: its rows, its statement, its paging.
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(0)));
    assert_eq!(rig.column_cells(0), ["1", "2", "3", "4", "5"]);
    // Result 1 was read before the UPDATE ran: the statements run in order.
    assert_eq!(rig.column_cells(1), ["a", "b", "c", "d", "e"]);
}

#[gpui::test]
fn switching_tabs_shows_each_result_and_keeps_one_grid(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script(MIXED_SCRIPT);
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");
    let tab = rig.tab;

    rig.click("script-tab-1");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(1)));
    assert_eq!(rig.column_cells(0), ["5"], "Result 2 is the count");
    rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.last_executed_query.as_deref(), Some("SELECT count(*) AS n FROM items"));
        // The other result is a kept page, not a grid: `grid` is the only `GridState` of the tab.
        assert_eq!(runtime.grid.row_count(), 1);
        assert_eq!(runtime.script.as_ref().unwrap().results[0].source.row_count(), 5);
        assert!(runtime.driver_page_id.is_none(), "no driver page is held");
    });
    assert!(rig.session.try_lock().is_ok());

    rig.click("script-tab-0");
    assert_eq!(rig.column_cells(0), ["1", "2", "3", "4", "5"]);

    // Statistics drops the grid altogether.
    rig.click("script-tab-stats");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Statistics));
    rig.app.read_with(rig.visual, |this, _| {
        let grid = &this.workspace.runtime(tab).unwrap().grid;
        assert_eq!((grid.row_count(), grid.column_count()), (0, 0));
    });
    rig.wait_rendered("script-stats");
    for selector in ["script-stat-row-0", "script-stat-row-1", "script-stat-row-2", "script-stat-row-3"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector} is drawn");
    }
    // The statement and its status are cells of the table.
    for selector in ["script-stat-1-1", "script-stat-1-2", "script-stat-1-3", "script-stat-1-4"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector} is drawn");
    }
}

#[gpui::test]
fn a_statistics_row_focuses_its_statement_and_opens_its_result(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script(MIXED_SCRIPT);
    rig.wait_script_done();
    rig.click("script-tab-stats");
    rig.wait_rendered("script-stat-row-3");

    // A statement with rows: the editor selects it and its result tab opens.
    rig.click("script-stat-row-3");
    assert_eq!(rig.editor_selection(), "SELECT count(*) AS n FROM items");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(1)));

    // A command has no result tab: the editor selects it and Statistics stays.
    rig.click("script-tab-stats");
    rig.wait_rendered("script-stat-row-1");
    rig.click("script-stat-row-1");
    assert_eq!(rig.editor_selection(), "UPDATE items SET name = 'z' WHERE id <= 2");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Statistics));
}

#[gpui::test]
fn an_error_stops_the_script_or_not_by_the_setting(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let script = "SELECT 1 AS a;\nSELECT * FROM nope;\nSELECT 3 AS c;\nUPDATE items SET name = 'x' WHERE id > 0";

    // Stop (the default): the failure is the last statement that ran.
    rig.run_script(script);
    rig.wait_script_done();
    let statements = rig.statements();
    assert_eq!(
        statements.iter().map(|(status, _)| status.as_str()).collect::<Vec<_>>(),
        ["ok", "error", "skipped", "skipped"]
    );
    assert!(statements[1].1.contains("nope"), "the error says why: {:?}", statements[1]);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'x'"), "0", "nothing ran after the failure");
    // The failure is what the user needs to see: Statistics is on screen, the error is logged.
    assert_eq!(rig.script_shown(), Some(ScriptTab::Statistics));
    assert_eq!(rig.result_tabs(), 1);
    assert_eq!(rig.log().len(), 1, "{:?}", rig.log());

    // Continue: the statements after the failure still run.
    rig.set_settings(|settings| settings.editor.on_script_error = crate::settings::OnScriptError::Continue);
    rig.run_script(script);
    rig.wait_script_done();
    assert_eq!(
        rig.statements().iter().map(|(status, _)| status.as_str()).collect::<Vec<_>>(),
        ["ok", "error", "ok", "ok"]
    );
    assert_eq!(rig.result_tabs(), 2);
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'x'"), "5");
}

#[gpui::test]
fn cancel_stops_the_running_statement_and_skips_the_rest(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // Continue would carry on after an error: a cancel must not.
    rig.set_settings(|settings| settings.editor.on_script_error = crate::settings::OnScriptError::Continue);
    rig.run_script(&format!("SELECT 1 AS a;\n{LONG_QUERY};\nUPDATE items SET name = 'x' WHERE id > 0"));
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(250));
    rig.wait_rendered("script-progress");
    assert!(matches!(rig.status(), crate::query::QueryStatus::Running { .. }));
    rig.click("run-query");

    assert_eq!(rig.status(), crate::query::QueryStatus::Cancelled);
    assert_eq!(
        rig.statements().iter().map(|(status, _)| status.as_str()).collect::<Vec<_>>(),
        ["ok", "cancelled", "skipped"]
    );
    // What ran before the cancel is kept.
    assert_eq!(rig.result_tabs(), 1);
    assert_eq!(rig.script_shown(), Some(ScriptTab::Statistics));
    rig.wait_driver_idle();
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'x'"), "0", "the third statement never ran");
    assert!(rig.log().is_empty(), "a cancel is not a failure: {:?}", rig.log());
    assert!(!rig.operation_registered());
}

#[gpui::test]
fn statements_past_the_result_cap_run_and_appear_in_statistics_only(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_settings(|settings| settings.editor.max_result_tabs = 2);
    rig.run_script("SELECT 1 AS a; SELECT 2 AS b; SELECT id FROM items; UPDATE items SET name = 'x' WHERE id > 0");
    rig.wait_script_done();

    assert_eq!(rig.result_tabs(), 2);
    assert_eq!(
        rig.statements(),
        [
            ("ok".into(), "1".into()),
            ("ok".into(), "1".into()),
            ("ok".into(), "5 (rows not kept)".into()),
            ("ok".into(), "5 affected".into()),
        ]
    );
    assert_eq!(db.sql("SELECT count(*) FROM items WHERE name = 'x'"), "5", "every statement ran");
    rig.wait_rendered("script-tab-1");
    assert!(rig.visual.debug_bounds("script-tab-2").is_none());

    // Its Statistics row focuses the statement; there is no result to open.
    rig.click("script-tab-stats");
    rig.wait_rendered("script-stat-row-2");
    rig.click("script-stat-row-2");
    assert_eq!(rig.editor_selection(), "SELECT id FROM items");
    assert_eq!(rig.script_shown(), Some(ScriptTab::Statistics));
}

#[gpui::test]
fn paging_a_later_result_tab_reruns_its_statement_and_keeps_the_others(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(2);
    rig.run_script("SELECT 7 AS a;\nSELECT id FROM items ORDER BY id");
    rig.wait_script_done();
    rig.wait_rendered("script-tab-1");
    rig.click("script-tab-1");
    assert_eq!(rig.column_cells(0), ["1", "2"]);
    assert_eq!(rig.paging(), (0, true), "the kept page knows a next page exists");

    rig.click("page-next");
    rig.wait_not_running("the next page");
    assert_eq!(rig.column_cells(0), ["3", "4"]);
    assert_eq!(rig.paging(), (1, true));
    // Still a script: both tabs are there and the paged one shows its new page again after a visit
    // to the other.
    assert_eq!(rig.result_tabs(), 2);
    assert_eq!(rig.script_shown(), Some(ScriptTab::Result(1)));
    rig.click("script-tab-0");
    assert_eq!(rig.column_cells(0), ["7"]);
    rig.click("script-tab-1");
    assert_eq!(rig.column_cells(0), ["3", "4"]);
    assert_eq!(rig.paging(), (1, true));

    // And back one page.
    rig.click("page-previous");
    rig.wait_not_running("the previous page");
    assert_eq!(rig.column_cells(0), ["1", "2"]);
    assert_eq!(rig.paging(), (0, true));
}

#[gpui::test]
fn a_single_statement_keeps_todays_behaviour_and_replaces_a_script(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);

    // One statement (a trailing `;` is not a second one): no tab strip, a normal result.
    rig.run_script("SELECT id FROM items ORDER BY id;");
    rig.wait_script_done();
    assert!(rig.visual.debug_bounds("script-strip").is_none());
    assert_eq!(rig.rows(), 5);
    assert_eq!(rig.result_tabs(), 0);
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().script.is_none());
    });

    // A script, then a single statement typed in the editor: the script's tabs go.
    rig.run_script(MIXED_SCRIPT);
    rig.wait_script_done();
    rig.wait_rendered("script-strip");
    rig.run_script("SELECT name FROM items WHERE id = 3");
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["c"]);
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("script-strip").is_none(), "a single result has no tab strip");
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().script.is_none());
    });
}

#[gpui::test]
fn a_script_is_read_only_checked_before_anything_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let app = rig.app.clone();
    publish_types(&app, rig.visual, false);
    rig.give_profile(typed("replica"));
    rig.run_script("SELECT 1 AS a; DELETE FROM items WHERE id = 1; SELECT 2 AS b");
    settle(rig.visual);
    assert!(matches!(rig.status(), crate::query::QueryStatus::Failed(_)), "{:?}", rig.status());
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "nothing ran, not even the first read");
}

#[gpui::test]
fn dangerous_statements_are_asked_about_once_for_the_script(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let script = "SELECT 1 AS a;\nDELETE FROM items;\nDROP TABLE items;\nSELECT 2 AS b";
    rig.run_script(script);
    rig.wait_rendered("script-confirm-run");
    assert!(rig.visual.debug_bounds("script-confirm-list").is_some(), "the dangerous statements are listed");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "nothing started");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Cancel: nothing ran.
    rig.click("script-confirm-cancel");
    assert!(rig.visual.debug_bounds("script-confirm-run").is_none());
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Run anyway: the listed statements run.
    rig.run_script(script);
    rig.wait_rendered("script-confirm-run");
    rig.click("script-confirm-run");
    rig.wait_script_done();
    assert_eq!(
        rig.statements().iter().map(|(status, _)| status.as_str()).collect::<Vec<_>>(),
        ["ok", "ok", "ok", "ok"]
    );
    assert_eq!(db.sql("SELECT count(*) FROM sqlite_master WHERE name = 'items'"), "0");
}

#[gpui::test]
fn a_dangerous_statement_the_user_was_not_asked_about_still_fails(cx: &mut TestAppContext) {
    // The confirmation covers the statements the dialog listed; a statement the driver flags
    // that the app did not is refused by the driver, and the script stops there.
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let tab = rig.tab;
    let statements = vec![
        crate::script_run::PlannedStatement { range: 0..8, sql: "SELECT 1".into(), confirmed: false },
        crate::script_run::PlannedStatement { range: 9..27, sql: "DELETE FROM items".into(), confirmed: false },
        crate::script_run::PlannedStatement { range: 28..36, sql: "SELECT 2".into(), confirmed: false },
    ];
    rig.app.update(rig.visual, |this, cx| {
        this.start_run(tab, "SELECT 1".into(), None, None, Some(statements), cx);
    });
    rig.wait_script_done();
    let statuses: Vec<_> = rig.statements().into_iter().map(|(status, _)| status).collect();
    assert_eq!(statuses, ["ok", "error", "skipped"]);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5", "unconfirmed, the DELETE did not run");
}

#[gpui::test]
fn a_session_scoped_statement_carries_to_the_next_one(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_script("CREATE TEMP TABLE scratch(v); INSERT INTO scratch VALUES (41), (42); SELECT sum(v) AS s FROM scratch");
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["83"], "the statements share one session");
}

#[gpui::test]
fn a_production_connection_asks_once_for_the_whole_script(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let app = rig.app.clone();
    publish_types(&app, rig.visual, false);
    rig.give_profile(typed(PRODUCTION));
    rig.run_script("SELECT 1 AS a; SELECT 2 AS b");
    rig.wait_rendered("execute-confirm-run");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle, "asked before anything ran");
    rig.click("execute-confirm-run");
    rig.wait_script_done();
    assert_eq!(rig.result_tabs(), 2);
    assert!(rig.visual.debug_bounds("execute-confirm-run").is_none());
}

// ---- parameters ---------------------------------------------------------------------------------

impl SqlRig<'_> {
    /// Sets the statement and presses Run (the dialog, if there are parameters, opens).
    fn run_with_parameters(&mut self, sql: &str) {
        self.run_script(sql);
        settle(self.visual);
    }

    /// Types `text` into the focused field, replacing what is there.
    fn type_value(&mut self, text: &str) {
        self.visual.simulate_keystrokes("secondary-a");
        self.visual.simulate_input(text);
        settle(self.visual);
    }

    fn dialog_open(&mut self) -> bool {
        self.visual.debug_bounds("param-run").is_some()
    }

    fn remembered(&self, name: &str) -> Option<crate::sql_params::Value> {
        let (tab, name) = (self.tab, name.to_owned());
        self.app.read_with(self.visual, |this, _| {
            this.workspace.runtime(tab).unwrap().param_values.get(&name).cloned()
        })
    }
}

impl SqlRig<'_> {
    /// Picks `row` of a parameter's type select (0 Text, 1 Number, 2 NULL, 3 Raw SQL) the way a
    /// user does: open it, arrow down from the current choice, Enter.
    fn pick_kind(&mut self, selector: &'static str, downs: usize) {
        self.click(selector);
        for _ in 0..downs {
            self.visual.simulate_keystrokes("down");
        }
        self.visual.simulate_keystrokes("enter");
        settle(self.visual);
    }
}

const PARAM_SQL: &str = "SELECT id, name FROM items WHERE id >= :min AND name <> ${skip} AND id <= :min + 10 ORDER BY id";

#[gpui::test]
fn parameters_are_asked_once_each_bound_and_remembered_for_the_next_run(cx: &mut TestAppContext) {
    use crate::sql_params::{Value, ValueKind};
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_with_parameters(PARAM_SQL);

    // Two names, one row each (`:min` is written twice), nothing has run.
    assert!(rig.dialog_open());
    for selector in ["param-input-min", "param-kind-min", "param-input-skip", "param-kind-skip"] {
        assert!(rig.visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);

    // `:min` is a number, `${skip}` text.
    rig.pick_kind("param-kind-min", 1);
    rig.click("param-input-min");
    rig.type_value("2");
    rig.click("param-input-skip");
    rig.type_value("d");
    // Enter runs.
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert!(!rig.dialog_open());
    assert_eq!(rig.column_cells(0), ["2", "3", "5"]);
    assert_eq!(rig.column_cells(1), ["b", "c", "e"]);
    assert_eq!(rig.remembered("min"), Some(Value { kind: ValueKind::Number, text: "2".into() }));
    assert_eq!(rig.remembered("skip"), Some(Value { kind: ValueKind::Text, text: "d".into() }));
    // What ran is the bound statement: it is the one paging and Refresh re-send.
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        let sql = this.workspace.runtime(tab).unwrap().last_executed_query.clone().unwrap();
        assert!(sql.contains(">= 2 AND name <> 'd' AND id <= 2 + 10"), "{sql}");
    });

    // Run again: the dialog is filled in with the same values, so Enter alone runs the same query.
    rig.run_with_parameters(PARAM_SQL);
    assert!(rig.dialog_open());
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["2", "3", "5"]);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn escape_cancels_the_prompt_and_nothing_runs(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_with_parameters("DELETE FROM items WHERE id = :id");
    assert!(rig.dialog_open());
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert!(!rig.dialog_open());
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
    assert!(!rig.operation_registered());
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // The Cancel button does the same.
    rig.run_with_parameters("DELETE FROM items WHERE id = :id");
    rig.click("param-cancel");
    assert!(!rig.dialog_open());
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

#[gpui::test]
fn a_text_value_stays_a_literal_whatever_it_says(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_with_parameters("SELECT id FROM items WHERE name = :name");
    rig.type_value("a'; DROP TABLE items; --");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    // One statement ran, with the whole text as its literal: no row matches, the table is there.
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { rows: 0 }), "{:?}", rig.status());
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert!(runtime.script.is_none(), "still one statement");
        assert_eq!(
            runtime.last_executed_query.as_deref(),
            Some("SELECT id FROM items WHERE name = 'a''; DROP TABLE items; --'")
        );
    });
}

#[gpui::test]
fn null_and_raw_sql_values_are_written_as_chosen(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    db.sql("INSERT INTO items(name) VALUES (NULL)");
    let mut rig = SqlRig::sqlite(cx, &db);

    // NULL: the type choice alone, the text is not used.
    rig.run_with_parameters("SELECT id FROM items WHERE name IS :name");
    rig.pick_kind("param-kind-name", 2);
    rig.click("param-run");
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["6"]);

    // Raw SQL goes in as typed.
    rig.run_with_parameters("SELECT id FROM items WHERE id = :expr");
    rig.pick_kind("param-kind-expr", 3);
    rig.click("param-input-expr");
    rig.type_value("1 + 2");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["3"]);
}

#[gpui::test]
fn a_number_that_is_not_one_keeps_the_dialog_open_and_runs_nothing(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_with_parameters("DELETE FROM items WHERE id = :id");
    rig.pick_kind("param-kind-id", 1);
    rig.click("param-input-id");
    rig.type_value("1 OR 1=1");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    assert!(rig.dialog_open(), "the dialog stays");
    assert!(rig.visual.debug_bounds("param-error").is_some(), "and says why");
    assert_eq!(rig.status(), crate::query::QueryStatus::Idle);
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");

    // Fixed, it runs.
    rig.type_value("1");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert_eq!(db.sql("SELECT count(*) FROM items"), "4");
}

#[gpui::test]
fn a_script_asks_for_its_parameters_once(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.run_with_parameters("SELECT :n AS a; SELECT :n + 1 AS b; SELECT ':n' AS literal");
    assert!(rig.dialog_open());
    rig.pick_kind("param-kind-n", 1);
    rig.click("param-input-n");
    rig.type_value("41");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert!(!rig.dialog_open(), "asked once");
    // The third statement's `':n'` is a string, not a parameter.
    assert_eq!(rig.result_tabs(), 3);
    assert_eq!(rig.column_cells(0), ["41"]);
    rig.click("script-tab-1");
    assert_eq!(rig.column_cells(0), ["42"]);
}

#[gpui::test]
fn text_without_parameters_and_the_setting_off_run_without_a_prompt(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // Not parameters: a string, a comment, a SQLite `::`-less cast-looking text.
    rig.run_with_parameters("SELECT ':a' AS s, '${b}' AS t -- :c\n, 1 AS n");
    assert!(!rig.dialog_open());
    rig.wait_script_done();
    assert!(matches!(rig.status(), crate::query::QueryStatus::Ready { rows: 1 }), "{:?}", rig.status());

    // Parameters, with the setting off: the text goes to the driver as written.
    rig.set_settings(|settings| settings.editor.parameters = false);
    rig.run_with_parameters("SELECT :a AS v");
    assert!(!rig.dialog_open());
    rig.wait_script_done();
    assert!(!matches!(rig.status(), crate::query::QueryStatus::Idle));
}

#[gpui::test]
fn explain_asks_for_parameters_the_same_way(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT * FROM items WHERE id = :id".into();
        this.explain_active_tab(engine_plan::ExplainMode::Estimate, cx);
    });
    settle(rig.visual);
    assert!(rig.dialog_open(), "the Explain prompts");
    rig.pick_kind("param-kind-id", 1);
    rig.click("param-input-id");
    rig.type_value("3");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    let plan = rig.app.read_with(rig.visual, |this, _| this.workspace.runtime(tab).unwrap().plan.is_some());
    assert!(plan, "the plan of the bound statement is shown");
    assert_eq!(db.sql("SELECT count(*) FROM items"), "5");
}

// ---- live servers (ignored: they need the dedicated tablex-test-* containers) -------------------

/// A script of DDL, DML and two reads on a live server, with a text value that only survives if
/// its quoting is right for the dialect (a backslash and a quote).
fn live_script(cx: &mut TestAppContext, kind: ConnectionKind, driver: (&'static str, &'static str), env: &str) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let table = format!("tablex_script_{}", uuid::Uuid::new_v4().to_simple());
    let script = format!(
        "CREATE TABLE {table}(id int PRIMARY KEY, name varchar(40));\n\
         INSERT INTO {table} VALUES (1, 'a'), (2, 'b');\n\
         SELECT id, name FROM {table} ORDER BY id;\n\
         UPDATE {table} SET name = 'z' WHERE id = 1;\n\
         SELECT count(*) AS n FROM {table};\n\
         DROP TABLE {table}"
    );
    rig.run_script(&script);
    rig.wait_rendered("script-confirm-run");
    rig.click("script-confirm-run");
    rig.wait_script_done();
    assert_eq!(
        rig.statements(),
        [
            ("ok".into(), "0 affected".into()),
            ("ok".into(), "2 affected".into()),
            ("ok".into(), "2".into()),
            ("ok".into(), "1 affected".into()),
            ("ok".into(), "1".into()),
            ("ok".into(), "0 affected".into()),
        ]
    );
    assert_eq!(rig.result_tabs(), 2);

    // The value is read back exactly as typed.
    rig.run_with_parameters("SELECT :t AS t");
    assert!(rig.dialog_open());
    rig.type_value("a\\'b; -- \"c\"");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    rig.wait_script_done();
    assert_eq!(rig.column_cells(0), ["a\\'b; -- \"c\""]);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_script_and_parameters(cx: &mut TestAppContext) {
    live_script(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), "TABLEX_TEST_PG_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_script_and_parameters(cx: &mut TestAppContext) {
    live_script(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_script_and_parameters(cx: &mut TestAppContext) {
    live_script(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI");
}

// ---- cost ----------------------------------------------------------------------------------------

/// Run on demand (informational, no timing asserts; allocation counts do not depend on the profile):
///
///   cargo test -j 6 -p tablex-gpui script_run_memory -- --ignored --nocapture --test-threads=1
///
/// A 50-statement script with 10 result sets of 500 rows: what the run keeps, how many grids are
/// alive, what showing a tab and drawing a frame cost.
#[gpui::test]
#[ignore = "on demand"]
fn script_run_memory_and_frame_cost(cx: &mut TestAppContext) {
    let db = SqliteDb::new(SCRIPT_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.set_page_size(500);
    let select = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c WHERE x < 500) \
                  SELECT x, 'row ' || x AS t FROM c";
    let script = (0..50)
        .map(|index| if index % 5 == 0 { select.to_owned() } else { "UPDATE items SET name = name WHERE id > 0".into() })
        .collect::<Vec<_>>()
        .join(";\n");
    settle(rig.visual);
    // The same script with a cap of one result tab: the heap it leaves behind is the baseline, so
    // the difference is what nine more kept results cost.
    rig.set_settings(|settings| settings.editor.max_result_tabs = 1);
    rig.run_script(&script);
    rig.wait_script_done();
    settle(rig.visual);
    let heap_one = crate::grid_bench::live_bytes();
    rig.set_settings(|settings| settings.editor.max_result_tabs = 20);
    settle(rig.visual);
    let heap_before = crate::grid_bench::live_bytes();
    let started = std::time::Instant::now();
    rig.run_script(&script);
    rig.wait_script_done();
    settle(rig.visual);
    let elapsed = started.elapsed();
    let heap_after = crate::grid_bench::live_bytes();

    let tab = rig.tab;
    let (results, statements, retained, grids) = rig.app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        let script = runtime.script.as_ref().unwrap();
        // `TabRuntime::grid` is the tab's only `GridState`; a result tab holds a kept page.
        (script.results.len(), script.reports.len(), script.retained_bytes(), 1)
    });
    assert_eq!((results, statements, grids), (10, 50, 1));
    eprintln!("script: {statements} statements, {results} result tabs, {grids} live grid");
    eprintln!("  run to settled UI: {elapsed:?} (debug build; includes the driver and 50 round trips)");
    eprintln!("  retained row data: {retained} bytes ({} per result)", retained / results);
    eprintln!(
        "  live heap: {} bytes with 1 kept result, {} with {results}: {} per additional result (rows are {} Arrow bytes)",
        heap_one,
        heap_after,
        heap_after.saturating_sub(heap_one) / (results as u64 - 1),
        retained / results
    );
    let _ = heap_before;

    // Showing another result tab builds one grid from its kept page.
    let (allocs, bytes) = counted(|| {
        rig.click("script-tab-5");
    });
    eprintln!("  switching to a result tab (click, layout, paint): {allocs} allocations, {bytes} bytes");
    let (allocs, bytes) = counted(|| {
        rig.app.update(rig.visual, |this, cx| this.show_script_tab(tab, ScriptTab::Result(2), cx));
    });
    eprintln!("  show_script_tab alone: {allocs} allocations, {bytes} bytes");

    // A frame (the view re-rendered, as after any change to the tab) of: a result tab of the
    // script, the Statistics table, and the same 500 rows as a plain single result.
    let frame = |rig: &mut SqlRig<'_>| {
        let _warm = counted(|| {
            rig.app.update(rig.visual, |this, cx| this.notify_tab(tab, cx));
            rig.visual.update(|window, _| window.refresh());
            rig.visual.run_until_parked();
        });
        counted(|| {
            for _ in 0..10 {
                rig.app.update(rig.visual, |this, cx| this.notify_tab(tab, cx));
                rig.visual.update(|window, _| window.refresh());
                rig.visual.run_until_parked();
            }
        })
        .0 / 10
    };
    let in_script = frame(&mut rig);
    eprintln!("  frame of a result tab of the script: {in_script} allocations");
    rig.click("script-tab-stats");
    rig.wait_rendered("script-stats");
    let statistics = frame(&mut rig);
    eprintln!("  frame of the Statistics table (50 rows): {statistics} allocations");
    rig.run_script(select);
    rig.wait_script_done();
    settle(rig.visual);
    let plain = frame(&mut rig);
    eprintln!("  frame of the same 500 rows as a single result: {plain} allocations");
    eprintln!("  the strip adds {} allocations per frame", in_script as i64 - plain as i64);
}

/// `(allocations, bytes allocated)` while `work` runs.
fn counted(work: impl FnOnce()) -> (u64, u64) {
    let (allocs, bytes) = (crate::grid_bench::allocation_count(), crate::grid_bench::allocation_bytes());
    work();
    (crate::grid_bench::allocation_count() - allocs, crate::grid_bench::allocation_bytes() - bytes)
}
