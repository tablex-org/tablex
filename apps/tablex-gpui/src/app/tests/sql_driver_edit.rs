// Driver-route grid edits and Properties. They reuse the real-SQLite-driver
// harness of `sql_driver.rs` (the real host loader, the real driver dylib, the
// real inline editor, dialogs and Apply): staged edits go out as ONE `batch`
// mutation, composite keys come from the primary-key flags, and Properties is
// served by `describe_object`. The live PostgreSQL / MySQL / MariaDB smoke
// tests at the end are `#[ignore]`d and need `TABLEX_TEST_PG_URI`,
// `TABLEX_TEST_MYSQL_URI` and `TABLEX_TEST_MARIADB_URI`.

const EDIT_ITEMS_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT, qty INTEGER); \
     INSERT INTO items(name, qty) VALUES ('a',1),('b',2),('c',3);";
const ITEMS_DUMP: &str = "SELECT group_concat(id || ':' || name || ':' || qty, '|') \
     FROM (SELECT * FROM items ORDER BY id)";

impl SqlRig<'_> {
    /// Turns the rig's tab into the table-data tab of SQLite's `table`.
    fn open_table(&mut self, table: &str) {
        self.open_table_in("main", None, table);
    }

    /// Turns the rig's tab into the table-data tab of `table`, loads the
    /// table's columns through the driver (the way the Properties view does,
    /// `describe_object`) and runs its SELECT.
    fn open_table_in(&mut self, database: &str, schema: Option<&str>, table: &str) {
        let tab = self.tab;
        let columns = describe_columns(&self.session, database, schema, table);
        let (database, schema, table) = (database.to_string(), schema.map(str::to_string), table.to_string());
        self.app.update(self.visual, |this, cx| {
            let target = this.workspace.tab_mut(tab).unwrap();
            target.database = database;
            target.schema = schema;
            target.collection = table.clone();
            target.query_mode = Some(QueryMode::SqlTable);
            let target = target.clone();
            let sql = crate::sql_table::base_select(this.sql_table_dialect(&target), target.schema.as_deref(), &table).unwrap();
            this.workspace.tab_mut(tab).unwrap().sql = sql;
            this.workspace.runtime_mut(tab).unwrap().structure_columns = columns;
            this.run_active_tab_query(cx);
        });
        self.wait_status("the table SELECT", |status| {
            matches!(status, crate::query::QueryStatus::Ready { .. })
        });
    }

    /// Applies new WHERE / ORDER BY fragments and reruns the table SELECT.
    fn rerun_table(&mut self, where_clause: &str, order_by: &str) {
        let tab = self.tab;
        let (where_clause, order_by) = (where_clause.to_string(), order_by.to_string());
        self.app.update(self.visual, |this, cx| {
            let target = this.workspace.tab_mut(tab).unwrap();
            target.applied_where = where_clause;
            target.applied_order_by = order_by;
            this.workspace.runtime_mut(tab).unwrap().pagination.reset();
            this.run_active_tab_query(cx);
        });
        self.wait_status("the filtered SELECT", |status| {
            matches!(status, crate::query::QueryStatus::Ready { .. })
        });
    }

    /// The grid's cells as text.
    fn grid_rows(&self) -> Vec<Vec<String>> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.active_grid().row_count())
                .map(|row| {
                    (0..this.active_grid().column_count())
                        .map(|column| {
                            this.active_grid()
                                .full_cell_text(CellPosition { row, column })
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .collect()
        })
    }

    /// Turns the rig's tab into the table-data tab a restored session opens: the
    /// saved target and WHERE / ORDER BY, no structure loaded (nothing opened
    /// it from the sidebar), then runs it.
    fn open_restored_table(&mut self, (database, schema): (&str, &str), table: &str, where_clause: &str, order_by: &str) {
        let tab = self.tab;
        let (database, schema) = (database.to_string(), schema.to_string());
        let (table, where_clause, order_by) =
            (table.to_string(), where_clause.to_string(), order_by.to_string());
        self.app.update(self.visual, |this, cx| {
            let target = this.workspace.tab_mut(tab).unwrap();
            target.database = database;
            target.schema = Some(schema);
            target.collection = table.clone();
            target.query_mode = Some(QueryMode::SqlTable);
            target.applied_where = where_clause.clone();
            target.applied_order_by = order_by.clone();
            target.table_where = where_clause;
            target.table_order_by = order_by;
            let target = target.clone();
            let sql = crate::sql_table::base_select(this.sql_table_dialect(&target), target.schema.as_deref(), &table).unwrap();
            this.workspace.tab_mut(tab).unwrap().sql = sql;
            this.view_mut(tab).grid_ui.inline_edit = None;
            this.workspace.runtime_mut(tab).unwrap().structure_columns.clear();
            this.run_active_tab_query(cx);
        });
        self.wait_status("the restored table SELECT", |status| {
            matches!(status, crate::query::QueryStatus::Ready { .. })
        });
    }

    /// Waits until the tab's table structure (keys, types) is loaded.
    fn wait_structure(&mut self) {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, "the table structure", |visual| {
            app.read_with(visual, |this, _| {
                !this.workspace.runtime(tab).unwrap().structure_columns.is_empty()
            })
        });
    }

    /// Double-clicks the painted cell the way a user does (a click, then the
    /// second press with `click_count` 2).
    fn double_click_cell(&mut self, row: usize, column: usize) {
        let selector: &'static str = Box::leak(format!("grid-body-cell-{row}-{column}").into_boxed_str());
        self.wait_rendered(selector);
        let center = self.visual.debug_bounds(selector).expect("the cell is painted").center();
        double_click(self.visual, center);
    }

    /// The cell the inline editor is open on.
    fn editing(&self) -> Option<CellPosition> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| this.view(tab).grid_ui.inline_edit.as_ref().map(|edit| edit.position))
    }

    /// Edits one cell through the real inline editor.
    fn edit_cell(&mut self, row: usize, column: usize, text: &str) {
        self.app.update_in(self.visual, |this, window, cx| {
            let tab = this.workspace.active_tab().cloned().expect("active tab");
            this.begin_inline_edit(tab.id, row, column, window, cx);
        });
        settle(self.visual);
        let editor = self.app.read_with(self.visual, |this, _| {
            this.av().grid_ui
                .inline_edit
                .as_ref()
                .expect("the inline editor opens")
                .editor
                .clone()
        });
        editor
            .as_input()
            .expect("a short value uses the single-line input")
            .update_in(self.visual, |state, window, cx| state.set_value(text, window, cx));
        self.app.update_in(self.visual, |this, window, cx| {
            this.commit_inline_edit(this.active_id(), InlineEditExit::Submit, Some(window), cx);
        });
        settle(self.visual);
    }

    /// Stages an insert the way the "Stage insert" dialog does.
    fn stage_insert(&mut self, json: &str) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            let target = this.workspace.tab(tab).unwrap().clone();
            let columns = this.workspace.runtime(tab).unwrap().structure_columns.clone();
            let request = crate::staged_sql::InsertRowRequest {
                database: target.database.clone(),
                schema: this.sql_row_schema(&target),
                table: target.collection.clone(),
                values: insert_values_from_json(json, &columns).expect("valid insert values"),
            };
            this.stage_sql_insert(request, tab, cx);
        });
    }

    /// Stages the deletion of `row` through the real confirmation dialog.
    fn delete_row(&mut self, row: usize) {
        self.app.update_in(self.visual, |this, window, cx| {
            this.av().grid_ui.selection.borrow_mut().select(CellPosition { row, column: 0 });
            this.confirm_delete_selected_row(this.active_id(), window, cx);
        });
        settle(self.visual);
        self.visual.simulate_keystrokes("enter");
        settle(self.visual);
    }

    /// Apply, confirmed in the real dialog.
    fn apply(&mut self) {
        self.app.update_in(self.visual, |this, window, cx| {
            this.confirm_apply_sql_changes(this.active_id(), window, cx);
        });
        settle(self.visual);
        self.visual.simulate_keystrokes("enter");
        settle(self.visual);
    }

    fn wait_apply_finished(&mut self) {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, "Apply to finish", |visual| {
            app.read_with(visual, |this, _| {
                !this.workspace.runtime(tab).unwrap().sql_apply_running
                    && (!this.error_log.log.is_empty()
                        || this.workspace.runtime(tab).unwrap().pending_edits.is_empty()
                            && this.workspace.runtime(tab).unwrap().pending_sql_rows.is_empty())
            })
        });
        settle(self.visual);
    }

    /// `(pending cell edits, pending row operations)`.
    fn staged(&self) -> (usize, usize) {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            let runtime = this.workspace.runtime(tab).unwrap();
            (runtime.pending_edits.len(), runtime.pending_sql_rows.len())
        })
    }

    /// The `batch` request the staged changes would be sent as.
    fn batch_request(&self) -> engine_driver_api::MutationRequest {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| {
            let target = this.workspace.tab(tab).unwrap();
            let runtime = this.workspace.runtime(tab).unwrap();
            this.driver_sql_batch_request(target, &runtime.pending_edits, &runtime.pending_sql_rows)
                .expect("the staged changes form a batch")
        })
    }

    fn detail_error(&self) -> Option<String> {
        self.app.read_with(self.visual, |this, _| this.av().detail_ui.error.clone())
    }
}

/// The columns `describe_object` reports for `table`, as the catalog model.
fn describe_columns(
    session: &std::sync::Arc<engine_driver_host::SharedDriverSession>,
    database: &str,
    schema: Option<&str>,
    table: &str,
) -> Vec<crate::catalog::CatalogColumn> {
    let described = session
        .lock()
        .unwrap()
        .describe_object(&serde_json::json!({ "database": database, "schema": schema, "object": table }))
        .expect("the driver describes the table");
    let properties: crate::catalog::TableProperties =
        serde_json::from_value(described).expect("the describe answer parses");
    properties.columns
}

#[gpui::test]
fn driver_route_apply_sends_edit_insert_and_delete_as_one_batch_and_persists(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    assert_eq!(rig.rows(), 3);

    rig.edit_cell(0, 1, "alpha");
    // A second edit of the same row travels in the same update.
    rig.edit_cell(0, 2, "10");
    rig.stage_insert(r#"{"name":"new","qty":7}"#);
    rig.delete_row(2);
    assert_eq!(rig.staged(), (2, 2));
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "nothing is written before Apply");

    let request = rig.batch_request();
    assert_eq!(request.kind, "batch");
    assert_eq!(request.object.as_deref(), Some("items"));
    assert_eq!(request.database.as_deref(), Some("main"));
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
    assert_eq!(db.sql(ITEMS_DUMP), "1:alpha:10|2:b:2|4:new:7");
    assert_eq!(rig.staged(), (0, 0));
    assert_eq!(rig.detail_error(), None);
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    // The table reloaded from the database.
    rig.wait_not_running("the reload after Apply");
    assert_eq!(rig.rows(), 3);
    let first_name = rig.app.read_with(rig.visual, |this, _| {
        this.active_grid().full_cell_text(CellPosition { row: 0, column: 1 })
    });
    assert_eq!(first_name.as_deref(), Some("alpha"));
}

#[gpui::test]
fn double_clicking_a_cell_of_a_filtered_sorted_table_opens_the_inline_editor(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table_in("main", Some("main"), "items");
    rig.rerun_table("id > 1 AND qty > 0", "id DESC");
    assert_eq!(rig.grid_rows()[0][0], "3");

    rig.double_click_cell(1, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 1, column: 1 }));
}

#[gpui::test]
fn a_restored_table_tab_loads_its_structure_so_a_double_click_edits(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_restored_table(("main", "main"), "items", "id > 1 AND qty > 0", "id DESC");
    assert_eq!(rig.grid_rows()[0][0], "3");
    // Nothing opened the table from the sidebar: the run itself loads the keys.
    rig.wait_structure();

    rig.double_click_cell(1, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 1, column: 1 }));
    assert_eq!(rig.detail_error(), None);
}

#[gpui::test]
fn a_double_click_before_the_structure_is_known_says_why_and_loads_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_restored_table(("main", "main"), "items", "", "");
    rig.wait_structure();
    // The keys are gone (a failed or superseded load): the next edit asks again.
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, _| {
        this.workspace.runtime_mut(tab).unwrap().structure_columns.clear();
    });

    // Straight after the click, before the load had time to answer.
    rig.app.update_in(rig.visual, |this, window, cx| {
        this.begin_inline_edit(tab, 0, 1, window, cx);
    });
    assert_eq!(rig.editing(), None);
    assert_eq!(
        rig.detail_error().as_deref(),
        Some("Loading the table structure; try again in a moment")
    );
    rig.wait_structure();
    assert_eq!(rig.detail_error(), None, "the load answers its own notice");
    rig.double_click_cell(0, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 0, column: 1 }));
    assert_eq!(rig.detail_error(), None, "opening the editor clears the message");
}

#[gpui::test]
fn a_double_click_on_a_table_without_a_key_or_during_a_run_says_why(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE journal(msg TEXT); INSERT INTO journal VALUES ('x'),('y'); \
         CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); INSERT INTO items(name) VALUES ('a');",
    );
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table_in("main", Some("main"), "journal");
    rig.double_click_cell(0, 0);
    assert_eq!(rig.editing(), None);
    assert_eq!(rig.detail_error().as_deref(), Some("A single primary key is required"));

    rig.open_table_in("main", Some("main"), "items");
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, _| {
        // A run in flight: its result is about to replace the rows.
        this.workspace.runtime_mut(tab).unwrap().query.begin().expect("a run begins");
    });
    rig.double_click_cell(0, 1);
    assert_eq!(rig.editing(), None);
    assert_eq!(rig.detail_error().as_deref(), Some("Wait for the running query to finish"));
}

#[gpui::test]
fn double_clicking_a_cell_in_split_panes_opens_the_editor_in_the_clicked_pane(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table_in("main", Some("main"), "items");
    rig.rerun_table("id > 1", "id DESC");
    let tab = rig.tab;
    // The table tab beside a second tab, each in its own pane.
    let first_pane = rig.app.update(rig.visual, |this, cx| {
        let first_pane = this.workspace.panes().focused();
        this.workspace.open_ran_query_tab("other");
        this.split_tab_ui(tab, crate::panes::Edge::Right, cx);
        first_pane
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.workspace.panes().pane_count(), 2, "two panes");
    });

    rig.double_click_cell(0, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 0, column: 1 }));
    assert_eq!(rig.detail_error(), None);

    // Focus the other pane, then double-click another cell of the table: the
    // press focuses its pane and still opens the editor.
    rig.app.update(rig.visual, |this, cx| {
        this.view_mut(tab).grid_ui.inline_edit = None;
        this.focus_pane_ui(first_pane, cx);
    });
    settle(rig.visual);
    rig.double_click_cell(1, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 1, column: 1 }));
}

#[gpui::test]
fn a_composite_primary_key_table_is_edited_by_its_whole_key(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE tenant_items(tenant INTEGER, sku TEXT, name TEXT, PRIMARY KEY(tenant, sku)); \
         INSERT INTO tenant_items VALUES (1,'a','one'),(1,'b','two'),(2,'a','three');",
    );
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("tenant_items");
    assert_eq!(rig.rows(), 3);

    rig.edit_cell(1, 2, "TWO");
    rig.delete_row(2);
    assert_eq!(rig.staged(), (1, 1), "a composite key can be edited on the driver route");
    assert_eq!(
        rig.batch_request().payload,
        serde_json::json!({"changes": [
            {"op": "update", "key": {"tenant": 1, "sku": "b"}, "values": {"name": "TWO"}},
            {"op": "delete", "key": {"tenant": 2, "sku": "a"}},
        ]}),
        "every primary-key column identifies the row"
    );

    rig.apply();
    rig.wait_apply_finished();
    let dump = "SELECT group_concat(tenant || ':' || sku || ':' || name, '|') \
                FROM (SELECT * FROM tenant_items ORDER BY tenant, sku)";
    assert_eq!(
        db.sql(dump),
        "1:a:one|1:b:TWO",
        "the sibling row sharing the tenant is untouched"
    );
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn a_table_without_a_primary_key_is_not_editable(cx: &mut TestAppContext) {
    let db = SqliteDb::new("CREATE TABLE journal(msg TEXT); INSERT INTO journal VALUES ('x'),('y');");
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("journal");
    assert_eq!(rig.rows(), 2);

    rig.app.update_in(rig.visual, |this, window, cx| {
        let tab = this.workspace.active_tab().cloned().unwrap();
        this.begin_inline_edit(tab.id, 0, 0, window, cx);
        assert!(this.av().grid_ui.inline_edit.is_none(), "no editor without a key");
        let detail = this.build_cell_detail(&tab, CellPosition { row: 0, column: 0 }).unwrap();
        assert!(!detail.editable);
        assert_eq!(
            this.prepare_sql_delete(this.active_id(), 0).unwrap_err().to_string(),
            "A single primary key is required",
            "the existing message"
        );
    });
    assert_eq!(rig.staged(), (0, 0));
}

#[gpui::test]
fn a_failing_change_rolls_the_whole_batch_back_and_reaches_the_error_log(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    rig.delete_row(2);
    // Another client removes the row first: the delete matches nothing.
    db.sql("DELETE FROM items WHERE id = 3");

    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(
        db.sql(ITEMS_DUMP),
        "1:a:1|2:b:2",
        "the earlier update was rolled back with the failed delete: one transaction"
    );
    assert_eq!(rig.staged(), (1, 1), "everything stays staged for a retry");
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0.as_deref(), Some("mutation.no_rows_affected"));
    assert_eq!(log[0].1, crate::error_log::Severity::Error);
    assert!(rig.detail_error().is_some_and(|error| error.contains("mutation.no_rows_affected")));
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        assert!(!this.workspace.runtime(tab).unwrap().sql_apply_running);
    });
}

#[gpui::test]
fn apply_inside_a_user_transaction_joins_it_instead_of_committing(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    // The user's transaction is open on the connection's shared session (the driver's contract: a
    // mutation joins it through a SAVEPOINT). A tab that types BEGIN now has a session of its own,
    // so the transaction is opened on the shared one directly.
    exec_confirmed(&rig.session, "BEGIN");
    rig.open_table("items");
    rig.edit_cell(1, 1, "inside");
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(rig.staged(), (0, 0));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(
        db.sql(ITEMS_DUMP),
        "1:a:1|2:b:2|3:c:3",
        "the driver did not commit the user's transaction"
    );

    // The user's ROLLBACK discards the applied edit, so the mutation really lived inside the
    // transaction.
    exec_confirmed(&rig.session, "ROLLBACK");
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3");
    assert!(rig.log().is_empty(), "{:?}", rig.log());
}

#[gpui::test]
fn apply_on_a_dropped_session_marks_the_connection_lost_and_keeps_the_staging(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    rig.session.lock().unwrap().disconnect().expect("the driver disconnects");

    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3");
    assert_eq!(rig.staged(), (1, 0));
    let log = rig.log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(log[0].0.as_deref(), Some("session.not_connected"));
    let key = rig.key.clone();
    rig.app.read_with(rig.visual, |this, _| {
        assert!(
            matches!(this.sessions.status(&key), ConnectionStatus::Failed(_)),
            "the connection is flagged for reconnect"
        );
    });
}

#[test]
fn staged_values_travel_as_exact_json() {
    use results::editing::mutation_value_to_json;
    assert_eq!(mutation_value_to_json(&MutationValue::Null), serde_json::Value::Null);
    assert_eq!(mutation_value_to_json(&MutationValue::Boolean(true)), serde_json::json!(true));
    assert_eq!(mutation_value_to_json(&MutationValue::Text("a\\b'c".into())), serde_json::json!("a\\b'c"));
    let number = |text: &str| mutation_value_to_json(&MutationValue::Number(text.into()));
    assert_eq!(number("42"), serde_json::json!(42));
    assert_eq!(number("-7"), serde_json::json!(-7));
    assert_eq!(number("18446744073709551615"), serde_json::json!(18446744073709551615u64));
    // Every number that is not a whole number keeps its exact text.
    assert_eq!(number("1.5"), serde_json::json!("1.5"));
    assert_eq!(number("1.50"), serde_json::json!("1.50"));
    assert_eq!(number("12345678901234567890.123456789"), serde_json::json!("12345678901234567890.123456789"));
}

#[gpui::test]
fn properties_render_what_the_driver_describes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(
        "CREATE TABLE customers(id INTEGER PRIMARY KEY, email TEXT UNIQUE);\n\
         CREATE TABLE orders(id INTEGER PRIMARY KEY, customer_id INTEGER REFERENCES customers(id), \
         qty INTEGER CHECK (qty > 0));\n\
         CREATE INDEX orders_customer_idx ON orders(customer_id);\n\
         CREATE TRIGGER orders_ai AFTER INSERT ON orders BEGIN SELECT 1; END;\n\
         INSERT INTO customers VALUES (1, 'a@example.com');\n\
         INSERT INTO orders VALUES (1, 1, 2), (2, 1, 3);",
    );
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table("orders");
    settle(rig.visual);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let wait_loaded = |visual: &mut gpui::VisualTestContext| {
        wait_until(visual, "the properties fetch", |visual| {
            app.read_with(visual, |this, _| {
                this.workspace.runtime(tab).unwrap().columns_panel.props.status != LoadStatus::Loading
            })
        });
        settle(visual);
    };

    properties_open(rig.visual);
    wait_loaded(rig.visual);
    let (_, status, fetches, visible) = properties_snapshot(&app, rig.visual);
    assert_eq!(status, LoadStatus::Loaded, "describe_object answered");
    assert_eq!(fetches, 1);
    assert_eq!(visible, PropertiesTab::ALL, "the SQLite driver reports every list");
    assert!(rig.visual.debug_bounds("columns-view-row").is_some(), "Columns renders");

    properties_click_tab(rig.visual, PropertiesTab::Keys);
    let (headers, keys, ..) = properties_table(&app, rig.visual, PropertiesTab::Keys);
    assert!(headers.starts_with(&["Name", "Type", "Columns"]), "{headers:?}");
    assert!(keys.iter().any(|row| row[1] == "PK" && row[2] == "id"), "{keys:?}");
    assert_eq!(properties_rendered_rows(rig.visual), keys.len());

    properties_click_tab(rig.visual, PropertiesTab::Indexes);
    let (_, indexes, ..) = properties_table(&app, rig.visual, PropertiesTab::Indexes);
    assert!(
        indexes.iter().any(|row| row[0] == "orders_customer_idx" && row[1] == "customer_id"),
        "{indexes:?}"
    );

    properties_click_tab(rig.visual, PropertiesTab::ForeignKeys);
    let (_, fks, ..) = properties_table(&app, rig.visual, PropertiesTab::ForeignKeys);
    assert!(fks.iter().any(|row| row.iter().any(|cell| cell.starts_with("customers"))), "{fks:?}");

    properties_click_tab(rig.visual, PropertiesTab::Triggers);
    let (_, triggers, ..) = properties_table(&app, rig.visual, PropertiesTab::Triggers);
    assert!(triggers.iter().any(|row| row[0] == "orders_ai"), "{triggers:?}");

    // Nothing references `orders`: reported by the driver, rendered empty.
    properties_click_tab(rig.visual, PropertiesTab::References);
    let (_, references, ..) = properties_table(&app, rig.visual, PropertiesTab::References);
    assert!(references.is_empty(), "{references:?}");
    assert!(rig.visual.debug_bounds("properties-empty").is_some());

    properties_click_tab(rig.visual, PropertiesTab::Ddl);
    app.read_with(rig.visual, |this, cx| {
        let editor = this.workspace.runtime(tab).unwrap().columns_panel.props.ddl.as_ref().expect("DDL editor");
        let text = editor.state.read(cx).value();
        assert!(text.contains("CREATE TABLE") && text.contains("orders"), "{text}");
    });
    assert!(rig.visual.debug_bounds("properties-ddl-editor").is_some());

    // Re-running the result does not refetch; the refresh icon does.
    app.update(rig.visual, |this, cx| this.run_active_tab_query(cx));
    rig.wait_status("the re-run", |status| matches!(status, crate::query::QueryStatus::Ready { .. }));
    assert_eq!(properties_snapshot(&app, rig.visual).2, 1, "a new result does not refetch");
    let refresh = rig.visual.debug_bounds("properties-refresh").expect("refresh icon");
    rig.visual.simulate_click(refresh.center(), Default::default());
    wait_until(rig.visual, "the refetch", |visual| {
        app.read_with(visual, |this, _| {
            let props = &this.workspace.runtime(tab).unwrap().columns_panel.props;
            props.fetches == 2 && props.status != LoadStatus::Loading
        })
    });
    assert_eq!(properties_snapshot(&app, rig.visual).1, LoadStatus::Loaded);
}

#[gpui::test]
fn a_driver_without_table_properties_shows_the_empty_state_and_is_not_called(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table("items");
    let (key, tab) = (rig.key.clone(), rig.tab);
    // The cached capabilities of this session lack `metadata.table_properties`.
    rig.app.update(rig.visual, |this, _| {
        let real = this.driver_runtime.capabilities[&key].clone();
        let features = real
            .features()
            .iter()
            .filter(|feature| feature.as_str() != "metadata.table_properties")
            .cloned()
            .collect();
        this.driver_runtime.capabilities.insert(
            key.clone(),
            std::sync::Arc::new(engine_driver_api::DriverCapabilities::new(
                real.driver.clone(),
                real.display_name.clone(),
                features,
            )),
        );
    });
    // Were the driver asked, the dropped session would answer with an error.
    rig.session.lock().unwrap().disconnect().expect("the driver disconnects");

    properties_open(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the properties fetch", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().columns_panel.props.status != LoadStatus::Loading
        })
    });
    settle(rig.visual);
    let (active, status, _, visible) = properties_snapshot(&app, rig.visual);
    assert_eq!(status, LoadStatus::Loaded);
    assert_eq!(active, PropertiesTab::Columns);
    assert_eq!(visible, [PropertiesTab::Columns], "no list is reported");
    assert!(rig.visual.debug_bounds("properties-error").is_none());
    assert!(rig.visual.debug_bounds("columns-view-row").is_some(), "Columns still renders");
}

// --- RowFields cache ---------------------------------------------------------

#[test]
fn row_fields_cache_requests_each_selection_once_and_drops_old_pages() {
    use crate::mongo_result::RowFieldsCache;
    let fields = |page_id, row_index| engine_driver_api::RowFields {
        page_id,
        row_index,
        fields: Vec::new(),
        mutation_identity: serde_json::json!({ "_id": row_index }),
    };
    let mut cache = RowFieldsCache::default();
    assert!(cache.wants(7, 0, 9), "a new selection is fetched");
    cache.mark_requested(7, 0, 9);
    assert!(!cache.wants(7, 0, 9), "the same selection is not fetched again");
    assert!(cache.wants(7, 0, 10), "a changed selection is");
    cache.mark_requested(7, 0, 10);
    assert_eq!(cache.missing(7, 0..4), [0, 1, 2, 3]);

    cache.insert(7, (0..4).map(|row| (row, fields(7, row))));
    assert!(cache.missing(7, 0..4).is_empty());
    assert_eq!(cache.missing(7, 2..6), [4, 5]);
    let first = cache.get(7, 2).expect("cached").clone();
    assert_eq!(first.mutation_identity, serde_json::json!({ "_id": 2 }));
    assert!(std::sync::Arc::ptr_eq(&first, cache.get(7, 2).unwrap()), "hits share one allocation");
    assert!(cache.get(8, 2).is_none(), "another page never hits");

    // A new page drops the old page's rows.
    cache.insert(8, [(0, fields(8, 0))]);
    assert_eq!(cache.len(), 1);
    assert!(cache.get(7, 2).is_none());

    // The cache is bounded: the oldest rows go first.
    let total = RowFieldsCache::CAPACITY as u64 + 10;
    cache.insert(9, (0..total).map(|row| (row, fields(9, row))));
    assert_eq!(cache.len(), RowFieldsCache::CAPACITY);
    assert!(cache.get(9, 0).is_none() && cache.get(9, total - 1).is_some());
}

/// A Mongo find tab (the mock driver's page) on a real host session.
fn find_tab_on_real_session(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<AppRoot>,
    &mut gpui::VisualTestContext,
    u64,
    std::sync::Arc<engine_driver_host::SharedDriverSession>,
) {
    let (app, visual, tab, session) =
        script_tab_on_real_session(cx, &["navigator", "query.find", "query.mongo_script"]);
    visual.dispatcher.allow_parking();
    click_debug_button(visual, "run-query");
    wait_until(visual, "the run to finish", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().driver_page_id.is_some()
        })
    });
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab).unwrap().query_mode = Some(QueryMode::MongoFind);
        this.av().grid_ui.selection.borrow_mut().select(CellPosition { row: 0, column: 0 });
    });
    (app, visual, tab, session)
}

#[gpui::test]
fn edit_and_detail_readers_use_the_selection_cache_never_the_driver_or_its_lock(
    cx: &mut TestAppContext,
) {
    let (app, visual, tab, session) = find_tab_on_real_session(cx);
    let holder = SessionLockHolder::hold(&session);

    // Before the background read lands the reader answers at once, "loading".
    let started = std::time::Instant::now();
    let early = app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0));
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    assert!(early.is_err_and(|error| error.contains("still loading")));

    // The frame notices the selection and fills the cache in the background,
    // while the session lock is held elsewhere.
    wait_until(visual, "the selection's row fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).is_ok())
    });
    assert!(session.try_lock().is_err(), "the session was busy throughout");
    let (first, again) = app.read_with(visual, |this, _| {
        (this.mongo_row_fields(tab, 0).unwrap(), this.mongo_row_fields(tab, 0).unwrap())
    });
    assert!(std::sync::Arc::ptr_eq(&first, &again), "a hit is a shared Arc, not a copy");
    assert_eq!(first.mutation_identity, serde_json::json!({"_id": {"$oid": "1"}}));

    // Frames with the selection unchanged do not read again.
    for _ in 0..5 {
        settle(visual);
    }
    let after = app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).unwrap());
    assert!(std::sync::Arc::ptr_eq(&first, &after));
    holder.release();

    // The editors, detail builders and the delete dialog read the same cache.
    app.update_in(visual, |this, window, cx| {
        let target = this.workspace.tab(tab).unwrap().clone();
        assert!(this.build_cell_detail(&target, CellPosition { row: 0, column: 0 }).is_some());
        this.begin_inline_edit(target.id, 0, 0, window, cx);
    });
}

#[gpui::test]
#[ignore = "allocation benchmark; run alone with --test-threads=1"]
fn a_row_fields_cache_hit_allocates_nothing(cx: &mut TestAppContext) {
    let (app, visual, tab, _session) = find_tab_on_real_session(cx);
    wait_until(visual, "the selection's row fields", |visual| {
        settle(visual);
        app.read_with(visual, |this, _| this.mongo_row_fields(tab, 0).is_ok())
    });
    app.read_with(visual, |this, _| {
        // Warm any lazily initialised state, then count.
        drop(this.mongo_row_fields(tab, 0));
        let before = crate::grid_bench::allocation_count();
        for _ in 0..10_000 {
            let hit = this.mongo_row_fields(tab, 0);
            assert!(hit.is_ok());
        }
        let allocations = crate::grid_bench::allocation_count() - before;
        assert_eq!(allocations, 0, "10 000 cache hits allocated {allocations} times");
    });
}

// --- batch edit + describe against a real server ----------------------------

const SMOKE_TABLE: &str = "tablex_w3_edit_smoke";
/// A value that exercises both escape rules: a backslash and a quote.
const TRICKY: &str = "C:\\dir\\it's";

/// Runs one confirmed statement on `session`, bypassing the UI (setup and
/// cleanup of the scratch table).
fn exec_confirmed(session: &std::sync::Arc<engine_driver_host::SharedDriverSession>, sql: &str) {
    let runner = session.query_runner().expect("a query runner");
    let request = serde_json::to_string(&engine_driver_api::QueryRequest {
        kind: "sql".into(),
        operation_id: runner.operation_id(),
        database: None,
        object: None,
        offset: 0,
        limit: 0,
        payload: serde_json::json!({ "sql": sql, "confirmed": true }),
    })
    .unwrap();
    let page = runner.open_query(&request).unwrap_or_else(|error| panic!("{sql}: {error}"));
    runner.release_page(page.page_id);
}

/// Drops the scratch table when the test ends, pass or fail.
struct DropScratchTable(std::sync::Arc<engine_driver_host::SharedDriverSession>, &'static str);

impl Drop for DropScratchTable {
    fn drop(&mut self) {
        // A failed test still holds its tabs' sessions here (this guard drops before the rig), and one of them may
        // sit in a transaction holding the table's lock: the DROP would wait for it forever and hang the whole run.
        // The scratch tables have fixed names and are dropped again before the next run creates them.
        if std::thread::panicking() {
            return;
        }
        let drop_sql = format!("DROP TABLE IF EXISTS {}", self.1);
        let runner = self.0.query_runner();
        if let Ok(runner) = runner {
            let request = serde_json::to_string(&engine_driver_api::QueryRequest {
                kind: "sql".into(),
                operation_id: runner.operation_id(),
                database: None,
                object: None,
                offset: 0,
                limit: 0,
                payload: serde_json::json!({ "sql": drop_sql, "confirmed": true }),
            })
            .unwrap();
            if let Ok(page) = runner.open_query(&request) {
                runner.release_page(page.page_id);
            }
        }
    }
}

/// Composite-key batch edit (update + insert + delete in one Apply), the
/// reload, the WHERE round trip of a backslash-and-quote value under the
/// dialect the driver advertises, and the Properties described by the driver.
fn edit_smoke(rig: &mut SqlRig, database: &str, schema: Option<&str>) {
    let _cleanup = DropScratchTable(rig.session.clone(), SMOKE_TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {SMOKE_TABLE}"));
    exec_confirmed(
        &rig.session,
        &format!("CREATE TABLE {SMOKE_TABLE} (id integer, sub integer, name text, PRIMARY KEY (id, sub))"),
    );
    exec_confirmed(
        &rig.session,
        &format!("INSERT INTO {SMOKE_TABLE} (id, sub, name) VALUES (1,1,'a'),(1,2,'b'),(2,1,'c')"),
    );
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table_in(database, schema, SMOKE_TABLE);
    rig.rerun_table("", "id, sub");
    let row = |id: &str, sub: &str, name: &str| vec![id.to_string(), sub.to_string(), name.to_string()];
    assert_eq!(rig.grid_rows(), [row("1", "1", "a"), row("1", "2", "b"), row("2", "1", "c")]);

    rig.edit_cell(1, 2, TRICKY);
    rig.stage_insert(r#"{"id":3,"sub":1,"name":"new"}"#);
    rig.delete_row(2);
    assert_eq!(rig.staged(), (1, 2));
    rig.apply();
    rig.wait_apply_finished();
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    assert_eq!(rig.staged(), (0, 0));
    rig.wait_not_running("the reload after Apply");
    assert_eq!(rig.grid_rows(), [row("1", "1", "a"), row("1", "2", TRICKY), row("3", "1", "new")]);

    // The value finds itself again through a WHERE built for the driver's
    // dialect: a doubled backslash on MySQL/MariaDB, a single one elsewhere.
    let key = rig.key.clone();
    let literal = rig.app.read_with(rig.visual, |this, _| {
        let dialect = this.sql_dialect_for_connection(&key).expect("a dialect");
        crate::sql_filter::sql_string_literal(dialect, TRICKY)
    });
    rig.rerun_table(&format!("name = {literal}"), "id, sub");
    assert_eq!(rig.grid_rows(), [row("1", "2", TRICKY)], "{literal}");

    // Properties come from `describe_object`.
    let (app, tab) = (rig.app.clone(), rig.tab);
    properties_open(rig.visual);
    wait_until(rig.visual, "the properties fetch", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().columns_panel.props.status != LoadStatus::Loading
        })
    });
    settle(rig.visual);
    assert_eq!(properties_snapshot(&app, rig.visual).1, LoadStatus::Loaded);
    properties_click_tab(rig.visual, PropertiesTab::Keys);
    let (_, keys, ..) = properties_table(&app, rig.visual, PropertiesTab::Keys);
    assert!(
        keys.iter().any(|key| key[1] == "PK" && key[2].contains("id") && key[2].contains("sub")),
        "the composite primary key: {keys:?}"
    );
}

#[gpui::test]
fn sqlite_batch_edit_smoke_with_a_composite_key(cx: &mut TestAppContext) {
    let db = SqliteDb::new("SELECT 1;");
    let mut rig = SqlRig::sqlite(cx, &db);
    edit_smoke(&mut rig, "main", None);
}

fn live_edit_smoke(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
    schema: Option<&str>,
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    // The dedicated containers' database, as the navigator would name it.
    edit_smoke(&mut rig, "tablex", schema);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_batch_edit_and_describe_smoke(cx: &mut TestAppContext) {
    live_edit_smoke(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        "TABLEX_TEST_PG_URI",
        Some("public"),
    );
}

/// The reported case on a live PostgreSQL: a table with a text primary key
/// and a WHERE / ORDER BY, opened the way a restored session does (no
/// sidebar), edited by a real double-click. A table without a key says why.
#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_double_click_edits_a_restored_text_keyed_table(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    const KEYED: &str = "tablex_de_text_keys";
    const KEYLESS: &str = "tablex_de_no_key";
    let _keyed = DropScratchTable(rig.session.clone(), KEYED);
    let _keyless = DropScratchTable(rig.session.clone(), KEYLESS);
    for table in [KEYED, KEYLESS] {
        exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {table}"));
    }
    exec_confirmed(
        &rig.session,
        &format!("CREATE TABLE {KEYED} (id text PRIMARY KEY, trans_id text, status text, created timestamptz DEFAULT now())"),
    );
    exec_confirmed(
        &rig.session,
        &format!(
            "INSERT INTO {KEYED} (id, trans_id, status) \
             SELECT g::text, 't' || g, 'active' FROM generate_series(11, 30) g"
        ),
    );
    exec_confirmed(&rig.session, &format!("CREATE TABLE {KEYLESS} (id text, trans_id text)"));
    exec_confirmed(&rig.session, &format!("INSERT INTO {KEYLESS} VALUES ('1','a'),('2','b')"));
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));

    rig.open_restored_table(("tablex", "public"), KEYED, "id > '10' AND status = 'active'", "created DESC");
    assert_eq!(rig.rows(), 20);
    rig.wait_structure();
    rig.double_click_cell(3, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 3, column: 1 }));
    assert_eq!(rig.detail_error(), None);

    rig.open_restored_table(("tablex", "public"), KEYLESS, "", "");
    rig.wait_structure();
    rig.double_click_cell(0, 1);
    assert_eq!(rig.editing(), None);
    assert_eq!(rig.detail_error().as_deref(), Some("A single primary key is required"));
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_batch_edit_and_describe_smoke(cx: &mut TestAppContext) {
    live_edit_smoke(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", None);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_batch_edit_and_describe_smoke(cx: &mut TestAppContext) {
    live_edit_smoke(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI", None);
}

// --- lock-free mutate / describe -------------------------------------------------
//
// A batch Apply, create-database, Properties and the diagram fetch run through
// `SharedDriverSession::{mutate, describe_object}`, which never take the
// per-connection session lock: a slow write must not queue the navigator, and
// a session busy elsewhere must not delay them.

/// Every row update runs for far longer than any test waits, unless the driver
/// is cancelled.
const SLOW_UPDATE_TRIGGER: &str = "CREATE TRIGGER items_slow AFTER UPDATE ON items BEGIN \
     SELECT count(*) FROM (WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c \
     WHERE x < 4000000000) SELECT x FROM c); END;";

#[gpui::test]
fn a_slow_apply_does_not_block_the_navigator_on_the_same_connection(cx: &mut TestAppContext) {
    let db = SqliteDb::new(&format!("{EDIT_ITEMS_SEED} {SLOW_UPDATE_TRIGGER}"));
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    rig.apply();
    settle(rig.visual);
    std::thread::sleep(std::time::Duration::from_millis(200));
    let tab = rig.tab;
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().sql_apply_running, "the batch is still running");
        assert!(this.driver_runtime.query_operations.contains_key(&tab));
    });
    assert!(rig.handle.active_operation_count() >= 1, "the driver holds the batch's operation");

    // A navigator call takes the session lock while the batch is on the driver.
    let started = std::time::Instant::now();
    let nodes = rig
        .session
        .lock()
        .expect("the session lock is free")
        .list_nav_nodes(
            &serde_json::json!({"parent": null, "database": null, "payload": null, "cursor": null, "limit": 0})
                .to_string(),
        )
        .expect("the navigator answers while the batch runs");
    assert!(!nodes.nodes.is_empty());
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.workspace.runtime(tab).unwrap().sql_apply_running, "the batch ran throughout");
    });
    assert!(rig.session.try_lock().is_ok());

    // The tab's Cancel still reaches the lock-free batch by its operation id.
    rig.click("discard-edit");
    rig.wait_apply_finished_or_error();
    rig.wait_driver_idle();
    assert_eq!(db.sql(ITEMS_DUMP), "1:a:1|2:b:2|3:c:3", "the cancelled batch changed nothing");
    assert_eq!(rig.staged(), (1, 0), "the staged change is kept");
}

#[gpui::test]
fn apply_completes_while_the_session_lock_is_held_elsewhere(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.edit_cell(0, 1, "alpha");
    let holder = SessionLockHolder::hold(&rig.session);
    rig.apply();
    rig.wait_apply_finished();
    assert!(rig.session.try_lock().is_err(), "the session was busy throughout");
    assert_eq!(db.sql(ITEMS_DUMP), "1:alpha:1|2:b:2|3:c:3", "the batch landed without the lock");
    assert_eq!(rig.staged(), (0, 0));
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    holder.release();
}

impl SqlRig<'_> {
    /// Waits until the Apply is no longer running (it finished or failed).
    fn wait_apply_finished_or_error(&mut self) {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, "the Apply to end", |visual| {
            app.read_with(visual, |this, _| !this.workspace.runtime(tab).unwrap().sql_apply_running)
        });
        settle(self.visual);
    }
}

#[gpui::test]
fn properties_describe_completes_while_the_session_lock_is_held_elsewhere(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table("items");
    let (app, tab) = (rig.app.clone(), rig.tab);
    let holder = SessionLockHolder::hold(&rig.session);
    properties_open(rig.visual);
    wait_until(rig.visual, "the properties fetch", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().columns_panel.props.status != LoadStatus::Loading
        })
    });
    assert_eq!(properties_snapshot(&app, rig.visual).1, LoadStatus::Loaded);
    assert!(rig.session.try_lock().is_err(), "the session was busy throughout");
    holder.release();
}

#[gpui::test]
fn the_driver_diagram_fetch_completes_while_the_session_lock_is_held_elsewhere(
    cx: &mut TestAppContext,
) {
    let db = SqliteDb::new(DIAGRAM_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let session = rig.session.clone();
    let visual = rig.visual;
    open_album_diagram_tab(&app, visual, tab);
    let holder = SessionLockHolder::hold(&session);
    app.update(visual, |this, cx| this.request_diagram(tab, cx));
    wait_until(visual, "the driver diagram", |visual| {
        app.read_with(visual, |this, _| {
            this.diagram_graph(tab).is_some_and(|snapshot| {
                snapshot.graph.entities.len() == 3
                    && snapshot.graph.entities.iter().all(|entity| entity.state == EntityState::Ready)
            })
        })
    });
    assert!(session.try_lock().is_err(), "the session was busy throughout");
    holder.release();
}

