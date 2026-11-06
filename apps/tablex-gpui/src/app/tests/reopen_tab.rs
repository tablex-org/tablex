// Reopen Closed Tab (Cmd+Shift+T, the palette): closed tabs come back newest first with their text,
// data tabs reload, a bounded history, tabs of a deleted connection are skipped, and an empty
// history does nothing. Driven by real keystrokes on the production theme and `Root`.

fn ro_chord() -> String {
    sql_chord("workspace.reopen_closed_tab")
}

/// Keyboard focus on the app root, as it is after a tab went away: the chord is an app action.
fn ro_focus_root(rig: &mut ExplainRig<'_>) {
    rig.visual.update(|window, cx| {
        rig.app.update(cx, |this, cx| this.sidebar.focus.focus(window, cx));
    });
    settle(rig.visual);
}

fn ro_press(rig: &mut ExplainRig<'_>, keys: &str) {
    ro_focus_root(rig);
    rig.visual.simulate_keystrokes(keys);
    settle(rig.visual);
}

fn ro_titles(rig: &ExplainRig<'_>) -> Vec<String> {
    rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().map(|tab| tab.title.clone()).collect())
}

fn ro_active(rig: &ExplainRig<'_>) -> (u64, String, String) {
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().expect("an active tab");
        (tab.id, tab.title.clone(), tab.sql.clone())
    })
}

fn ro_closed(rig: &ExplainRig<'_>) -> usize {
    rig.app.read_with(rig.visual, |this, _| this.shell.closed_tabs.len())
}

fn ro_open(rig: &mut ExplainRig<'_>, title: &str, connection: &str) -> u64 {
    let (title, connection) = (title.to_owned(), connection.to_owned());
    let tab = rig.app.update(rig.visual, |this, cx| {
        let tab = this
            .workspace
            .open_query_tab_with_metadata(title, TabMetadata { connection_id: connection, ..Default::default() });
        cx.notify();
        tab
    });
    settle(rig.visual);
    tab
}

#[gpui::test]
fn the_chord_brings_back_a_closed_sql_tab_with_the_text_typed_in_it(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let other = rig.tab(ConnectionKind::Postgres, "");
    let tab = rig.tab(ConnectionKind::Postgres, "");
    sql_select(&mut rig, tab, 0..0);
    rig.visual.simulate_input("SELECT 42 -- typed, never saved");
    settle(rig.visual);
    assert_eq!(sql_tab_text(&rig, tab), "SELECT 42 -- typed, never saved");
    let before = ro_titles(&rig).len();

    rig.visual.simulate_keystrokes(&sql_primary("w"));
    settle(rig.visual);
    assert_eq!(ro_titles(&rig).len(), before - 1, "the tab closed");
    assert_eq!(ro_active(&rig).0, other, "its neighbour took over");

    ro_press(&mut rig, &ro_chord());
    let (reopened, title, sql) = ro_active(&rig);
    assert_ne!(reopened, other);
    assert_eq!(title, "SQL");
    assert_eq!(sql, "SELECT 42 -- typed, never saved", "the text is back as it was");
    assert_eq!(ro_titles(&rig).len(), before, "one tab again");
    assert_eq!(sql_text(&rig, reopened), "SELECT 42 -- typed, never saved", "and so is the editor");
    assert_eq!(ro_closed(&rig), 0);
}

#[gpui::test]
fn repeated_presses_walk_back_through_the_closed_tabs(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let first = rig.tab(ConnectionKind::Postgres, "select 1");
    let second = rig.tab(ConnectionKind::Postgres, "select 2");
    for (tab, title) in [(first, "First"), (second, "Second")] {
        rig.app.update(rig.visual, |this, cx| {
            this.workspace.rename_tab(tab, title);
            this.activate_tab_ui(tab, cx);
        });
        settle(rig.visual);
        ro_press(&mut rig, &sql_primary("w"));
    }
    assert!(!ro_titles(&rig).iter().any(|title| title == "First" || title == "Second"));

    ro_press(&mut rig, &ro_chord());
    let (_, title, sql) = ro_active(&rig);
    assert_eq!((title.as_str(), sql.as_str()), ("Second", "select 2"), "the last closed comes first");
    ro_press(&mut rig, &ro_chord());
    let (_, title, sql) = ro_active(&rig);
    assert_eq!((title.as_str(), sql.as_str()), ("First", "select 1"), "then the one before");
}

#[gpui::test]
fn closing_others_and_closing_all_are_remembered_too(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tabs: Vec<u64> = (0..3).map(|n| ro_open(&mut rig, &format!("T{n}"), "")).collect();
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&tabs, None, cx));
    settle(rig.visual);
    assert_eq!(ro_closed(&rig), 3);
    for title in ["T2", "T1", "T0"] {
        ro_press(&mut rig, &ro_chord());
        assert_eq!(ro_active(&rig).1, title);
    }
}

#[gpui::test]
fn only_twenty_closed_tabs_are_kept(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tabs: Vec<u64> = (0..25).map(|n| ro_open(&mut rig, &format!("T{n}"), "")).collect();
    rig.app.update(rig.visual, |this, cx| this.close_tabs_now(&tabs, None, cx));
    settle(rig.visual);
    assert_eq!(ro_closed(&rig), 20, "the oldest five were dropped");
    for n in (5..25).rev() {
        ro_press(&mut rig, &ro_chord());
        assert_eq!(ro_active(&rig).1, format!("T{n}"));
    }
    let open = ro_titles(&rig);
    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_titles(&rig), open, "T0 to T4 are gone for good");
}

#[gpui::test]
fn a_tab_of_a_deleted_connection_is_skipped(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let kept = rig.tab(ConnectionKind::Postgres, "select 'kept'");
    let gone = rig.tab(ConnectionKind::Mysql, "select 'gone'");
    for tab in [kept, gone] {
        rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(tab, cx));
        settle(rig.visual);
        ro_press(&mut rig, &sql_primary("w"));
    }
    // The newest closed tab's connection is deleted.
    rig.app.update(rig.visual, |this, _| {
        this.saved_connections.pop();
    });

    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_active(&rig).2, "select 'kept'", "the tab of the deleted connection was passed over");
    assert_eq!(ro_closed(&rig), 0, "and dropped");
    let open = ro_titles(&rig);
    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_titles(&rig), open);
}

#[gpui::test]
fn the_palette_entry_reopens_the_last_closed_tab(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let tab = rig.tab(ConnectionKind::Postgres, "select 'from the palette'");
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.rename_tab(tab, "Palette tab");
        cx.notify();
    });
    ro_press(&mut rig, &sql_primary("w"));
    assert!(!ro_titles(&rig).iter().any(|title| title == "Palette tab"));

    palette_open(rig.visual);
    palette_type(rig.visual, "reopen closed");
    rig.visual.simulate_keystrokes("enter");
    settle(rig.visual);
    let (_, title, sql) = ro_active(&rig);
    assert_eq!((title.as_str(), sql.as_str()), ("Palette tab", "select 'from the palette'"));
    assert!(!palette_is_open(rig.visual), "the palette closed");
}

#[gpui::test]
fn the_chord_with_nothing_closed_does_nothing(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "select 1");
    let (open, active) = (ro_titles(&rig), ro_active(&rig));
    let errors = rig.app.read_with(rig.visual, |this, _| this.error_log.log.len());

    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_titles(&rig), open);
    assert_eq!(ro_active(&rig), active);
    assert!(!rig.visual.update(|window, cx| window.has_active_dialog(cx)), "no dialog");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.error_log.log.len()), errors, "no error");
}

#[gpui::test]
fn a_table_tab_reloads_its_rows_and_a_file_tab_keeps_its_unsaved_text(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    let connection = rig.app.read_with(rig.visual, |this, _| this.saved_connections.len());
    rig.tab(ConnectionKind::Postgres, "");
    let connection_id = rig.app.read_with(rig.visual, |this, _| this.saved_connections[connection].id.to_string());
    let table = rig.app.update(rig.visual, |this, cx| {
        let tab = this.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: connection_id.clone(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::SqlTable),
                ..Default::default()
            },
        );
        this.workspace.tab_mut(tab).unwrap().applied_where = "id > 3".into();
        this.workspace.tab_mut(tab).unwrap().table_where = "id > 3".into();
        cx.notify();
        tab
    });
    let file = rig.app.update(rig.visual, |this, cx| {
        let tab = this
            .workspace
            .open_query_tab_with_metadata("report.sql", TabMetadata { connection_id: connection_id.clone(), ..Default::default() });
        let entry = this.workspace.tab_mut(tab).unwrap();
        entry.sql = "select edited".into();
        entry.file = Some(crate::workspace::SqlFile { path: "/tmp/report.sql".into(), saved: "select on_disk".into() });
        cx.notify();
        tab
    });
    settle(rig.visual);
    rig.app.update(rig.visual, |this, cx| {
        this.close_tab(file, cx);
        this.close_tab(table, cx);
    });

    ro_press(&mut rig, &ro_chord());
    let (reopened, title, _) = ro_active(&rig);
    assert_eq!(title, "orders");
    rig.wait_failed(reopened);
    let sql = rig.failed_sql(reopened).expect("the table was read again");
    assert!(sql.contains("orders") && sql.contains("id > 3"), "{sql}");

    ro_press(&mut rig, &ro_chord());
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.title, "report.sql");
        assert_eq!(tab.sql, "select edited");
        assert!(tab.has_unsaved_file(), "still unsaved against the file");
    });
}

#[gpui::test]
fn running_jobs_and_utility_tabs_are_not_remembered(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "");
    let id = rig.app.read_with(rig.visual, |this, _| this.saved_connections.last().unwrap().id.to_string());
    let tabs = rig.app.update(rig.visual, |this, _| {
        vec![
            this.workspace.open_import_tab(&id, "db", Some("public"), "Import"),
            this.workspace.open_transfer_tab(&id, "db", Some("public"), "Transfer"),
            this.workspace.open_file_run_tab(&id, "db", Some("public"), "Run file"),
            this.workspace.open_backup_tab(false, &id, "db", Some("public"), "Backup"),
            this.workspace.open_backup_tab(true, &id, "db", Some("public"), "Restore"),
            this.workspace.open_accounts_tab(&id, "Users"),
            this.workspace.open_process_list_tab(&id, "Process List"),
            this.workspace.open_driver_manager_tab(),
            this.workspace.open_settings_tab(),
        ]
    });
    rig.app.update(rig.visual, |this, cx| {
        for tab in tabs {
            this.close_tab(tab, cx);
        }
    });
    assert_eq!(ro_closed(&rig), 0);
}

/// The chord after the app closed tabs on its own: nothing comes back, and nothing was remembered.
fn ro_nothing_reopens(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    let titles = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<String> {
        app.read_with(visual, |this, _| this.workspace.tabs().iter().map(|tab| tab.title.clone()).collect())
    };
    assert_eq!(app.read_with(visual, |this, _| this.shell.closed_tabs.len()), 0, "the app's own closes are not remembered");
    let open = titles(app, visual);
    // What the chord runs (the chord itself is driven by the tests above).
    app.update(visual, |this, cx| this.reopen_closed_tab(cx));
    settle(visual);
    assert_eq!(titles(app, visual), open, "reopening brings nothing back");
}

/// Opens the table-data tab `name` of a Postgres connection (what the navigator's double click does).
fn ro_table_tab(rig: &mut ExplainRig<'_>, connection: &str, name: &str) -> u64 {
    let (connection, name) = (connection.to_owned(), name.to_owned());
    let tab = rig.app.update(rig.visual, |this, cx| {
        let (tab, _) = this.open_sql_table_tab(&name, &connection, "warehouse", "public", &name);
        cx.notify();
        tab
    });
    settle(rig.visual);
    tab
}

#[gpui::test]
fn a_closed_file_tab_whose_file_is_open_in_another_tab_shows_that_tab_and_opens_no_second_one(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "");
    let connection = rig.app.read_with(rig.visual, |this, _| this.saved_connections.last().unwrap().id.to_string());
    let file_tab = |rig: &mut ExplainRig<'_>, text: &str| {
        let text = text.to_owned();
        let connection = connection.clone();
        rig.app.update(rig.visual, |this, cx| {
            let tab = this.workspace.open_query_tab_with_metadata("report.sql", TabMetadata { connection_id: connection, ..Default::default() });
            let entry = this.workspace.tab_mut(tab).unwrap();
            entry.sql = text;
            entry.file = Some(crate::workspace::SqlFile { path: "/tmp/one-tab-per-file.sql".into(), saved: "select on_disk".into() });
            cx.notify();
            tab
        })
    };
    let closed = file_tab(&mut rig, "select first");
    rig.app.update(rig.visual, |this, cx| this.close_tab(closed, cx));
    // The same file is opened again, in a tab of its own, and another tab is in front.
    let open = file_tab(&mut rig, "select second");
    let other = rig.tab(ConnectionKind::Postgres, "select 'other'");
    assert_eq!(ro_active(&rig).0, other);
    let tabs = ro_titles(&rig).len();

    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_active(&rig), (open, "report.sql".to_owned(), "select second".to_owned()), "the tab that has the file is shown");
    assert_eq!(ro_titles(&rig).len(), tabs, "no second tab on the file");
    assert_eq!(ro_closed(&rig), 0, "and the closed one is spent");
}

#[gpui::test]
fn a_closed_welcome_or_table_tab_that_is_open_again_shows_the_open_one(cx: &mut TestAppContext) {
    let mut rig = ExplainRig::new(cx);
    rig.tab(ConnectionKind::Postgres, "");
    let connection = rig.app.read_with(rig.visual, |this, _| this.saved_connections.last().unwrap().id.to_string());

    // A singleton Welcome.
    rig.app.update(rig.visual, |this, cx| {
        let welcomes: Vec<u64> = this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Welcome).map(|tab| tab.id).collect();
        for welcome in welcomes {
            this.close_tab(welcome, cx);
        }
    });
    let welcome = rig.app.update(rig.visual, |this, _| this.workspace.open_welcome_tab());
    let other = rig.tab(ConnectionKind::Postgres, "select 'other'");
    assert_eq!(ro_active(&rig).0, other);
    let tabs = ro_titles(&rig).len();
    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_active(&rig).0, welcome, "the open Welcome is shown");
    assert_eq!(ro_titles(&rig).len(), tabs, "not a second Welcome");
    assert_eq!(rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Welcome).count()), 1);

    // A table tab: same connection, database, schema and object.
    let first = ro_table_tab(&mut rig, &connection, "orders");
    rig.app.update(rig.visual, |this, cx| this.close_tab(first, cx));
    let again = ro_table_tab(&mut rig, &connection, "orders");
    rig.app.update(rig.visual, |this, cx| this.activate_tab_ui(other, cx));
    settle(rig.visual);
    let tabs = ro_titles(&rig).len();
    ro_press(&mut rig, &ro_chord());
    assert_eq!(ro_active(&rig).0, again, "the open table tab is shown");
    assert_eq!(ro_titles(&rig).len(), tabs, "not a second tab on the table");
    let tables = rig.app.read_with(rig.visual, |this, _| this.workspace.tabs().iter().filter(|tab| tab.collection == "orders" && tab.query_mode.is_table_data()).count());
    assert_eq!(tables, 1);
}
