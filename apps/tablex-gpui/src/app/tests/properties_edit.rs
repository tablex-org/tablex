// Structure edit mode of the table Properties (R3-2a), driven through the real elements: the
// toggle and the toolbar buttons are clicked, rows are clicked and keys are pressed, the review
// dialog is the shared one, and Apply runs through the injected executor of `AppRoot`.

use crate::app::results::properties_edit::{PropertiesEdit, RowKind};
use crate::app::structure_edit::{ApplyRequest, ApplyTarget, ReviewHost};
use crate::sql_ddl::{ColumnSpec, PrimaryKeyRef, TableId};

/// The table the fixture's `orders` tab is described as for these tests (the fixture engine
/// describes tables without columns).
fn pe_described() -> crate::catalog::TableProperties {
    use crate::catalog::{CatalogColumn, CatalogConstraint, CatalogConstraintKind};
    let column = |name: &str, nullable: bool, primary_key: bool| CatalogColumn {
        name: name.into(),
        data_type: if primary_key { "integer" } else { "text" }.into(),
        nullable,
        primary_key,
        ..CatalogColumn::default()
    };
    crate::catalog::TableProperties {
        columns: vec![column("id", false, true), column("name", false, false), column("note", true, false), column("legacy", true, false)],
        keys: Some(vec![CatalogConstraint {
            name: "orders_pkey".into(),
            kind: CatalogConstraintKind::PrimaryKey,
            columns: vec!["id".into()],
            definition: None,
        }]),
        foreign_keys: Some(Vec::new()),
        indexes: Some(Vec::new()),
        ..crate::catalog::TableProperties::default()
    }
}

fn pe_orders() -> TableId {
    TableId { schema: Some("public".into()), name: "orders".into() }
}

fn pe_spec(name: &str, data_type: &str, nullable: bool) -> ColumnSpec {
    ColumnSpec { name: name.into(), data_type: data_type.into(), nullable, ..ColumnSpec::default() }
}

/// The Postgres fixture's `orders` tab with Properties open and a described table that has columns.
fn pe_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    properties_open(visual);
    pe_describe(&app, visual, tab_id, pe_described());
    (app, visual, tab_id)
}

/// Makes `properties` what the tab's Properties holds (a fetch result of the table).
fn pe_describe(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, properties: crate::catalog::TableProperties) {
    app.update(visual, |this, cx| {
        let props = &mut this.workspace.runtime_mut(tab_id).unwrap().columns_panel.props;
        props.data = Some(std::sync::Arc::new(properties));
        props.revision += 1;
        props.tables.clear();
        cx.notify();
    });
    settle(visual);
}

fn pe_click(visual: &mut gpui::VisualTestContext, selector: String) {
    diagram_click(visual, d3_sel(selector));
}

fn pe_edit(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<gpui::Entity<PropertiesEdit>> {
    app.read_with(visual, |this, _| this.properties_edits.get(&tab_id).cloned())
}

fn pe_changes(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Vec<Change> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.draft_changes().to_vec())
}

/// Position of column `name` in the drawn list.
fn pe_position(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, name: &str) -> usize {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.position_of(name)).unwrap_or_else(|| panic!("{name} is listed"))
}

fn pe_kind(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, name: &str) -> RowKind {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.rows().iter().find(|row| row.spec.name == name).map(|row| row.kind)).unwrap_or_else(|| panic!("{name} is a row"))
}

fn pe_pending(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> usize {
    pe_edit(app, visual, tab_id).map_or(0, |edit| edit.read_with(visual, |edit, _| edit.pending_count()))
}

/// Edit mode on, and `legacy` / `note` ... reachable by name.
fn pe_start(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) {
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(app, visual, tab_id).is_some(), "the edit mode started");
}

fn pe_select(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, name: &str) {
    let position = pe_position(app, visual, tab_id, name);
    // On the marker cell: a click on a text cell opens its editor.
    let row = visual.debug_bounds(d3_sel(format!("properties-edit-row-{position}"))).expect("the row is drawn");
    visual.simulate_click(row.origin + gpui::point(gpui::px(14.0), gpui::px(13.0)), Default::default());
    settle(visual);
}

type PeLog = std::sync::Arc<std::sync::Mutex<Vec<String>>>;
type PeFail = std::sync::Arc<std::sync::Mutex<Option<&'static str>>>;

/// An executor that logs every statement; `fail` makes the statements containing it fail.
fn pe_inject(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> (PeLog, PeFail) {
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let fail = std::sync::Arc::new(std::sync::Mutex::new(None::<&'static str>));
    let (log, failing) = (ran.clone(), fail.clone());
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, _: &crate::app::structure_edit::apply::ApplyControl| {
            if failing.lock().unwrap().is_some_and(|needle| sql.contains(needle)) {
                return Err(format!("boom: {sql}"));
            }
            log.lock().unwrap().push(sql.to_owned());
            Ok(())
        }));
    });
    (ran, fail)
}

const PE_CONTROLS: [&str; 5] =
    ["properties-edit-changes", "properties-edit-undo", "properties-edit-redo", "properties-edit-discard-all", "properties-edit-add-column"];

#[gpui::test]
fn edit_structure_toggles_the_edit_mode_and_its_controls(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    assert!(visual.debug_bounds("properties-edit-toggle").is_some(), "the toggle is on the Columns toolbar");
    for selector in PE_CONTROLS {
        assert!(visual.debug_bounds(selector).is_none(), "{selector} only exists while editing");
    }
    assert!(visual.debug_bounds("columns-view-row").is_some(), "the plain grid");
    assert!(pe_edit(&app, visual, tab_id).is_none());

    pe_start(&app, visual, tab_id);
    for selector in PE_CONTROLS {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is shown");
    }
    assert!(visual.debug_bounds("columns-view-row").is_none(), "the edit rows replace the plain ones");
    for position in 0..4 {
        assert!(visual.debug_bounds(d3_sel(format!("properties-edit-row-{position}"))).is_some(), "row {position}");
    }
    assert!(visual.debug_bounds("properties-edit-row-4").is_none(), "four columns, four rows");
    // The markers are empty for untouched columns; Undo / Redo / Discard all have nothing to do.
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert!(edit.read_with(visual, |edit, _| edit.rows().iter().all(|row| row.kind == RowKind::Existing)));

    // The Keys sub-tab has the toggle and the review controls, not Add column.
    properties_click_tab(visual, PropertiesTab::Keys);
    for selector in ["properties-edit-toggle", "properties-edit-changes", "properties-edit-undo", "properties-edit-redo", "properties-edit-discard-all"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} on Keys");
    }
    assert!(visual.debug_bounds("properties-edit-add-column").is_none());
    properties_click_tab(visual, PropertiesTab::Columns);

    // Without pending changes the toggle leaves the mode.
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none());
    assert!(visual.debug_bounds("properties-edit-changes").is_none());
    assert!(visual.debug_bounds("columns-view-row").is_some());

    // With pending changes it stays (leaving would lose them): the toggle is disabled.
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_some(), "the pending change keeps the edit mode");
    diagram_click(visual, "properties-edit-discard-all");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none(), "nothing pending: it leaves");
}

#[gpui::test]
fn edit_structure_is_off_with_its_reason_on_a_read_only_connection(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    publish_types(&app, visual, true);
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    let gate = app.read_with(visual, |this, _| this.properties_edit_gate(tab_id));
    let reason = gate.expect("a table tab").expect_err("read-only: no editing");
    assert!(reason.contains("read-only"), "{reason}");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none(), "the disabled toggle starts nothing");

    // The reason lifts with the profile.
    give_profile(&app, visual, &connection, ConnectionProfile::default());
    assert!(app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).unwrap().is_ok());

    // An edit mode that is open when the connection turns read-only is frozen, not applied.
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert!(!edit.read_with(visual, |edit, _| edit.editable()));
    diagram_click(visual, "properties-edit-add-column");
    assert_eq!(pe_pending(&app, visual, tab_id), 1, "Add column is disabled");
    let position = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-nullable-{position}"));
    assert_eq!(pe_pending(&app, visual, tab_id), 1, "the checkboxes are disabled");
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-disabled").is_some(), "the dialog says why Apply is off");
    diagram_click(visual, "changes-close");
}

#[gpui::test]
fn edit_structure_is_off_for_an_in_memory_sqlite_database(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::SQLITE_ID.into();
        let saved = this.saved_connections.iter_mut().find(|saved| saved.matches_id(crate::ui_fixture::SQLITE_ID)).expect("the fixture SQLite connection");
        saved.uri = "sqlite::memory:".into();
    });
    settle(visual);
    let reason = app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).unwrap().expect_err("in-memory: no editing");
    assert!(reason.contains("in-memory SQLite"), "{reason}");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none());
}

#[gpui::test]
fn edit_structure_is_off_for_a_driver_without_table_properties(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    app.update(visual, |this, _| {
        this.driver_runtime
            .capabilities
            .insert(connection.clone(), std::sync::Arc::new(engine_driver_api::DriverCapabilities::new("postgres", "PostgreSQL", Vec::new())));
    });
    settle(visual);
    let reason = app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).unwrap().expect_err("no table properties");
    assert!(reason.contains("metadata.table_properties"), "{reason}");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none());
}

#[gpui::test]
fn delete_drops_a_column_the_row_shows_it_and_restore_takes_it_back(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::DropColumn { table: pe_orders(), column: "legacy".into() }]);
    assert_eq!(pe_kind(&app, visual, tab_id, "legacy"), RowKind::Dropped);
    let position = pe_position(&app, visual, tab_id, "legacy");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-marker-{position}"))).is_some(), "the marker cell");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-restore-{position}"))).is_some(), "Restore replaces Drop");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-drop-{position}"))).is_none());
    open_review(visual, "properties-edit-changes");
    diagram_click(visual, "changes-close");

    // Restore removes the change from the draft, and the row is a plain one again.
    pe_click(visual, format!("properties-edit-restore-{position}"));
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    assert_eq!(pe_kind(&app, visual, tab_id, "legacy"), RowKind::Existing);

    // Delete on the selected column toggles: drop, then restore.
    pe_select(&app, visual, tab_id, "note");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped);
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Existing);
    // The row button does the same as the key.
    let note = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-drop-{note}"));
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped);
    // The arrow keys move the selection among the rows.
    visual.simulate_keystrokes("up");
    settle(visual);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.selected().map(str::to_owned)).as_deref(), Some("name"));
}

#[gpui::test]
fn the_nullable_and_key_checkboxes_edit_the_draft(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let note = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-nullable-{note}"));
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [Change::AlterColumn { table: pe_orders(), from: pe_spec("note", "text", true), to: pe_spec("note", "text", false) }]
    );
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Changed);
    // Ticking it again folds the alteration away.
    pe_click(visual, format!("properties-edit-nullable-{note}"));
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Existing);
    // A key column cannot be made nullable.
    let id = pe_position(&app, visual, tab_id, "id");
    pe_click(visual, format!("properties-edit-nullable-{id}"));
    assert!(pe_changes(&app, visual, tab_id).is_empty());

    // The key: `previous` is the key the catalog reported.
    let name = pe_position(&app, visual, tab_id, "name");
    pe_click(visual, format!("properties-edit-key-{name}"));
    let previous = Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] });
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [Change::SetPrimaryKey { table: pe_orders(), columns: vec!["id".into(), "name".into()], previous: previous.clone() }]
    );
    assert_eq!(pe_kind(&app, visual, tab_id, "name"), RowKind::Changed);
    pe_click(visual, format!("properties-edit-key-{name}"));
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "back to the catalog's key: nothing to change");
    pe_click(visual, format!("properties-edit-key-{id}"));
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::SetPrimaryKey { table: pe_orders(), columns: Vec::new(), previous }]);
}

fn pe_selected(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<String> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.selected().map(str::to_owned))
}

/// Space on the focused control: the official controls act on key up.
fn pe_space(visual: &mut gpui::VisualTestContext) {
    let keystroke = gpui::Keystroke::parse("space").unwrap();
    visual.simulate_event(gpui::KeyDownEvent { keystroke: keystroke.clone(), is_held: false, prefer_character_input: false });
    visual.simulate_event(gpui::KeyUpEvent { keystroke });
    settle(visual);
}

/// The tab stops of the window in Tab order, starting after whatever is focused now.
fn pe_tab_stops(visual: &mut gpui::VisualTestContext) -> Vec<String> {
    let mut stops: Vec<String> = Vec::new();
    for _ in 0..200 {
        visual.update(|window, cx| window.focus_next(cx));
        settle(visual);
        let stop = visual.update(|window, cx| window.focused(cx).map(|handle| format!("{handle:?}"))).unwrap_or_default();
        if stops.contains(&stop) {
            break;
        }
        stops.push(stop);
    }
    stops
}

/// Tabs to the stop `target` (one of `pe_tab_stops`).
fn pe_focus(visual: &mut gpui::VisualTestContext, target: &str) {
    for _ in 0..200 {
        visual.update(|window, cx| window.focus_next(cx));
        settle(visual);
        if visual.update(|window, cx| window.focused(cx).map(|handle| format!("{handle:?}"))).as_deref() == Some(target) {
            return;
        }
    }
    panic!("{target} is a tab stop");
}

/// Hover over `selector` long enough for its tooltip, and say whether one is shown.
fn pe_tooltip_shown(visual: &mut gpui::VisualTestContext, selector: String) -> bool {
    let bounds = visual.debug_bounds(d3_sel(selector)).expect("the control is drawn");
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.executor().advance_clock(std::time::Duration::from_millis(700));
    settle(visual);
    visual.debug_bounds("properties-edit-tooltip").is_some()
}

#[gpui::test]
fn the_boxes_and_actions_of_rows_without_selection_act_on_a_single_click(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "name");
    let (note, legacy, id) = (pe_position(&app, visual, tab_id, "note"), pe_position(&app, visual, tab_id, "legacy"), pe_position(&app, visual, tab_id, "id"));

    // Nullable of a row that is not selected: one click toggles it and selects the row.
    pe_click(visual, format!("properties-edit-nullable-{note}"));
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [Change::AlterColumn { table: pe_orders(), from: pe_spec("note", "text", true), to: pe_spec("note", "text", false) }]
    );
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("note"));

    // Key of another row that is not selected.
    pe_click(visual, format!("properties-edit-key-{legacy}"));
    let previous = Some(PrimaryKeyRef { name: Some("orders_pkey".into()), columns: vec!["id".into()] });
    assert_eq!(
        pe_changes(&app, visual, tab_id)[1..],
        [Change::SetPrimaryKey { table: pe_orders(), columns: vec!["id".into(), "legacy".into()], previous }]
    );
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("legacy"));

    // A box that is off does nothing, but the click still selects the row.
    let pending = pe_pending(&app, visual, tab_id);
    pe_click(visual, format!("properties-edit-nullable-{id}"));
    assert_eq!(pe_pending(&app, visual, tab_id), pending, "a key column cannot be NULL");
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("id"));

    // Drop, Restore and Remove of a row that is not selected.
    pe_select(&app, visual, tab_id, "name");
    pe_click(visual, format!("properties-edit-drop-{note}"));
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped);
    pe_select(&app, visual, tab_id, "name");
    pe_click(visual, format!("properties-edit-restore-{note}"));
    assert_ne!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped, "Restore takes the drop back");
    diagram_click(visual, "properties-edit-add-column");
    let added = pe_position(&app, visual, tab_id, "column1");
    pe_select(&app, visual, tab_id, "name");
    assert_eq!(pe_kind(&app, visual, tab_id, "column1"), RowKind::New);
    pe_click(visual, format!("properties-edit-remove-{added}"));
    assert!(pe_edit(&app, visual, tab_id).unwrap().read_with(visual, |edit, _| edit.rows().iter().all(|row| row.spec.name != "column1")), "Remove takes the new column out");
}

#[gpui::test]
fn a_drawn_box_that_is_off_says_why_and_the_selected_rows_real_controls_still_work(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_kind(&app, visual, tab_id, "legacy"), RowKind::Dropped);
    let legacy = pe_position(&app, visual, tab_id, "legacy");
    // The dropped row is not the selected one: its boxes are drawn, off, with the reason on hover.
    pe_select(&app, visual, tab_id, "name");
    assert!(pe_tooltip_shown(visual, format!("properties-edit-key-{legacy}")), "the reason shows on hover");
    visual.simulate_mouse_move(gpui::point(gpui::px(2.0), gpui::px(2.0)), None, Default::default());
    settle(visual);
    assert!(visual.debug_bounds("properties-edit-tooltip").is_none(), "and goes with the pointer");
    let pending = pe_pending(&app, visual, tab_id);
    pe_click(visual, format!("properties-edit-key-{legacy}"));
    pe_click(visual, format!("properties-edit-nullable-{legacy}"));
    assert_eq!(pe_pending(&app, visual, tab_id), pending, "a dropped column's boxes are off");

    // Only the selected row has the real controls (a tab stop each: Nullable, Key, Drop), Space
    // toggles the focused box, and selecting another row moves them there.
    let none = pe_tab_stops(visual);
    pe_select(&app, visual, tab_id, "note");
    let on_note = pe_tab_stops(visual);
    let note_controls: Vec<&String> = on_note.iter().filter(|stop| !none.contains(stop)).collect();
    assert_eq!(note_controls.len(), 3, "Nullable, Key and Drop of the selected row, and no other row's: {note_controls:?}");
    pe_focus(visual, note_controls[0]);
    pe_space(visual);
    let changes = pe_changes(&app, visual, tab_id);
    assert!(
        changes.iter().any(|change| matches!(change, Change::AlterColumn { to, .. } if to.name == "note" && !to.nullable)),
        "Space on the focused Nullable box of the selected row: {changes:?}"
    );
    pe_select(&app, visual, tab_id, "name");
    let on_name = pe_tab_stops(visual);
    let name_controls: Vec<&String> = on_name.iter().filter(|stop| !none.contains(stop)).collect();
    assert_eq!(name_controls.len(), 3, "the real controls moved to the newly selected row: {name_controls:?}");
    assert!(name_controls.iter().all(|stop| !note_controls.contains(stop)), "they are the new row's own");
    pe_focus(visual, name_controls[1]);
    pe_space(visual);
    let changes = pe_changes(&app, visual, tab_id);
    assert!(
        changes.iter().any(|change| matches!(change, Change::SetPrimaryKey { columns, .. } if columns.contains(&"name".to_owned()))),
        "Space on the focused Key box of the new selection: {changes:?}"
    );
}

#[gpui::test]
fn add_column_adds_a_unique_placeholder_that_can_be_removed(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let first_type = crate::sql_ddl::dialect_for(ConnectionKind::Postgres).unwrap().common_types()[0];
    diagram_click(visual, "properties-edit-add-column");
    diagram_click(visual, "properties-edit-add-column");
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [
            Change::AddColumn { table: pe_orders(), column: pe_spec("column1", first_type, true) },
            Change::AddColumn { table: pe_orders(), column: pe_spec("column2", first_type, true) },
        ]
    );
    assert_eq!(pe_kind(&app, visual, tab_id, "column2"), RowKind::New);
    let position = pe_position(&app, visual, tab_id, "column2");
    assert_eq!(position, 5, "new rows follow the described ones");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-marker-{position}"))).is_some());

    // The new column can be discarded with its row button, or with Delete.
    pe_click(visual, format!("properties-edit-remove-{position}"));
    assert_eq!(pe_changes(&app, visual, tab_id).len(), 1);
    pe_select(&app, visual, tab_id, "column1");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    // A free placeholder name is reused.
    diagram_click(visual, "properties-edit-add-column");
    assert!(matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AddColumn { column, .. }] if column.name == "column1"));
}

#[gpui::test]
fn undo_and_redo_walk_the_draft_with_the_buttons_and_the_keys(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);

    diagram_click(visual, "properties-edit-undo");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    assert_eq!(pe_kind(&app, visual, tab_id, "legacy"), RowKind::Existing);
    diagram_click(visual, "properties-edit-redo");
    assert_eq!(pe_pending(&app, visual, tab_id), 2);

    // Cmd/Ctrl+Z and Shift+Cmd/Ctrl+Z with the grid focused.
    pe_select(&app, visual, tab_id, "note");
    visual.simulate_keystrokes("secondary-z");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    visual.simulate_keystrokes("secondary-shift-z");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);

    // Discard all is an edit like the others.
    diagram_click(visual, "properties-edit-discard-all");
    assert_eq!(pe_pending(&app, visual, tab_id), 0);
    diagram_click(visual, "properties-edit-undo");
    assert_eq!(pe_pending(&app, visual, tab_id), 2);
}

#[gpui::test]
fn the_changes_dialog_lists_the_sql_of_every_change_and_copies_it(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    // Empty: the dialog says what to do from here, not "the diagram".
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-empty").is_some());
    diagram_click(visual, "changes-close");

    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    let note = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-nullable-{note}"));
    diagram_click(visual, "properties-edit-add-column");
    let name = pe_position(&app, visual, tab_id, "name");
    pe_click(visual, format!("properties-edit-key-{name}"));
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-subject").is_some(), "the dialog is open");
    for change in 0..4 {
        assert!(visual.debug_bounds(d3_sel(format!("changes-item-{change}"))).is_some(), "change {change}");
        assert!(visual.debug_bounds(d3_sel(format!("changes-sql-{change}-0"))).is_some(), "its SQL");
    }
    let sql = d3_copy_sql(visual);
    for expected in [
        "ALTER TABLE \"public\".\"orders\" DROP COLUMN \"legacy\"",
        "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"note\" SET NOT NULL",
        "ALTER TABLE \"public\".\"orders\" ADD COLUMN \"column1\"",
        "PRIMARY KEY",
    ] {
        assert!(sql.contains(expected), "{expected} in\n{sql}");
    }
    // Discarding one change in the dialog is an edit of the draft.
    diagram_click(visual, "changes-discard-1");
    assert_eq!(pe_pending(&app, visual, tab_id), 3);
    diagram_click(visual, "changes-close");
}

#[gpui::test]
fn a_destructive_change_and_a_production_connection_gate_apply_which_then_runs_and_refreshes(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    let (ran, fail) = pe_inject(&app, visual);
    publish_types(&app, visual, false);
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    let note = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-nullable-{note}"));

    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-understood").is_some(), "dropping a column loses data");
    assert!(visual.debug_bounds("changes-confirm").is_some(), "a production connection asks for a named confirmation");
    diagram_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "nothing runs without the acknowledgements");
    diagram_click(visual, "changes-understood");
    diagram_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "the confirmation is still missing");
    diagram_click(visual, "changes-confirm");

    // A failing statement: the report says so, the draft is kept, the error log hears about it.
    *fail.lock().unwrap() = Some("DROP COLUMN");
    diagram_click(visual, "changes-apply");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    wait_until(visual, "the apply to answer", |visual| edit.read_with(visual, |edit, _| !edit.review_applying()));
    settle(visual);
    assert!(visual.debug_bounds("changes-report").is_some());
    assert_eq!(pe_pending(&app, visual, tab_id), 2, "nothing was applied");
    assert!(app.read_with(visual, |this, _| (0..this.error_log.log.len()).filter_map(|i| this.error_log.log.get(i)).any(|e| e.message.contains("boom"))));

    // It works the second time: the statements run in one transaction, the dialog closes, the
    // draft is empty, the properties are fetched again.
    *fail.lock().unwrap() = None;
    ran.lock().unwrap().clear();
    let fetches = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab_id).unwrap().columns_panel.props.fetches)
    };
    let before = fetches(&app, visual);
    diagram_click(visual, "changes-understood");
    diagram_click(visual, "changes-confirm");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the changes to apply", |visual| edit.read_with(visual, |edit, _| edit.pending_count() == 0));
    wait_until(visual, "the properties to be fetched again", |visual| fetches(&app, visual) > before);
    settle(visual);
    let statements = ran.lock().unwrap().clone();
    assert_eq!(statements.first().map(String::as_str), Some("BEGIN"), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
    assert!(statements.iter().any(|sql| sql == "ALTER TABLE \"public\".\"orders\" DROP COLUMN \"legacy\""), "{statements:?}");
    assert!(statements.iter().any(|sql| sql.contains("ALTER COLUMN \"note\" SET NOT NULL")), "{statements:?}");
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog closed");
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 0, "the connection is free again");
    let state = edit.read_with(visual, |edit, _| edit.render_state());
    assert!(!state.can_undo, "undoing would bring back DDL that already ran");
}

#[gpui::test]
fn a_change_whose_table_moved_on_is_stale_and_refused_in_the_words_of_the_properties(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let (ran, _) = pe_inject(&app, visual);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    // Someone dropped `legacy` meanwhile: the refreshed table no longer has it.
    let mut moved = pe_described();
    moved.columns.retain(|column| column.name != "legacy");
    pe_describe(&app, visual, tab_id, moved);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert!(edit.read_with(visual, |edit, _| edit.session().stale.iter().any(Option::is_some)), "the drop is stale now");
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-stale-0").is_some(), "the dialog says which change");
    diagram_click(visual, "changes-apply");
    assert!(ran.lock().unwrap().is_empty(), "Apply is off for a stale change");
    // And the session itself refuses, whoever asks.
    let (changes, script) = edit.read_with(visual, |edit, _| {
        let changes = edit.draft_changes().to_vec();
        (changes.clone(), crate::sql_ddl::plan(&*edit.edit_context().unwrap().dialect(), &changes).script())
    });
    edit.update(visual, |edit, cx| ReviewHost::request_apply(edit, changes, script, cx));
    let report = edit.read_with(visual, |edit, _| edit.review_report()).expect("a refusal");
    assert!(report.1 && report.0.contains("stale") && report.0.contains("refresh the table properties"), "{report:?}");
    assert!(!report.0.contains("diagram"), "{report:?}");
    assert!(ran.lock().unwrap().is_empty());
}

#[gpui::test]
fn one_apply_runs_per_connection_whether_a_diagram_or_a_properties_tab_asks(cx: &mut TestAppContext) {
    use crate::app::results::diagram::view::DiagramKind;
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (log, released) = (ran.clone(), release.clone());
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, control: &crate::app::structure_edit::apply::ApplyControl| {
            log.lock().unwrap().push(sql.to_owned());
            while sql.starts_with("ALTER") && !released.load(std::sync::atomic::Ordering::SeqCst) {
                if control.is_cancelled() {
                    return Err("cancelled by the user".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(())
        }));
    });
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    open_review(visual, "properties-edit-changes");

    // A diagram applies on the connection: the Properties apply is refused and says why.
    let control = std::sync::Arc::new(crate::app::structure_edit::apply::ApplyControl::default());
    let held = app
        .update(visual, |this, _| this.diagram.applies.begin(&connection, ApplyTarget::Diagram { tab_id: 4242, kind: DiagramKind::Schema }, &control))
        .expect("the connection is free");
    diagram_click(visual, "changes-apply");
    settle(visual);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let report = edit.read_with(visual, |edit, _| edit.review_report()).expect("a report");
    assert!(report.1 && report.0.contains("already running"), "{report:?}");
    assert!(ran.lock().unwrap().is_empty(), "nothing ran");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    drop(held);

    // The Properties apply runs; now a diagram's request is the refused one.
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to start", |_| ran.lock().unwrap().iter().any(|sql| sql.starts_with("ALTER TABLE")));
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 1);
    let request = {
        let changes = pe_changes(&app, visual, tab_id);
        ApplyRequest { script: crate::sql_ddl::plan(crate::sql_ddl::dialect_for(ConnectionKind::Postgres).unwrap(), &changes).script(), changes, confirmed: true }
    };
    app.update(visual, |this, cx| {
        this.apply_structure_changes(ApplyTarget::Diagram { tab_id, kind: DiagramKind::Schema }, request, cx)
    });
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 1, "still the Properties one");
    assert!(
        app.read_with(visual, |this, _| (0..this.error_log.log.len())
            .filter_map(|i| this.error_log.log.get(i))
            .any(|e| e.area == crate::error_log::Area::Diagram && e.message.contains("already running"))),
        "the diagram's request is refused with a clear message"
    );
    release.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_until(visual, "the apply to finish", |visual| edit.read_with(visual, |edit, _| edit.pending_count() == 0));
    assert_eq!(app.read_with(visual, |this, _| this.diagram.applies.len()), 0);
}

#[gpui::test]
fn the_edit_state_is_dropped_when_the_tab_shows_another_table(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-subject").is_some());
    let old = pe_edit(&app, visual, tab_id).unwrap();

    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().collection = "customers".into());
    settle(visual);
    assert!(pe_edit(&app, visual, tab_id).is_none(), "the draft is dropped, not carried onto customers");
    assert!(visual.debug_bounds("changes-subject").is_none(), "its dialog went with it");
    assert!(old.read_with(visual, |edit, _| !edit.editable()), "and the old draft can no longer be applied");

    // The other table starts clean.
    pe_describe(&app, visual, tab_id, pe_described());
    assert!(visual.debug_bounds("properties-edit-changes").is_none());
    pe_start(&app, visual, tab_id);
    assert_eq!(pe_pending(&app, visual, tab_id), 0);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.session().pending_count()), 0);
}

// ---- inline cell editing (R3-2b) ----------------------------------------------------------------

use crate::app::results::properties_edit::Field;

fn pe_cell_selector(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, name: &str, field: Field) -> &'static str {
    let position = pe_position(app, visual, tab_id, name);
    d3_sel(format!("properties-edit-{}-{position}", field.slug()))
}

/// A click on the text cell `field` of column `name`.
fn pe_open_cell(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, name: &str, field: Field) {
    let selector = pe_cell_selector(app, visual, tab_id, name, field);
    diagram_click(visual, selector);
}

/// The cell the editor is open in, as (column name, field).
fn pe_editing(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<(String, Field)> {
    let edit = pe_edit(app, visual, tab_id)?;
    edit.read_with(visual, |edit, _| edit.editing().map(|(row, field)| (edit.rows()[row].spec.name.clone(), field)))
}

fn pe_editor_text(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> String {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    let editor = edit.read_with(visual, |edit, _| edit.editor().clone());
    editor.read_with(visual, |editor, _| editor.value().to_string())
}

/// Types `text` one character at a time, as the keyboard does (every character is its own edit,
/// which is what refilters the type suggestions).
fn pe_type(visual: &mut gpui::VisualTestContext, text: &str) {
    for character in text.chars() {
        visual.simulate_input(&character.to_string());
        settle(visual);
    }
}

/// Selects all, then types: the value of the open cell becomes `text`.
fn pe_replace(visual: &mut gpui::VisualTestContext, text: &str) {
    visual.simulate_keystrokes("secondary-a");
    settle(visual);
    pe_type(visual, text);
}

fn pe_menu_open(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> bool {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    let editor = edit.read_with(visual, |edit, _| edit.editor().clone());
    editor.read_with(visual, |editor, _| editor.completion_menu_state().open)
}

#[gpui::test]
fn a_click_opens_a_cell_and_enter_commits_a_rename_a_type_and_a_default(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);

    // Click opens the name; the value is selected, so typing replaces it.
    pe_open_cell(&app, visual, tab_id, "legacy", Field::Name);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("legacy".into(), Field::Name)));
    assert_eq!(pe_editor_text(&app, visual, tab_id), "legacy");
    pe_type(visual, "archive");
    assert_eq!(pe_editor_text(&app, visual, tab_id), "archive");
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "nothing is recorded before the commit");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::RenameColumn { table: pe_orders(), from: "legacy".into(), to: "archive".into() }]);
    assert_eq!(pe_kind(&app, visual, tab_id, "archive"), RowKind::Changed);

    // The selection followed the column, and the grid has the focus again: Right + Enter opens the type.
    visual.simulate_keystrokes("right enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("archive".into(), Field::Type)));
    pe_replace(visual, "varchar(320)");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [
            Change::RenameColumn { table: pe_orders(), from: "legacy".into(), to: "archive".into() },
            Change::AlterColumn { table: pe_orders(), from: pe_spec("archive", "text", true), to: pe_spec("archive", "varchar(320)", true) },
        ]
    );

    // F2 opens the default of the same column; an empty text is no default.
    visual.simulate_keystrokes("right f2");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("archive".into(), Field::Default)));
    pe_type(visual, "n/a");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let default = |changes: &[Change]| match changes.last() {
        Some(Change::AlterColumn { to, .. }) => to.default.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(default(&pe_changes(&app, visual, tab_id)), Some("n/a".into()));
    visual.simulate_keystrokes("enter");
    settle(visual);
    pe_replace(visual, "");
    visual.simulate_keystrokes("backspace enter");
    settle(visual);
    assert_eq!(default(&pe_changes(&app, visual, tab_id)), None);
}

fn pe_menu_labels(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Vec<String> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    let editor = edit.read_with(visual, |edit, _| edit.editor().clone());
    editor.read_with(visual, |editor, _| editor.completion_menu_state().items.iter().map(|item| item.label.clone()).collect())
}

#[gpui::test]
fn the_type_cell_offers_the_dialect_types_and_keeps_free_text(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let types: Vec<String> = crate::sql_ddl::dialect_for(ConnectionKind::Postgres).unwrap().common_types().iter().map(|t| t.to_string()).collect();

    // Opening the cell lists every type of the dialect; the name cell lists none.
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    assert!(!pe_menu_open(&app, visual, tab_id));
    pe_type(visual, "n");
    assert!(!pe_menu_open(&app, visual, tab_id), "typing a name suggests nothing");
    visual.simulate_keystrokes("escape");
    settle(visual);
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Type)));
    assert!(pe_menu_open(&app, visual, tab_id), "the picker opens with the cell");
    assert_eq!(pe_menu_labels(&app, visual, tab_id), types);

    // Typing narrows the list, the choice replaces the text, Enter on a closed menu commits.
    pe_replace(visual, "var");
    let labels = pe_menu_labels(&app, visual, tab_id);
    assert!(!labels.is_empty() && labels.iter().all(|label| label.starts_with("var")), "{labels:?}");
    let second = labels.get(1).cloned().unwrap_or_else(|| labels[0].clone());
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert!(!pe_menu_open(&app, visual, tab_id), "choosing closes the menu");
    assert_eq!(pe_editor_text(&app, visual, tab_id).trim_end(), second);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Type)), "the cell stays open until it is committed");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert!(matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AlterColumn { to, .. }] if to.data_type == second), "{:?}", pe_changes(&app, visual, tab_id));

    // Free text stays typeable: nothing matches `varchar(320)`, so the menu closes and Enter commits it.
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    assert_eq!(pe_editor_text(&app, visual, tab_id), second, "reopened on the current type");
    pe_replace(visual, "varchar(320)");
    assert!(!pe_menu_open(&app, visual, tab_id), "{:?}", pe_menu_labels(&app, visual, tab_id));
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AlterColumn { to, .. }] if to.data_type == "varchar(320)"));
}

#[gpui::test]
fn the_type_picker_closes_with_escape_and_with_a_click_outside(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);

    // Escape closes the menu first; the cell stays open. A second Escape cancels the cell.
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    assert!(pe_menu_open(&app, visual, tab_id));
    pe_replace(visual, "t");
    assert!(pe_menu_open(&app, visual, tab_id), "types start with t");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!pe_menu_open(&app, visual, tab_id));
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Type)));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "Escape restores the value");
    // Escape on the grid does nothing to the draft or the selection.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.selected().map(str::to_owned)).as_deref(), Some("note"));

    // Reopened, the cell shows the current type again, not what was typed before.
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    assert_eq!(pe_editor_text(&app, visual, tab_id), "text");
    assert!(pe_menu_open(&app, visual, tab_id));

    // A click outside (the title of the grid) closes the menu and the cell; the typed text is kept.
    pe_replace(visual, "bigint");
    assert!(pe_menu_open(&app, visual, tab_id));
    let toolbar = visual.debug_bounds("columns-view-header").expect("the grid toolbar");
    visual.simulate_click(toolbar.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    assert!(!pe_menu_open(&app, visual, tab_id), "the menu is dismissed");
    assert_eq!(pe_editing(&app, visual, tab_id), None, "the cell is closed");
    assert!(matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AlterColumn { to, .. }] if to.data_type == "bigint"), "{:?}", pe_changes(&app, visual, tab_id));
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    assert_eq!(pe_editor_text(&app, visual, tab_id), "bigint", "reopened on the new type");
}

#[gpui::test]
fn escape_cancels_an_edit_and_an_unchanged_value_records_nothing(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    pe_type(visual, "remark");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "Escape drops what was typed");
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Existing);
    // The selection stays, and the grid has the focus: the arrow keys move it.
    visual.simulate_keystrokes("up");
    settle(visual);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.selected().map(str::to_owned)).as_deref(), Some("name"));
    // The cell opens with the old value.
    visual.simulate_keystrokes("down enter");
    settle(visual);
    assert_eq!(pe_editor_text(&app, visual, tab_id), "note");
    // Enter on the value it opened with is no change; neither is typing it back.
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    visual.simulate_keystrokes("enter");
    settle(visual);
    visual.simulate_keystrokes("end");
    pe_type(visual, "x");
    assert_eq!(pe_editor_text(&app, visual, tab_id), "notex");
    visual.simulate_keystrokes("backspace enter");
    settle(visual);
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    // An edit of a type that is then put back folds away in the draft.
    pe_open_cell(&app, visual, tab_id, "note", Field::Default);
    pe_type(visual, "n/a");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_changes(&app, visual, tab_id).len(), 1);
    pe_open_cell(&app, visual, tab_id, "note", Field::Default);
    pe_replace(visual, "");
    visual.simulate_keystrokes("backspace enter");
    settle(visual);
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "{:?}", pe_changes(&app, visual, tab_id));
}

#[gpui::test]
fn tab_commits_and_opens_the_next_editable_cell_and_shift_tab_the_previous(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    // Drop `name`: its cells are skipped.
    pe_select(&app, visual, tab_id, "name");
    visual.simulate_keystrokes("delete");
    settle(visual);

    pe_open_cell(&app, visual, tab_id, "id", Field::Name);
    pe_type(visual, "ident");
    visual.simulate_keystrokes("tab");
    settle(visual);
    assert_eq!(pe_changes(&app, visual, tab_id).len(), 2, "the name was committed by Tab: {:?}", pe_changes(&app, visual, tab_id));
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("ident".into(), Field::Type)));
    for (name, field) in [("ident", Field::Default), ("ident", Field::Comment), ("note", Field::Name), ("note", Field::Type)] {
        visual.simulate_keystrokes("tab");
        settle(visual);
        assert_eq!(pe_editing(&app, visual, tab_id), Some((name.into(), field)), "the dropped column is skipped");
    }
    visual.simulate_keystrokes("shift-tab shift-tab shift-tab shift-tab");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("ident".into(), Field::Type)));
    // The editor is one entity the whole time: it holds the value of the cell that is open.
    assert_eq!(pe_editor_text(&app, visual, tab_id), "integer");
    // Shift+Tab at the first cell leaves the editor and keeps the draft.
    visual.simulate_keystrokes("shift-tab shift-tab");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert_eq!(pe_changes(&app, visual, tab_id).len(), 2);
}

#[gpui::test]
fn a_blur_commits_the_typed_value_and_a_refused_one_is_dropped_with_the_reason(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_open_cell(&app, visual, tab_id, "legacy", Field::Comment);
    pe_type(visual, "obsolete");
    // Another row's marker takes the focus away.
    pe_select(&app, visual, tab_id, "note");
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert!(
        matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AlterColumn { to, .. }] if to.name == "legacy" && to.comment.as_deref() == Some("obsolete")),
        "{:?}",
        pe_changes(&app, visual, tab_id)
    );

    // Clicking on another cell commits the open one and opens that one.
    pe_open_cell(&app, visual, tab_id, "legacy", Field::Name);
    pe_type(visual, "old");
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Name)), "the second cell opened");
    assert_eq!(pe_editor_text(&app, visual, tab_id), "note");
    assert!(pe_changes(&app, visual, tab_id).iter().any(|c| matches!(c, Change::RenameColumn { to, .. } if to == "old")));

    // A name another column has is refused: Enter keeps the editor open with the reason, a blur drops it.
    pe_type(visual, "name");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Name)), "refused: still open");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
    assert!(notice.as_ref().is_some_and(|(text, failed)| *failed && text.contains("already exists")), "{notice:?}");
    assert!(visual.debug_bounds("properties-edit-notice").is_some(), "the toolbar says why");
    pe_select(&app, visual, tab_id, "id");
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Existing, "the refused name was dropped");
}

#[gpui::test]
fn add_column_opens_its_name_and_every_cell_of_it_updates_the_add_column_change(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("column1".into(), Field::Name)), "the new row is open for its name");
    pe_type(visual, "email");
    visual.simulate_keystrokes("tab");
    settle(visual);
    pe_replace(visual, "varchar(320)");
    visual.simulate_keystrokes("tab");
    settle(visual);
    pe_type(visual, "''");
    visual.simulate_keystrokes("tab");
    settle(visual);
    pe_type(visual, "contact address");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let mut expected = pe_spec("email", "varchar(320)", true);
    expected.default = Some("''".into());
    expected.comment = Some("contact address".into());
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::AddColumn { table: pe_orders(), column: expected }]);
    assert_eq!(pe_kind(&app, visual, tab_id, "email"), RowKind::New);
}

#[gpui::test]
fn text_the_dialect_refuses_is_shown_on_the_row_and_blocks_apply(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let (ran, _) = pe_inject(&app, visual);
    pe_start(&app, visual, tab_id);
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    pe_replace(visual, "text; drop table orders");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let error = edit.read_with(visual, |edit, _| edit.row_error("note").map(str::to_owned)).expect("the row carries the dialect's reason");
    assert!(error.contains(';') || error.to_lowercase().contains("type"), "{error}");
    let position = pe_position(&app, visual, tab_id, "note");
    let marker = visual.debug_bounds(d3_sel(format!("properties-edit-marker-{position}"))).expect("the marker cell");
    assert!(marker.size.width > gpui::px(0.0));
    let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
    assert!(notice.is_some_and(|(text, failed)| failed && text.starts_with("note: ")), "the toolbar names the column");
    // An empty name is the dialect's to refuse as well.
    pe_open_cell(&app, visual, tab_id, "legacy", Field::Name);
    pe_replace(visual, "");
    visual.simulate_keystrokes("backspace enter");
    settle(visual);
    assert!(edit.read_with(visual, |edit, _| edit.row_error("").is_some()), "the empty name is refused by the dialect");

    // Both block Apply: the dialog lists the errors and Apply runs nothing.
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-error-0").is_some() || visual.debug_bounds("changes-error-1").is_some(), "the dialog names the refusal");
    diagram_click(visual, "changes-apply");
    settle(visual);
    assert!(ran.lock().unwrap().is_empty(), "{:?}", ran.lock().unwrap());
    diagram_click(visual, "changes-close");
    // Fixing the text clears the row's error.
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    pe_replace(visual, "varchar(30)");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(edit.read_with(visual, |edit, _| edit.row_error("note").is_none()));
}

#[gpui::test]
fn cmd_z_in_the_open_editor_undoes_its_text_not_the_draft(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    visual.simulate_keystrokes("enter");
    settle(visual);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);

    pe_open_cell(&app, visual, tab_id, "note", Field::Comment);
    pe_type(visual, "abc");
    visual.simulate_keystrokes("secondary-z");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2, "the draft is untouched");
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Comment)));
    assert!(pe_editor_text(&app, visual, tab_id).len() < 3, "the editor undid its own typing: {:?}", pe_editor_text(&app, visual, tab_id));
    visual.simulate_keystrokes("secondary-shift-z");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);
    // On the grid, the same chord is the draft's.
    visual.simulate_keystrokes("escape secondary-z");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
}

#[gpui::test]
fn a_dialect_that_cannot_alter_a_column_keeps_its_cells_shut_with_the_reason(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::SQLITE_ID.into();
    });
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described());
    pe_start(&app, visual, tab_id);
    // Type, default and comment of an existing column: refused with the reason; the name may change.
    for field in [Field::Type, Field::Default, Field::Comment] {
        pe_open_cell(&app, visual, tab_id, "note", field);
        assert_eq!(pe_editing(&app, visual, tab_id), None, "{field:?}");
        let edit = pe_edit(&app, visual, tab_id).unwrap();
        let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
        assert!(notice.is_some_and(|(text, failed)| failed && (text.contains("rebuilding the table") || text.contains("no column comments"))), "{field:?}");
    }
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".into(), Field::Name)));
    visual.simulate_keystrokes("escape");
    settle(visual);
    // A column the draft adds can be typed fully, but a comment is not offered by SQLite.
    diagram_click(visual, "properties-edit-add-column");
    visual.simulate_keystrokes("tab");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("column1".into(), Field::Type)));
    visual.simulate_keystrokes("tab tab");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None, "no comment cell on SQLite: Tab left the row's cells");
}

#[gpui::test]
fn a_view_cannot_be_edited_and_says_why(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    // The tab shows `daily_summary`, which the navigator lists under Views.
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    let views = NavigatorNodeId::ObjectGroup { connection: connection.clone(), database: "warehouse".into(), schema: "public".into(), group: ObjectGroup::Views };
    app.update(visual, |this, cx| {
        let row = this.navigator.visible_rows().into_iter().find(|row| row.id == views).expect("the Views group");
        this.handle_navigator_disclosure(&row, cx);
    });
    settle(visual);
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().collection = "daily_summary".into();
    });
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described());
    let reason = app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).expect("a table tab").expect_err("a view");
    assert!(reason.contains("Views and materialized views"), "{reason}");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none(), "the disabled toggle starts nothing");

    // The same tab as a table edits.
    app.update(visual, |this, _| {
        this.workspace.tab_mut(tab_id).unwrap().collection = "orders".into();
    });
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described());
    assert!(app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).unwrap().is_ok());
}

// ---- the draft is not dropped silently ----------------------------------------------------------

fn pe_discard_dialog_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.debug_bounds("discard-pending-body").is_some()
}

#[gpui::test]
fn closing_the_tab_with_a_draft_asks_and_cancel_keeps_it_while_discard_closes_it(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    // Nothing pending: closing does not ask (the other tests close tabs the same way).
    pe_start(&app, visual, tab_id);
    app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab_id, window, cx));
    settle(visual);
    assert!(!pe_discard_dialog_open(visual), "an empty draft is no reason to ask");
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).is_none()), "closed");

    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);

    app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab_id, window, cx));
    settle(visual);
    assert!(pe_discard_dialog_open(visual), "\"Discard 2 pending structure changes?\"");
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).is_some()));
    diagram_click(visual, "discard-pending-cancel");
    assert!(!pe_discard_dialog_open(visual));
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).is_some()), "Cancel keeps the tab");
    assert_eq!(pe_pending(&app, visual, tab_id), 2, "and the draft");

    // Cmd+W asks the same, and the tab bar's multi-tab closes too.
    app.update_in(visual, |this, window, cx| this.close_tabs_confirmed(vec![tab_id], None, window, cx));
    settle(visual);
    assert!(pe_discard_dialog_open(visual));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).is_some()));
    app.update_in(visual, |this, window, cx| this.confirm_close_tab(tab_id, window, cx));
    settle(visual);
    diagram_click(visual, "discard-pending-confirm");
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).is_none()), "Discard closes the tab");
    assert!(app.read_with(visual, |this, _| !this.properties_edits.contains_key(&tab_id)), "and drops the draft");
}

#[gpui::test]
fn closing_the_window_with_a_draft_asks_first(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    // Nothing pending: the window closes.
    assert!(visual.simulate_close(), "no draft: the close request goes through");

    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    assert!(!visual.simulate_close(), "a draft holds the window");
    settle(visual);
    assert!(pe_discard_dialog_open(visual));
    diagram_click(visual, "discard-pending-cancel");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    assert_eq!(visual.cx.update(|cx| cx.windows().len()), 1, "Cancel keeps the window");

    assert!(!visual.simulate_close());
    settle(visual);
    let discard = visual.debug_bounds("discard-pending-confirm").expect("the Discard button");
    visual.simulate_click(discard.center(), Default::default());
    visual.cx.run_until_parked();
    assert_eq!(visual.cx.update(|cx| cx.windows().len()), 0, "Discard closes the window");
}

#[gpui::test]
fn quitting_with_a_draft_asks_first(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    assert!(!app.update_in(visual, |this, window, cx| this.request_quit(window, cx)), "no draft: nothing to ask");
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    // The real Quit action (Cmd+Q) reaches the same question.
    visual.cx.update(|cx| crate::app::menus::finish_launch(cx, |_| {}));
    visual.simulate_keystrokes("secondary-q");
    settle(visual);
    assert!(pe_discard_dialog_open(visual), "Quit asks");
    diagram_click(visual, "discard-pending-cancel");
    assert!(app.update_in(visual, |this, window, cx| this.request_quit(window, cx)), "asks again");
    settle(visual);
    assert!(pe_discard_dialog_open(visual));
    diagram_click(visual, "discard-pending-cancel");
    assert_eq!(pe_pending(&app, visual, tab_id), 1, "Cancel keeps the draft");
}

#[gpui::test]
fn a_key_that_is_back_to_the_catalogs_when_its_new_column_goes_leaves_the_draft(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    let position = pe_position(&app, visual, tab_id, "column1");
    pe_click(visual, format!("properties-edit-key-{position}"));
    assert!(
        pe_changes(&app, visual, tab_id).iter().any(|change| matches!(change, Change::SetPrimaryKey { columns, .. } if columns == &["id".to_string(), "column1".to_string()])),
        "{:?}",
        pe_changes(&app, visual, tab_id)
    );
    pe_click(visual, format!("properties-edit-remove-{position}"));
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "the key is the catalog's again: {:?}", pe_changes(&app, visual, tab_id));
    assert_eq!(pe_pending(&app, visual, tab_id), 0);
}

#[gpui::test]
fn the_open_editor_sits_inside_its_cell_and_the_row_keeps_its_height(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    for field in Field::ALL {
        pe_open_cell(&app, visual, tab_id, "note", field);
        let selector = pe_cell_selector(&app, visual, tab_id, "note", field);
        let position = pe_position(&app, visual, tab_id, "note");
        let cell = visual.debug_bounds(selector).expect("the cell");
        let editor = visual.debug_bounds("properties-edit-editor").expect("the editor is drawn in the open cell");
        let row = visual.debug_bounds(d3_sel(format!("properties-edit-row-{position}"))).expect("the row");
        assert!(editor.origin.x >= cell.origin.x && editor.right() <= cell.right(), "{field:?}: the editor stays inside the cell's width");
        assert!(editor.origin.y >= cell.origin.y - gpui::px(0.5) && editor.bottom() <= cell.bottom() + gpui::px(0.5), "{field:?}: and its height");
        assert_eq!(row.size.height, gpui::px(26.0), "opening a cell does not change the row height");
        visual.simulate_keystrokes("escape");
        settle(visual);
    }
}

#[gpui::test]
fn a_generated_column_keeps_its_type_and_default_shut_and_a_sub_tab_switch_commits_the_open_cell(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let mut described = pe_described();
    described.columns.iter_mut().find(|column| column.name == "legacy").unwrap().is_generated = Some(true);
    pe_describe(&app, visual, tab_id, described);
    pe_start(&app, visual, tab_id);
    for field in [Field::Type, Field::Default] {
        pe_open_cell(&app, visual, tab_id, "legacy", field);
        assert_eq!(pe_editing(&app, visual, tab_id), None, "{field:?} of a generated column");
        let edit = pe_edit(&app, visual, tab_id).unwrap();
        let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
        assert!(notice.is_some_and(|(text, failed)| failed && text.contains("generated")), "{field:?}");
    }
    // Its name and comment stay editable; leaving for another sub-tab commits what is typed.
    pe_open_cell(&app, visual, tab_id, "legacy", Field::Comment);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("legacy".into(), Field::Comment)));
    pe_type(visual, "computed");
    properties_click_tab(visual, PropertiesTab::Keys);
    assert_eq!(pe_editing(&app, visual, tab_id), None);
    assert!(matches!(&pe_changes(&app, visual, tab_id)[..], [Change::AlterColumn { to, .. }] if to.comment.as_deref() == Some("computed")));
}

/// The `table-properties-edit` scenario of the capture runner, with `TABLEX_SEED_PROPERTIES` = `seed`.
fn pe_scenario<'a>(cx: &'a mut TestAppContext, seed: Option<&str>) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext, u64) {
    cx.dispatcher.allow_parking();
    match seed {
        Some(seed) => std::env::set_var("TABLEX_SEED_PROPERTIES", seed),
        None => std::env::remove_var("TABLEX_SEED_PROPERTIES"),
    }
    let (app, visual) = context_test_app(cx, "table-properties-edit", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    let tab_id = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    // The fixture describes the table; the seed stages its edits once it is loaded.
    wait_until(visual, "the seed to stage its edits", |visual| {
        settle(visual);
        pe_pending(&app, visual, tab_id) > 0
    });
    std::env::remove_var("TABLEX_SEED_PROPERTIES");
    (app, visual, tab_id)
}

// One test, five windows: the seed reads a process-wide variable, so no other test may run beside it.
#[gpui::test]
fn the_table_properties_edit_scenario_stages_a_rename_a_type_change_an_added_and_a_dropped_column(
    cx_default: &mut TestAppContext,
    cx_review: &mut TestAppContext,
    cx_type: &mut TestAppContext,
    cx_indexes: &mut TestAppContext,
    cx_foreign_keys: &mut TestAppContext,
) {
    let (app, visual, tab_id) = pe_scenario(cx_default, None);
    let changes = pe_changes(&app, visual, tab_id);
    let titles: Vec<String> = changes.iter().map(Change::title).collect();
    assert_eq!(changes.len(), 6, "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::CreateIndex { index, .. } if index.name == "idx_orders_order_status" && index.columns == ["order_status"])), "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::AddForeignKey { foreign_key, .. } if foreign_key.name == "fk_orders_region" && foreign_key.ref_table.name == "regions")), "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::RenameColumn { from, to, .. } if from == "status" && to == "order_status")), "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::AlterColumn { to, .. } if to.name == "score" && to.data_type == "numeric(12,2)")), "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::DropColumn { column, .. } if column == "big_number")), "{titles:?}");
    assert!(changes.iter().any(|c| matches!(c, Change::AddColumn { column, .. } if column.name == "region" && column.data_type == "varchar(40)")), "{titles:?}");
    for selector in PE_CONTROLS {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} is shown");
    }
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog is not open unless asked for");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.rows().len()), 9, "the table has its columns");

    // `review`: the Changes dialog is open over the same edits.
    let (app, visual, tab_id) = pe_scenario(cx_review, Some("review"));
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 6);
    assert!(visual.debug_bounds("changes-subject").is_some(), "the review dialog is open");

    // `type`: the Type cell of `payload` is open on its picker.
    let (app, visual, tab_id) = pe_scenario(cx_type, Some("type"));
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("payload".into(), Field::Type)));
    assert!(pe_menu_open(&app, visual, tab_id));

    // `indexes` / `foreign-keys`: that sub-tab is shown, with the pending row among the described ones.
    let (app, visual, tab_id) = pe_scenario(cx_indexes, Some("indexes"));
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).0, PropertiesTab::Indexes);
    let rows = pe_rows(&app, visual, tab_id, PropertiesTab::Indexes);
    assert_eq!(rows.last().map(|row| (row.name.as_str(), row.kind)), Some(("idx_orders_order_status", RowKind::New)), "{rows:?}");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-row-{}", rows.len() - 1))).is_some(), "the pending index is drawn");
    let (app, visual, tab_id) = pe_scenario(cx_foreign_keys, Some("foreign-keys"));
    settle(visual);
    assert_eq!(properties_snapshot(&app, visual).0, PropertiesTab::ForeignKeys);
    let rows = pe_rows(&app, visual, tab_id, PropertiesTab::ForeignKeys);
    assert_eq!(rows.last().map(|row| (row.name.as_str(), row.kind)), Some(("fk_orders_region", RowKind::New)), "{rows:?}");
    assert!(visual.debug_bounds(d3_sel(format!("properties-edit-row-{}", rows.len() - 1))).is_some(), "the pending foreign key is drawn");
}

// ---- indexes, foreign keys and the primary key marker (R3-3) -----------------------------------

use crate::app::results::properties_edit::lists::ListRow;

/// `pe_described` plus what the list sub-tabs show: the key's index, an index on `name`, a foreign key
/// to `customers`.
fn pe_described_with_lists() -> crate::catalog::TableProperties {
    use crate::catalog::{CatalogForeignKey, CatalogIndex};
    let mut properties = pe_described();
    properties.indexes = Some(vec![
        CatalogIndex { name: "orders_pkey".into(), columns: vec!["id".into()], unique: true, primary: true, method: Some("btree".into()), definition: None },
        CatalogIndex { name: "orders_name_idx".into(), columns: vec!["name".into()], unique: false, primary: false, method: Some("btree".into()), definition: None },
    ]);
    properties.foreign_keys = Some(vec![CatalogForeignKey {
        name: "orders_customer_fk".into(),
        table: "public.orders".into(),
        columns: vec!["note".into()],
        referenced_table: "public.customers".into(),
        referenced_columns: vec!["id".into()],
        on_update: Some("NO ACTION".into()),
        on_delete: Some("CASCADE".into()),
        definition: None,
    }]);
    properties
}

fn pe_app_with_lists(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_describe(&app, visual, tab_id, pe_described_with_lists());
    (app, visual, tab_id)
}

fn pe_popup_open(visual: &mut gpui::VisualTestContext) -> bool {
    visual.update(|_, cx| gpui_component::GlobalState::is_in_deferred_context(cx))
}

fn pe_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64, tab: PropertiesTab) -> Vec<ListRow> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.lists().rows(tab).to_vec())
}

/// Click on option `row` of the open popup that hangs below `selector` (rows are 28px high).
fn pe_option(visual: &mut gpui::VisualTestContext, selector: &'static str, row: usize) {
    pe_option_below(visual, selector, row, 0.0);
}

/// [`pe_option`] for a popup with a search box of `search` px above its rows.
fn pe_option_below(visual: &mut gpui::VisualTestContext, selector: &'static str, row: usize, search: f32) {
    let trigger = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is drawn"));
    visual.simulate_click(gpui::point(trigger.origin.x + gpui::px(30.0), trigger.bottom() + gpui::px(search + 14.0 + 28.0 * row as f32)), Default::default());
    settle(visual);
}

fn pe_names(rows: &[ListRow]) -> Vec<(&str, RowKind)> {
    rows.iter().map(|row| (row.name.as_str(), row.kind)).collect()
}

fn pe_form_open(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> bool {
    pe_edit(app, visual, tab_id).is_some_and(|edit| edit.read_with(visual, |edit, _| edit.form_is_open()))
}

fn pe_form_columns(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Vec<String> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, cx| edit.form_columns(cx))
}

fn pe_form_name(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> String {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, cx| edit.form_name(cx))
}

fn pe_form_error(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<String> {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.form_error().map(str::to_owned))
}

fn pe_index(name: &str, columns: &[&str], unique: bool) -> Change {
    Change::CreateIndex {
        table: pe_orders(),
        index: crate::sql_ddl::IndexSpec { name: name.into(), columns: columns.iter().map(|c| c.to_string()).collect(), unique },
    }
}

#[gpui::test]
fn the_indexes_tab_lists_the_rows_and_drop_and_restore_walk_the_draft(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Indexes);
    assert_eq!(pe_names(&pe_rows(&app, visual, tab_id, PropertiesTab::Indexes)), [("orders_pkey", RowKind::Existing), ("orders_name_idx", RowKind::Existing)]);
    for selector in ["properties-edit-toggle", "properties-edit-changes", "properties-edit-undo", "properties-edit-redo", "properties-edit-discard-all", "properties-edit-add-index"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} on Indexes");
    }
    assert!(visual.debug_bounds("properties-edit-add-column").is_none() && visual.debug_bounds("properties-edit-add-foreign-key").is_none());
    for position in 0..2 {
        assert!(visual.debug_bounds(d3_sel(format!("properties-edit-row-{position}"))).is_some(), "row {position}");
    }

    // The index behind the primary key goes with the key: Drop is off.
    pe_click(visual, "properties-edit-drop-0".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "the key's index cannot be dropped on its own");

    pe_click(visual, "properties-edit-drop-1".into());
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::DropIndex { table: pe_orders(), name: "orders_name_idx".into() }]);
    assert_eq!(pe_rows(&app, visual, tab_id, PropertiesTab::Indexes)[1].kind, RowKind::Dropped);
    assert!(visual.debug_bounds("properties-edit-restore-1").is_some(), "Restore replaces Drop");
    assert!(visual.debug_bounds("properties-edit-drop-1").is_none());
    open_review(visual, "properties-edit-changes");
    let sql = d3_copy_sql(visual);
    assert!(sql.contains("DROP INDEX \"public\".\"orders_name_idx\""), "{sql}");
    diagram_click(visual, "changes-close");

    pe_click(visual, "properties-edit-restore-1".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    assert_eq!(pe_rows(&app, visual, tab_id, PropertiesTab::Indexes)[1].kind, RowKind::Existing);
    // Undo / Redo work from this tab as well.
    pe_click(visual, "properties-edit-drop-1".into());
    diagram_click(visual, "properties-edit-undo");
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    diagram_click(visual, "properties-edit-redo");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
}

#[gpui::test]
fn add_index_picks_columns_in_order_and_the_picker_dismisses_with_escape_and_a_click_outside(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Indexes);
    assert!(!pe_form_open(&app, visual, tab_id));
    diagram_click(visual, "properties-edit-add-index");
    assert!(pe_form_open(&app, visual, tab_id) && visual.debug_bounds("properties-edit-form").is_some());
    assert_eq!(pe_form_name(&app, visual, tab_id), "idx_orders", "a generated name to start with");
    assert!(!pe_popup_open(visual));

    // Open the picker: the options are drawn below the trigger and a click toggles one.
    diagram_click(visual, "properties-edit-form-columns");
    assert!(pe_popup_open(visual), "the picker opens");
    pe_option(visual, "properties-edit-form-columns", 2); // note
    pe_option(visual, "properties-edit-form-columns", 1); // name
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["note", "name"], "the order they were picked in");
    assert!(pe_popup_open(visual), "a multi-select stays open while picking");
    assert_eq!(pe_form_name(&app, visual, tab_id), "idx_orders_note_name", "the generated name follows the columns");
    // Picking again takes one out.
    pe_option(visual, "properties-edit-form-columns", 2);
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["name"]);
    pe_option(visual, "properties-edit-form-columns", 2);
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["name", "note"]);

    // Escape closes the picker, not the form; reopened, it still holds the selection.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!pe_popup_open(visual), "Escape dismisses the picker");
    assert!(pe_form_open(&app, visual, tab_id), "and leaves the form");
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["name", "note"]);
    diagram_click(visual, "properties-edit-form-columns");
    assert!(pe_popup_open(visual));
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["name", "note"], "reopened with the selection");
    // A click outside dismisses it too, without click-through to what lies below.
    let title = visual.debug_bounds("properties-title").expect("the toolbar title");
    visual.simulate_click(title.center(), Default::default());
    settle(visual);
    assert!(!pe_popup_open(visual), "a click outside dismisses the picker");
    assert!(pe_form_open(&app, visual, tab_id));
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["name", "note"]);

    // Unique toggles; a typed name is kept when the columns change.
    diagram_click(visual, "properties-edit-form-unique");
    diagram_click(visual, "properties-edit-form-submit");
    assert!(!pe_form_open(&app, visual, tab_id), "added: the form closes");
    assert_eq!(pe_changes(&app, visual, tab_id), [pe_index("idx_orders_name_note", &["name", "note"], true)]);
    assert_eq!(
        pe_names(&pe_rows(&app, visual, tab_id, PropertiesTab::Indexes)),
        [("orders_pkey", RowKind::Existing), ("orders_name_idx", RowKind::Existing), ("idx_orders_name_note", RowKind::New)]
    );
    assert!(visual.debug_bounds("properties-edit-remove-2").is_some(), "a new row is removed, not dropped");
    open_review(visual, "properties-edit-changes");
    let sql = d3_copy_sql(visual);
    assert!(sql.contains("CREATE UNIQUE INDEX \"idx_orders_name_note\" ON \"public\".\"orders\" (\"name\", \"note\")"), "{sql}");
    diagram_click(visual, "changes-close");
    pe_click(visual, "properties-edit-remove-2".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty());
}

#[gpui::test]
fn a_refused_index_keeps_the_form_open_with_the_reason_and_escape_closes_the_form(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Indexes);
    diagram_click(visual, "properties-edit-add-index");
    // No column.
    diagram_click(visual, "properties-edit-form-submit");
    assert_eq!(pe_form_error(&app, visual, tab_id).as_deref(), Some("An index needs a name and at least one column."));
    assert!(visual.debug_bounds("properties-edit-form-error").is_some(), "the form says why");
    // A name the table already has.
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 1);
    visual.simulate_keystrokes("escape");
    settle(visual);
    let name = visual.debug_bounds("properties-edit-form-name").expect("name input");
    visual.simulate_click(name.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("orders_name_idx");
    settle(visual);
    assert_eq!(pe_form_name(&app, visual, tab_id), "orders_name_idx");
    diagram_click(visual, "properties-edit-form-submit");
    assert_eq!(pe_form_error(&app, visual, tab_id).as_deref(), Some("There is an index named orders_name_idx already."));
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    // The typed name survives a change of the columns.
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 2);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_form_name(&app, visual, tab_id), "orders_name_idx");
    // Escape with no picker open closes the form; Cancel does too.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!pe_form_open(&app, visual, tab_id), "Escape closes the form");
    diagram_click(visual, "properties-edit-add-index");
    assert!(pe_form_open(&app, visual, tab_id));
    assert_eq!(pe_form_columns(&app, visual, tab_id), Vec::<String>::new(), "a new form starts empty");
    diagram_click(visual, "properties-edit-form-cancel");
    assert!(!pe_form_open(&app, visual, tab_id));
    assert!(pe_changes(&app, visual, tab_id).is_empty());
}


/// The search box of a popup with more than ten entries.
const PE_SEARCH: f32 = 30.0;

fn pe_form_referenced(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> (Option<String>, Vec<String>) {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, cx| edit.form_referenced(cx))
}

fn pe_referenced_state(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> &'static str {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, _| edit.referenced_state())
}

fn pe_form_actions(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> (Option<crate::sql_ddl::FkAction>, Option<crate::sql_ddl::FkAction>) {
    let edit = pe_edit(app, visual, tab_id).expect("edit mode is on");
    edit.read_with(visual, |edit, cx| edit.form_actions(cx))
}

#[gpui::test]
fn add_foreign_key_chooses_the_table_loads_its_columns_and_adds_the_key(cx: &mut TestAppContext) {
    use crate::sql_ddl::FkAction;
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    assert_eq!(pe_names(&pe_rows(&app, visual, tab_id, PropertiesTab::ForeignKeys)), [("orders_customer_fk", RowKind::Existing)]);
    for selector in ["properties-edit-toggle", "properties-edit-changes", "properties-edit-add-foreign-key"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} on Foreign Keys");
    }
    assert!(visual.debug_bounds("properties-edit-add-index").is_none());
    diagram_click(visual, "properties-edit-add-foreign-key");
    assert_eq!(pe_form_name(&app, visual, tab_id), "fk_orders");
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "nothing");
    assert!(visual.debug_bounds("properties-edit-form-ref-columns").is_some());

    // The columns of this table.
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 2); // note
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_form_columns(&app, visual, tab_id), ["note"]);
    assert_eq!(pe_form_name(&app, visual, tab_id), "fk_orders_note");

    // The referenced table: a Select over the schema's tables (a search box above 10 of them).
    diagram_click(visual, "properties-edit-form-table");
    assert!(pe_popup_open(visual), "the Select opens");
    pe_option_below(visual, "properties-edit-form-table", 1, PE_SEARCH);
    assert!(!pe_popup_open(visual), "a choice closes the Select");
    let (table, columns) = pe_form_referenced(&app, visual, tab_id);
    assert_eq!(table.as_deref(), Some("fixture_table_01"), "the model holds the choice");
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loaded", "its columns were described");
    assert!(columns.is_empty());
    // Reopened, a different choice replaces the first and its columns are fetched again.
    diagram_click(visual, "properties-edit-form-table");
    assert!(pe_popup_open(visual));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!pe_popup_open(visual), "Escape dismisses the Select");
    assert_eq!(pe_form_referenced(&app, visual, tab_id).0.as_deref(), Some("fixture_table_01"), "dismissed: the choice stays");
    diagram_click(visual, "properties-edit-form-table");
    pe_option_below(visual, "properties-edit-form-table", 0, PE_SEARCH);
    assert_eq!(pe_form_referenced(&app, visual, tab_id).0.as_deref(), Some("orders"));
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loaded");
    // A click outside dismisses it as well.
    diagram_click(visual, "properties-edit-form-table");
    assert!(pe_popup_open(visual));
    let title = visual.debug_bounds("properties-title").expect("the toolbar title");
    visual.simulate_click(title.center(), Default::default());
    settle(visual);
    assert!(!pe_popup_open(visual), "a click outside dismisses the Select");
    assert_eq!(pe_form_referenced(&app, visual, tab_id).0.as_deref(), Some("orders"));

    // The referenced columns: the fixture describes every table with the same columns (`id` first).
    diagram_click(visual, "properties-edit-form-ref-columns");
    assert!(pe_popup_open(visual), "the referenced columns' picker opens");
    pe_option(visual, "properties-edit-form-ref-columns", 0);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_form_referenced(&app, visual, tab_id).1, ["id"]);

    // ON DELETE: none, NO ACTION, RESTRICT, CASCADE...; ON UPDATE keeps its default.
    assert_eq!(pe_form_actions(&app, visual, tab_id), (None, None));
    diagram_click(visual, "properties-edit-form-on-delete");
    assert!(pe_popup_open(visual));
    pe_option(visual, "properties-edit-form-on-delete", 3);
    assert!(!pe_popup_open(visual));
    assert_eq!(pe_form_actions(&app, visual, tab_id), (Some(FkAction::Cascade), None));
    diagram_click(visual, "properties-edit-form-on-update");
    pe_option(visual, "properties-edit-form-on-update", 2);
    assert_eq!(pe_form_actions(&app, visual, tab_id), (Some(FkAction::Cascade), Some(FkAction::Restrict)));
    // Reopened, the Select keeps what was chosen (a Confirm of the same row changes nothing).
    diagram_click(visual, "properties-edit-form-on-delete");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(pe_form_actions(&app, visual, tab_id).0, Some(FkAction::Cascade));

    diagram_click(visual, "properties-edit-form-submit");
    assert!(!pe_form_open(&app, visual, tab_id), "{:?}", pe_form_error(&app, visual, tab_id));
    let key = crate::sql_ddl::ForeignKeySpec {
        name: "fk_orders_note".into(),
        columns: vec!["note".into()],
        ref_table: pe_orders(),
        ref_columns: vec!["id".into()],
        on_delete: Some(FkAction::Cascade),
        on_update: Some(FkAction::Restrict),
    };
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::AddForeignKey { table: pe_orders(), foreign_key: key }]);
    assert_eq!(
        pe_names(&pe_rows(&app, visual, tab_id, PropertiesTab::ForeignKeys)),
        [("orders_customer_fk", RowKind::Existing), ("fk_orders_note", RowKind::New)]
    );
    open_review(visual, "properties-edit-changes");
    let sql = d3_copy_sql(visual);
    assert!(
        sql.contains("ALTER TABLE \"public\".\"orders\" ADD CONSTRAINT \"fk_orders_note\" FOREIGN KEY (\"note\") REFERENCES \"public\".\"orders\" (\"id\") ON DELETE CASCADE ON UPDATE RESTRICT"),
        "{sql}"
    );
    diagram_click(visual, "changes-close");
    pe_click(visual, "properties-edit-remove-1".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty());
}

#[gpui::test]
fn the_referenced_columns_show_loading_and_a_failure_with_retry_and_a_stale_answer_is_dropped(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    diagram_click(visual, "properties-edit-add-foreign-key");
    let edit = pe_edit(&app, visual, tab_id).unwrap();

    // The description fails: the form says so and offers Retry; Add refuses meanwhile.
    crate::ui_fixture_properties::FAIL_NEXT.with(|fail| fail.set(1));
    diagram_click(visual, "properties-edit-form-table");
    pe_option_below(visual, "properties-edit-form-table", 1, PE_SEARCH);
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "failed");
    assert!(visual.debug_bounds("properties-edit-form-ref-error").is_some(), "the reason is shown");
    diagram_click(visual, "properties-edit-form-submit");
    assert!(pe_form_error(&app, visual, tab_id).is_some_and(|text| text.contains("could not be loaded")), "{:?}", pe_form_error(&app, visual, tab_id));
    diagram_click(visual, "properties-edit-form-retry");
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loaded", "Retry described it again");

    // While a table is being described the form is in its loading state; an answer for another
    // table is dropped, the right one fills the picker.
    edit.update(visual, |edit, cx| edit.start_referenced_for_test("fixture_table_01", cx));
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loading");
    edit.update(visual, |edit, cx| edit.referenced_described("fixture_table_09", Ok(vec!["zzz".into()]), cx));
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loading", "an answer for a table that was not chosen is ignored");
    settle(visual);
    assert_eq!(pe_referenced_state(&app, visual, tab_id), "loaded");
    diagram_click(visual, "properties-edit-form-ref-columns");
    pe_option(visual, "properties-edit-form-ref-columns", 0);
    assert_eq!(pe_form_referenced(&app, visual, tab_id).1, ["id"], "the picker holds the described columns, not the stale answer's");
}

#[gpui::test]
fn dropping_and_restoring_an_existing_foreign_key_and_the_sql_of_both(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    pe_click(visual, "properties-edit-drop-0".into());
    assert_eq!(pe_changes(&app, visual, tab_id), [Change::DropForeignKey { table: pe_orders(), name: "orders_customer_fk".into() }]);
    assert_eq!(pe_rows(&app, visual, tab_id, PropertiesTab::ForeignKeys)[0].kind, RowKind::Dropped);
    assert!(visual.debug_bounds("properties-edit-restore-0").is_some());
    open_review(visual, "properties-edit-changes");
    let sql = d3_copy_sql(visual);
    assert!(sql.contains("ALTER TABLE \"public\".\"orders\" DROP CONSTRAINT \"orders_customer_fk\""), "{sql}");
    diagram_click(visual, "changes-close");
    pe_click(visual, "properties-edit-restore-0".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    assert_eq!(pe_rows(&app, visual, tab_id, PropertiesTab::ForeignKeys)[0].kind, RowKind::Existing);
}

#[gpui::test]
fn indexes_and_foreign_keys_are_off_with_the_dialects_own_reason(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    let edit_of = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| pe_edit(app, visual, tab_id).expect("edit mode is on");

    // SQLite can create and drop indexes, and can neither add nor drop a foreign key of an existing table.
    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::SQLITE_ID.into());
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described_with_lists());
    settle(visual);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let reason = edit_of(&app, visual).read_with(visual, |edit, _| edit.lists().foreign_key_add_block.clone()).expect("SQLite cannot add a foreign key");
    assert!(reason.contains("SQLite cannot add the foreign key") && reason.contains("rebuild"), "{reason}");
    assert!(visual.debug_bounds("properties-edit-list-note").is_some(), "the tab says why");
    diagram_click(visual, "properties-edit-add-foreign-key");
    assert!(!pe_form_open(&app, visual, tab_id), "the disabled button opens nothing");
    pe_click(visual, "properties-edit-drop-0".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "and Drop is off for the existing key");
    let block = edit_of(&app, visual).read_with(visual, |edit, _| edit.lists().foreign_key_blocks[0].clone()).expect("a reason");
    assert!(block.contains("SQLite cannot drop the foreign key orders_customer_fk"), "{block}");
    properties_click_tab(visual, PropertiesTab::Indexes);
    assert!(edit_of(&app, visual).read_with(visual, |edit, _| edit.lists().index_add_block.is_none()));
    diagram_click(visual, "properties-edit-add-index");
    assert!(pe_form_open(&app, visual, tab_id), "SQLite adds indexes");
    diagram_click(visual, "properties-edit-form-cancel");

    // ClickHouse has neither.
    app.update(visual, |this, _| {
        let id = this.workspace.tab(tab_id).unwrap().connection_id.clone();
        this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::POSTGRES_ID.into();
        let saved = this.saved_connections.iter_mut().find(|saved| saved.matches_id(&id) || saved.matches_id(crate::ui_fixture::POSTGRES_ID)).expect("the fixture connection");
        saved.kind = ConnectionKind::Clickhouse;
    });
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described_with_lists());
    if pe_edit(&app, visual, tab_id).is_none() {
        pe_start(&app, visual, tab_id);
    }
    properties_click_tab(visual, PropertiesTab::Indexes);
    let reason = edit_of(&app, visual).read_with(visual, |edit, _| edit.lists().index_add_block.clone()).expect("ClickHouse has no indexes");
    assert!(reason.contains("ClickHouse has no secondary indexes"), "{reason}");
    diagram_click(visual, "properties-edit-add-index");
    assert!(!pe_form_open(&app, visual, tab_id));
    pe_click(visual, "properties-edit-drop-1".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let reason = edit_of(&app, visual).read_with(visual, |edit, _| edit.lists().foreign_key_add_block.clone()).expect("ClickHouse has no foreign keys");
    assert!(reason.contains("ClickHouse has no foreign keys"), "{reason}");
}

#[gpui::test]
fn a_connection_that_turns_read_only_freezes_the_index_and_foreign_key_controls(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Indexes);
    diagram_click(visual, "properties-edit-add-index");
    assert!(pe_form_open(&app, visual, tab_id));
    publish_types(&app, visual, true);
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let reason = edit.read_with(visual, |edit, _| edit.lists().index_add_block.clone()).expect("read-only");
    assert!(reason.contains("read-only"), "{reason}");
    pe_click(visual, "properties-edit-drop-1".into());
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "Drop is off");
    // The open form cannot add either.
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 1);
    visual.simulate_keystrokes("escape");
    settle(visual);
    diagram_click(visual, "properties-edit-form-submit");
    assert!(pe_changes(&app, visual, tab_id).is_empty(), "a frozen draft takes nothing");
}

#[gpui::test]
fn the_keys_tab_shows_what_the_draft_does_to_the_primary_key(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Keys);
    assert!(visual.debug_bounds("properties-edit-key-pending").is_none(), "nothing pending");
    properties_click_tab(visual, PropertiesTab::Columns);
    let name = pe_position(&app, visual, tab_id, "name");
    pe_click(visual, format!("properties-edit-key-{name}"));
    properties_click_tab(visual, PropertiesTab::Keys);
    assert!(visual.debug_bounds("properties-edit-key-pending").is_some(), "the strip says the key is changing");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let key = edit.read_with(visual, |edit, _| edit.lists().key.clone()).expect("a pending key");
    assert_eq!(key.summary(), "Pending: the primary key (id) becomes (id, name); adds name");
    assert_eq!((key.before, key.after), (vec!["id".to_owned()], vec!["id".to_owned(), "name".to_owned()]));
    // The existing key's row is still listed (read-only), marked.
    assert!(visual.debug_bounds("properties-row-0").is_some());

    // Removing `id` from the key as well.
    properties_click_tab(visual, PropertiesTab::Columns);
    let id = pe_position(&app, visual, tab_id, "id");
    pe_click(visual, format!("properties-edit-key-{id}"));
    properties_click_tab(visual, PropertiesTab::Keys);
    let summary = edit.read_with(visual, |edit, _| edit.lists().key.as_ref().map(|key| key.summary())).expect("a pending key");
    assert_eq!(summary, "Pending: the primary key (id) becomes (name); adds name; removes id");

    // Discarding brings the strip down; dropping the whole key says so.
    diagram_click(visual, "properties-edit-discard-all");
    assert!(visual.debug_bounds("properties-edit-key-pending").is_none());
    properties_click_tab(visual, PropertiesTab::Columns);
    let id = pe_position(&app, visual, tab_id, "id");
    pe_click(visual, format!("properties-edit-key-{id}"));
    properties_click_tab(visual, PropertiesTab::Keys);
    let summary = edit.read_with(visual, |edit, _| edit.lists().key.as_ref().map(|key| key.summary())).expect("a pending key");
    assert_eq!(summary, "Pending: the primary key (id) is dropped");
}

#[gpui::test]
fn a_name_the_table_already_uses_is_refused_by_the_plan_on_its_row_and_blocks_apply(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    let (ran, _) = pe_inject(&app, visual);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    // Changes the editors never make (they refuse the name first) but a draft can hold.
    edit.update(visual, |edit, cx| {
        assert!(edit.push_for_test(pe_index("orders_name_idx", &["note"], false), cx));
        assert!(edit.push_for_test(Change::AddColumn { table: pe_orders(), column: pe_spec("note", "text", true) }, cx));
        assert!(edit.push_for_test(
            Change::AddForeignKey {
                table: pe_orders(),
                foreign_key: crate::sql_ddl::ForeignKeySpec {
                    name: "orders_customer_fk".into(),
                    columns: vec!["note".into()],
                    ref_table: pe_orders(),
                    ref_columns: vec!["id".into()],
                    ..Default::default()
                },
            },
            cx
        ));
    });
    settle(visual);
    let (index_error, fk_error, column_error, stale) = edit.read_with(visual, |edit, _| {
        (
            edit.list_error(PropertiesTab::Indexes, "orders_name_idx").map(str::to_owned),
            edit.list_error(PropertiesTab::ForeignKeys, "orders_customer_fk").map(str::to_owned),
            edit.row_error("note").map(str::to_owned),
            edit.session().stale.iter().flatten().count(),
        )
    });
    assert_eq!(index_error.as_deref(), Some("An index named \"orders_name_idx\" already exists on public.orders."));
    assert_eq!(fk_error.as_deref(), Some("A foreign key named \"orders_customer_fk\" already exists on public.orders."));
    assert_eq!(column_error.as_deref(), Some("A column named \"note\" already exists in public.orders."));
    assert_eq!(stale, 0, "a clash is the plan's refusal, not a stale change");

    // The row of the index carries the "!" marker; the dialog names the refusal; Apply runs nothing.
    properties_click_tab(visual, PropertiesTab::Indexes);
    assert!(visual.debug_bounds("properties-edit-marker-2").is_some());
    open_review(visual, "properties-edit-changes");
    assert!(visual.debug_bounds("changes-error-0").is_some(), "the dialog names the refusal");
    diagram_click(visual, "changes-apply");
    settle(visual);
    assert!(ran.lock().unwrap().is_empty(), "{:?}", ran.lock().unwrap());
    diagram_click(visual, "changes-close");
}

#[gpui::test]
fn an_index_and_a_foreign_key_from_the_forms_are_reviewed_and_applied_through_the_injected_executor(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    let (ran, _) = pe_inject(&app, visual);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::Indexes);
    diagram_click(visual, "properties-edit-add-index");
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 2);
    visual.simulate_keystrokes("escape");
    settle(visual);
    diagram_click(visual, "properties-edit-form-submit");
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    diagram_click(visual, "properties-edit-add-foreign-key");
    diagram_click(visual, "properties-edit-form-columns");
    pe_option(visual, "properties-edit-form-columns", 2);
    visual.simulate_keystrokes("escape");
    settle(visual);
    diagram_click(visual, "properties-edit-form-table");
    pe_option_below(visual, "properties-edit-form-table", 0, PE_SEARCH);
    diagram_click(visual, "properties-edit-form-ref-columns");
    pe_option(visual, "properties-edit-form-ref-columns", 0);
    visual.simulate_keystrokes("escape");
    settle(visual);
    diagram_click(visual, "properties-edit-form-submit");
    assert_eq!(pe_pending(&app, visual, tab_id), 2, "{:?}", pe_form_error(&app, visual, tab_id));

    open_review(visual, "properties-edit-changes");
    for change in 0..2 {
        assert!(visual.debug_bounds(d3_sel(format!("changes-sql-{change}-0"))).is_some(), "the SQL of change {change}");
    }
    let sql = d3_copy_sql(visual);
    assert!(sql.contains("CREATE INDEX \"idx_orders_note\" ON \"public\".\"orders\" (\"note\")"), "{sql}");
    assert!(sql.contains("ADD CONSTRAINT \"fk_orders_note\" FOREIGN KEY (\"note\") REFERENCES \"public\".\"orders\" (\"id\")"), "{sql}");
    diagram_click(visual, "changes-apply");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    wait_until(visual, "the changes to apply", |visual| edit.read_with(visual, |edit, _| edit.pending_count() == 0));
    settle(visual);
    let statements = ran.lock().unwrap().clone();
    assert!(statements.iter().any(|sql| sql.starts_with("CREATE INDEX \"idx_orders_note\"")), "{statements:?}");
    assert!(statements.iter().any(|sql| sql.contains("ADD CONSTRAINT \"fk_orders_note\" FOREIGN KEY")), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
    assert!(visual.debug_bounds("changes-subject").is_none(), "the dialog closed");
}

// ---- live: a column, an index and a foreign key through the Properties edit flow ------------------
// (ignored without TABLEX_TEST_PG_URI / TABLEX_TEST_MYSQL_URI / TABLEX_TEST_MARIADB_URI)
//
// The changes are the ones the Properties editors make (`PropertiesEdit::add_column`, the Type cell's
// `AlterColumn`, the index and foreign key forms through `structure_edit::specs`), put through
// `Draft::push`; they run on the shared apply path (`run_apply`: a dedicated session, the lock timeout,
// the catalog re-check inside the transaction) exactly as `ApplyTarget::Properties` does, and the
// described table is read back the way the Properties reads it.

mod live_properties {
    use super::live_apply::Live;
    use super::*;
    use crate::app::results::properties_edit::{build_rows, lists};
    use crate::app::structure_edit::{live as describe_live, specs, EditScope};
    use crate::sql_ddl::draft::Draft;
    use crate::sql_ddl::DEFAULT_LOCK_TIMEOUT_SECS;
    use engine_driver_conformance::harness::Ctx;
    use engine_driver_conformance::SqlFixture;

    fn describe(ctx: &Ctx<'_>, live: &Live<'_>, table: &str) -> crate::catalog::TableProperties {
        let postgres = matches!(live.kind, ConnectionKind::Postgres);
        let request = serde_json::json!({
            "database": if postgres { Some(live.scope.database.clone()) } else { Some(live.namespace.clone()) },
            "schema": postgres.then(|| live.namespace.clone()),
            "object": table,
        });
        let value = ctx.connect().expect("a reader").describe(&request).unwrap_or_else(|error| panic!("describe {table}: {}", engine_driver_conformance::harness::describe(&error)));
        serde_json::from_value(value).expect("table properties")
    }

    fn run(fixture: &dyn SqlFixture, uri: &str, dylib: std::path::PathBuf, kind: ConnectionKind) {
        let ctx = Ctx::load(&dylib, uri, fixture).expect("load the driver");
        let scratch = ctx.scratch(false).expect("scratch namespace");
        let postgres = matches!(kind, ConnectionKind::Postgres);
        let live = Live {
            ctx: &ctx,
            kind,
            scope: EditScope {
                kind,
                database: if postgres { scratch.scratch.database.clone() } else { scratch.scratch.name.clone() },
                schema: postgres.then(|| scratch.scratch.name.clone()),
            },
            namespace: scratch.scratch.name.clone(),
        };
        let unique = ctx.unique();
        let (parent, child) = (format!("parent_{unique}"), format!("child_{unique}"));
        let engine = if postgres { "" } else { " ENGINE=InnoDB" };
        let tables = [live.qualified(&child), live.qualified(&parent)];
        // Whatever happens below, the test tables go.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut setup = ctx.connect().expect("the user's session");
            for sql in [
                format!("CREATE TABLE {} (id INT PRIMARY KEY){engine}", live.qualified(&parent)),
                format!("CREATE TABLE {} (id INT PRIMARY KEY, parent_id INT, note VARCHAR(20)){engine}", live.qualified(&child)),
            ] {
                setup.exec(&sql).unwrap_or_else(|failure| panic!("{sql}: {failure:?}"));
            }
            drop(setup);

            // What the Properties describes, and the rows it edits.
            let table = live.table(&child);
            let before = describe(&ctx, &live, &child);
            let described = describe_live::live_table(&before);
            let rows = build_rows(&before, &described, &Draft::default(), &table);
            let note = rows.iter().find(|row| row.spec.name == "note").expect("note").spec.clone();
            let dialect = crate::sql_ddl::dialect_for(kind).unwrap();

            let mut draft = Draft::default();
            // Add column: the placeholder row of `add_column` (first common type, nullable).
            let added = ColumnSpec { name: "column1".into(), data_type: dialect.common_types()[0].into(), nullable: true, ..ColumnSpec::default() };
            draft.push(Change::AddColumn { table: table.clone(), column: added });
            // Alter the type of `note` (the Type cell).
            let widened = ColumnSpec { data_type: "varchar(80)".into(), ..note.clone() };
            draft.push(Change::AlterColumn { table: table.clone(), from: note, to: widened });
            // Add index (the form): name, columns in the order picked, unique.
            let index_name = specs::default_index_name(&child, &["note".into()], |_| false);
            let index = specs::index_spec(&index_name, vec!["note".into()], true, |name| before.indexes.iter().flatten().any(|i| i.name == name)).expect("a valid index");
            draft.push(Change::CreateIndex { table: table.clone(), index });
            // Add foreign key to the other table (the form).
            let fk = crate::sql_ddl::ForeignKeySpec {
                name: specs::default_foreign_key_name(&child, &["parent_id".into()], |_| false),
                columns: vec!["parent_id".into()],
                ref_table: live.table(&parent),
                ref_columns: vec!["id".into()],
                on_delete: Some(crate::sql_ddl::FkAction::Cascade),
                on_update: None,
            };
            specs::check_foreign_key(&fk).expect("a valid foreign key");
            draft.push(Change::AddForeignKey { table: table.clone(), foreign_key: fk.clone() });

            // The plan the dialog would show has no refusal, and the catalog re-check passes.
            let run = live.apply(draft.changes.clone(), DEFAULT_LOCK_TIMEOUT_SECS).ok().expect("the apply ran");
            assert!(run.outcome.succeeded(), "{}", run.outcome.report(&run.planned.steps()));
            assert_eq!(run.outcome.completed_changes(&run.planned).len(), 4, "every change is done");

            // Re-describe: the Properties reads the new structure.
            let after = describe(&ctx, &live, &child);
            let column = |name: &str| after.columns.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("column {name} in {:?}", after.columns));
            assert!(column("column1").nullable, "{:?}", after.columns);
            assert!(column("note").data_type.contains("(80)"), "widened from varchar(20): {:?}", column("note"));
            let indexes = after.indexes.as_deref().unwrap_or_default();
            assert!(indexes.iter().any(|i| i.name == index_name && i.columns == ["note"] && i.unique), "{indexes:?}");
            let keys = after.foreign_keys.as_deref().unwrap_or_default();
            assert!(
                keys.iter().any(|k| k.name == fk.name && k.columns == ["parent_id"] && k.referenced_table.ends_with(&parent) && k.referenced_columns == ["id"]),
                "{keys:?}"
            );
            assert!(keys.iter().any(|k| k.on_delete.as_deref().is_some_and(|action| action.eq_ignore_ascii_case("CASCADE"))), "{keys:?}");

            // The edit mode's rows over the described table show them as existing, nothing pending.
            let empty = Draft::default();
            let index_rows = lists::build_index_rows(&after, &empty, &table);
            assert!(index_rows.iter().any(|row| row.name == index_name && row.kind == RowKind::Existing), "{index_rows:?}");
            let fk_rows = lists::build_foreign_key_rows(&after, &empty, &table);
            assert!(fk_rows.iter().any(|row| row.name == fk.name && row.kind == RowKind::Existing), "{fk_rows:?}");
            let rows = build_rows(&after, &describe_live::live_table(&after), &empty, &table);
            assert!(rows.iter().any(|row| row.spec.name == "column1" && row.kind == RowKind::Existing));

            // And a second apply of the same index is refused by the re-check, not run twice.
            let again = live.apply(
                vec![Change::CreateIndex { table: table.clone(), index: crate::sql_ddl::IndexSpec { name: index_name.clone(), columns: vec!["note".into()], unique: true } }],
                DEFAULT_LOCK_TIMEOUT_SECS,
            );
            assert!(again.is_err_and(|refusal| refusal.message.contains("exists")), "a name the table has is refused");
        }));
        let mut cleanup = ctx.connect().expect("a session to clean up with");
        for table in tables {
            let _ = cleanup.exec(&format!("DROP TABLE IF EXISTS {table}"));
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_PG_URI"]
    fn postgres_a_column_an_index_and_a_foreign_key_from_the_properties_edit_flow() {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-postgres", "dbengine_postgres", &["abi-v2"]);
        run(&engine_driver_conformance::fixtures::PostgresFixture, &uri, dylib, ConnectionKind::Postgres);
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MYSQL_URI"]
    fn mysql_a_column_an_index_and_a_foreign_key_from_the_properties_edit_flow() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MYSQL_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mysql", "dbengine_mysql", &["abi-v2"]);
        run(&engine_driver_conformance::fixtures::MysqlFixture::MYSQL, &uri, dylib, ConnectionKind::Mysql);
    }

    #[test]
    #[ignore = "needs TABLEX_TEST_MARIADB_URI"]
    fn mariadb_a_column_an_index_and_a_foreign_key_from_the_properties_edit_flow() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MARIADB_URI") else { return };
        let dylib = crate::sql_ddl::live::dylib("dbengine-mariadb", "dbengine_mariadb", &["abi-v2"]);
        run(&engine_driver_conformance::fixtures::MysqlFixture::MARIADB, &uri, dylib, ConnectionKind::Mariadb);
    }
}

#[gpui::test]
fn the_foreign_key_tab_asks_for_the_schemas_tables_shows_loading_a_failure_with_retry_and_enables_add(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let group = app.read_with(visual, |this, _| {
        let tab = this.workspace.tab(tab_id).unwrap();
        crate::navigator::object_group_id(&tab.connection_id, &tab.database, tab.schema.as_deref().unwrap_or(""), crate::navigator::ObjectGroup::Tables)
    });
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let block = |visual: &mut gpui::VisualTestContext| edit.read_with(visual, |edit, _| edit.lists().foreign_key_add_block.clone());
    assert!(block(visual).is_none(), "the schema's tables are known");

    // The names are being loaded (the answer has not come): Add is off, the tab says it is loading.
    let request = app
        .update(visual, |this, cx| {
            this.navigator.reset_node(&group);
            cx.notify();
            this.navigator.begin_background_load(&group)
        })
        .expect("the navigator starts the load");
    settle(visual);
    assert!(block(visual).is_some_and(|text| text.contains("Loading the tables")), "{:?}", block(visual));
    assert!(visual.debug_bounds("properties-edit-list-note").is_some(), "a loading line is drawn");
    assert!(visual.debug_bounds("properties-edit-catalog-retry").is_none());
    diagram_click(visual, "properties-edit-add-foreign-key");
    assert!(!pe_form_open(&app, visual, tab_id), "nothing opens while the names are missing");

    // The load fails: the reason and Retry.
    let generation = app.read_with(visual, |this, _| this.navigator.load_generation(&request)).expect("a generation");
    app.update(visual, |this, cx| {
        this.navigator.apply_load(&request, generation, Err("connection refused".into()));
        cx.notify();
    });
    settle(visual);
    assert!(block(visual).is_some_and(|text| text.contains("Could not load the tables") && text.contains("connection refused")), "{:?}", block(visual));
    assert!(visual.debug_bounds("properties-edit-catalog-retry").is_some(), "Retry is offered");
    diagram_click(visual, "properties-edit-add-foreign-key");
    assert!(!pe_form_open(&app, visual, tab_id));

    // Retry goes through the navigator's own load again (the fixture answers it at once): the failure
    // is gone and the names are there.
    diagram_click(visual, "properties-edit-catalog-retry");
    assert!(app.read_with(visual, |this, _| matches!(this.navigator.load_state(&group), Some(crate::navigator::NodeLoadState::Loaded))), "the load ran again");
    assert!(visual.debug_bounds("properties-edit-catalog-retry").is_none());
    assert!(block(visual).is_none(), "{:?}", block(visual));
    diagram_click(visual, "properties-edit-add-foreign-key");
    assert!(pe_form_open(&app, visual, tab_id), "Add foreign key opens once the tables are known");
    diagram_click(visual, "properties-edit-form-table");
    assert!(pe_popup_open(visual));
    pe_option_below(visual, "properties-edit-form-table", 1, PE_SEARCH);
    assert_eq!(pe_form_referenced(&app, visual, tab_id).0.as_deref(), Some("fixture_table_01"), "the Select lists the names that arrived");

    // Names that are simply not there (never loaded, or reset): the tab asks the navigator for them by itself.
    diagram_click(visual, "properties-edit-form-cancel");
    app.update(visual, |this, cx| {
        this.navigator.reset_node(&group);
        cx.notify();
    });
    settle(visual);
    assert!(app.read_with(visual, |this, _| matches!(this.navigator.load_state(&group), Some(crate::navigator::NodeLoadState::Loaded))), "loaded on demand");
    assert!(block(visual).is_none(), "{:?}", block(visual));
}

#[gpui::test]
fn the_toolbar_and_the_grid_fit_a_1100_px_window_and_the_toggle_reads_as_on_while_changes_are_pending(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    diagram_click(visual, "properties-edit-add-column");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(pe_pending(&app, visual, tab_id) >= 2);

    // Every control of the Columns toolbar is inside it.
    let content = visual.debug_bounds("columns-content").expect("the Columns content");
    for selector in ["properties-edit-toggle", "properties-edit-changes", "properties-edit-undo", "properties-edit-redo", "properties-edit-discard-all", "properties-edit-add-column"] {
        let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector}"));
        assert!(bounds.right() <= content.right() + gpui::px(0.5), "{selector} is cut off: {bounds:?} in {content:?}");
    }
    // The action cell of every row is fully visible; Restore included (the dropped row).
    let dropped = pe_position(&app, visual, tab_id, "legacy");
    for position in 0..5 {
        let row = visual.debug_bounds(d3_sel(format!("properties-edit-row-{position}"))).expect("a row");
        assert!(row.right() <= content.right() + gpui::px(0.5), "row {position} is wider than the grid: {row:?} in {content:?}");
    }
    let restore = visual.debug_bounds(d3_sel(format!("properties-edit-restore-{dropped}"))).expect("Restore is drawn");
    assert!(restore.right() <= content.right() + gpui::px(0.5) && restore.size.width >= gpui::px(40.0), "Restore is fully visible: {restore:?}");
    // The header and the rows share their column edges.
    let header = visual.debug_bounds("columns-table-header").expect("the grid header");
    let row = visual.debug_bounds("properties-edit-row-0").expect("the first row");
    assert_eq!((header.origin.x, header.size.width), (row.origin.x, row.size.width));

    // The toggle is not greyed out while changes are pending: it is the active mode, and says why it stays.
    properties_click_tab(visual, PropertiesTab::ForeignKeys);
    let content = visual.debug_bounds("properties-content").expect("the Foreign Keys content");
    for selector in ["properties-edit-toggle", "properties-edit-changes", "properties-edit-discard-all", "properties-edit-add-foreign-key"] {
        let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector}"));
        assert!(bounds.right() <= content.right() + gpui::px(0.5), "{selector} is cut off on Foreign Keys: {bounds:?} in {content:?}");
    }
    properties_click_tab(visual, PropertiesTab::Columns);
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_some(), "the pending changes keep the mode");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
    assert!(notice.is_some_and(|(text, failed)| failed && text.contains("Apply or discard")), "clicking the toggle says what to do");
}

#[gpui::test]
fn a_long_warning_wraps_inside_the_review_dialog(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app_with_lists(cx);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let long = "a_column_with_a_very_long_name_".repeat(8);
    edit.update(visual, |edit, cx| {
        edit.push_for_test(Change::DropColumn { table: pe_orders(), column: long }, cx);
    });
    open_review(visual, "properties-edit-changes");
    let item = visual.debug_bounds("changes-item-0").expect("the change");
    let warning = visual.debug_bounds("changes-warning-0").expect("its warning");
    assert!(warning.right() <= item.right() + gpui::px(0.5), "the warning stays inside the dialog: {warning:?} in {item:?}");
    assert!(warning.size.height > gpui::px(20.0), "and wraps onto more than one line: {warning:?}");
    diagram_click(visual, "changes-close");
}

// ---- review round: apply of a renamed + altered column, key order, names, views, the apply's owner ---

/// A table whose key is composite and not in column order: key (tenant_id, id), columns id, tenant_id, x.
fn pe_described_composite() -> crate::catalog::TableProperties {
    use crate::catalog::{CatalogColumn, CatalogConstraint, CatalogConstraintKind};
    let column = |name: &str, primary_key: bool| CatalogColumn { name: name.into(), data_type: "integer".into(), nullable: !primary_key, primary_key, ..CatalogColumn::default() };
    crate::catalog::TableProperties {
        columns: vec![column("id", true), column("tenant_id", true), column("x", false)],
        keys: Some(vec![CatalogConstraint {
            name: "orders_pkey".into(),
            kind: CatalogConstraintKind::PrimaryKey,
            columns: vec!["tenant_id".into(), "id".into()],
            definition: None,
        }]),
        foreign_keys: Some(Vec::new()),
        indexes: Some(Vec::new()),
        ..crate::catalog::TableProperties::default()
    }
}

fn pe_key_columns(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> Option<Vec<String>> {
    pe_changes(app, visual, tab_id).into_iter().find_map(|change| match change {
        Change::SetPrimaryKey { columns, .. } => Some(columns),
        _ => None,
    })
}

#[gpui::test]
fn the_key_checkbox_keeps_the_keys_own_order_and_a_renamed_key_column_stays_in_the_key(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_describe(&app, visual, tab_id, pe_described_composite());
    pe_start(&app, visual, tab_id);
    let key_box = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, name: &str| {
        let position = pe_position(app, visual, tab_id, name);
        pe_click(visual, format!("properties-edit-key-{position}"));
    };
    // The key is (tenant_id, id) whatever order the columns are listed in: x goes at the end of it.
    key_box(&app, visual, "x");
    assert_eq!(pe_key_columns(&app, visual, tab_id), Some(vec!["tenant_id".into(), "id".into(), "x".into()]));
    // Taking it out again is the catalog's key: the pending change goes, the key is not reordered.
    key_box(&app, visual, "x");
    assert_eq!(pe_key_columns(&app, visual, tab_id), None, "{:?}", pe_changes(&app, visual, tab_id));
    assert!(pe_changes(&app, visual, tab_id).is_empty());
    // Taking a column out keeps the order of the rest.
    key_box(&app, visual, "tenant_id");
    assert_eq!(pe_key_columns(&app, visual, tab_id), Some(vec!["id".into()]));
    diagram_click(visual, "properties-edit-discard-all");

    // A key column renamed in the draft is still a key column, and ticking another one builds on it.
    pe_open_cell(&app, visual, tab_id, "tenant_id", Field::Name);
    pe_replace(visual, "tenant");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert!(edit.read_with(visual, |edit, _| edit.rows().iter().any(|row| row.spec.name == "tenant" && row.primary_key)), "the renamed column is still a key column");
    key_box(&app, visual, "x");
    assert_eq!(pe_key_columns(&app, visual, tab_id), Some(vec!["tenant".into(), "id".into(), "x".into()]));
    key_box(&app, visual, "x");
    assert_eq!(pe_key_columns(&app, visual, tab_id), None, "back to the catalog's key under the new name: nothing to replace");
    assert_eq!(pe_pending(&app, visual, tab_id), 1, "only the rename is left: {:?}", pe_changes(&app, visual, tab_id));
}

/// A renamed key column plus a key change reviews as executable (not stale) and applies, whichever
/// of the two was made first.
#[gpui::test]
fn a_renamed_key_column_and_a_key_change_apply_in_either_order(cx: &mut TestAppContext) {
    for rename_first in [true, false] {
        let (app, visual, tab_id) = pe_app(cx);
        let (ran, _) = pe_inject(&app, visual);
        pe_describe(&app, visual, tab_id, pe_described_composite());
        pe_start(&app, visual, tab_id);
        let rename = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
            pe_open_cell(app, visual, tab_id, "tenant_id", Field::Name);
            pe_replace(visual, "tenant");
            visual.simulate_keystrokes("enter");
            settle(visual);
        };
        if rename_first {
            rename(&app, visual);
        }
        let position = pe_position(&app, visual, tab_id, "x");
        pe_click(visual, format!("properties-edit-key-{position}"));
        if !rename_first {
            rename(&app, visual);
        }
        assert_eq!(pe_key_columns(&app, visual, tab_id), Some(vec!["tenant".into(), "id".into(), "x".into()]), "rename first: {rename_first}");
        let edit = pe_edit(&app, visual, tab_id).unwrap();
        open_review(visual, "properties-edit-changes");
        let stale = edit.read_with(visual, |edit, _| edit.session().stale.iter().flatten().cloned().collect::<Vec<_>>());
        assert!(stale.is_empty(), "rename first: {rename_first}: {stale:?}");
        diagram_click(visual, "changes-apply");
        wait_until(visual, "the changes to apply", |visual| edit.read_with(visual, |edit, _| edit.pending_count() == 0));
        let statements = ran.lock().unwrap().clone();
        assert!(statements.iter().any(|sql| sql.contains("RENAME COLUMN \"tenant_id\" TO \"tenant\"")), "{statements:?}");
        assert!(statements.iter().any(|sql| sql.contains("PRIMARY KEY (\"tenant\", \"id\", \"x\")")), "{statements:?}");
        assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
    }
}

#[gpui::test]
fn a_column_renamed_and_retyped_in_the_editor_applies(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let (ran, _) = pe_inject(&app, visual);
    pe_start(&app, visual, tab_id);
    pe_open_cell(&app, visual, tab_id, "note", Field::Type);
    pe_replace(visual, "varchar(80)");
    visual.simulate_keystrokes("enter");
    settle(visual);
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    pe_replace(visual, "memo");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_pending(&app, visual, tab_id), 2);
    open_review(visual, "properties-edit-changes");
    // text -> varchar(80) is not a proven widening: the data-loss acknowledgement comes first.
    diagram_click(visual, "changes-understood");
    diagram_click(visual, "changes-apply");
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    // An Apply click that missed (a dialog still sliding in) fails here, not after the 15 s wait.
    let (started, report) =
        edit.read_with(visual, |edit, _| (edit.review_applying() || edit.pending_count() == 0, edit.review_report()));
    assert!(started, "Apply did not start: report {report:?}, ran {:?}", ran.lock().unwrap());
    wait_until(visual, "the changes to apply", |visual| edit.read_with(visual, |edit, _| edit.pending_count() == 0));
    let statements = ran.lock().unwrap().clone();
    assert!(statements.iter().any(|sql| sql.contains("RENAME COLUMN \"note\" TO \"memo\"")), "{statements:?}");
    assert!(statements.iter().any(|sql| sql.contains("ALTER COLUMN \"memo\" TYPE varchar(80)")), "{statements:?}");
    assert_eq!(statements.last().map(String::as_str), Some("COMMIT"), "{statements:?}");
}

#[gpui::test]
fn a_column_may_be_renamed_onto_the_name_of_a_dropped_one_and_names_compare_the_way_the_server_does(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    pe_replace(visual, "legacy");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None, "the dropped column's name is free: the drop runs first");
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [Change::DropColumn { table: pe_orders(), column: "legacy".into() }, Change::RenameColumn { table: pe_orders(), from: "note".into(), to: "legacy".into() }]
    );
    let script = pe_edit(&app, visual, tab_id).unwrap().read_with(visual, |edit, _| {
        let planned = crate::sql_ddl::plan_against(&*edit.edit_context().unwrap().dialect(), edit.draft_changes(), &crate::sql_ddl::schema::LiveSchema::new(false));
        assert!(planned.executable(), "{:?}", planned.first_error());
        planned.script()
    });
    assert!(script.find("DROP COLUMN \"legacy\"") < script.find("RENAME COLUMN \"note\" TO \"legacy\""), "the drop runs before the rename: {script}");
    // The rows share the name: the one that stays is the one the editor acts on, and the dropped one
    // cannot come back while the other holds its name.
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    assert_eq!(edit.read_with(visual, |edit, _| edit.rows().iter().filter(|row| row.spec.name == "legacy").count()), 2);
    assert_eq!(pe_kind(&app, visual, tab_id, "legacy"), RowKind::Changed, "the name means the column that stays");
    pe_click(visual, "properties-edit-restore-3".into());
    assert_eq!(pe_changes(&app, visual, tab_id).len(), 2, "Restore is refused while the name is taken");
    let notice = edit.read_with(visual, |edit, _| edit.render_state().notice);
    assert!(notice.is_some_and(|(text, failed)| failed && text.contains("exists now")), "the toolbar says why");

    // Another live column's name is still refused; PostgreSQL tells `NAME` from `name`, SQLite does not.
    pe_open_cell(&app, visual, tab_id, "id", Field::Name);
    pe_replace(visual, "name");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("id".into(), Field::Name)), "refused: still open");
    pe_replace(visual, "NAME");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), None, "PostgreSQL: NAME is another column than name");
    diagram_click(visual, "properties-edit-discard-all");

    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().connection_id = crate::ui_fixture::SQLITE_ID.into());
    settle(visual);
    pe_describe(&app, visual, tab_id, pe_described());
    if pe_edit(&app, visual, tab_id).is_none() {
        pe_start(&app, visual, tab_id);
    }
    pe_open_cell(&app, visual, tab_id, "id", Field::Name);
    pe_replace(visual, "NAME");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("id".into(), Field::Name)), "SQLite compares column names without case");
}

#[test]
fn a_ddl_that_creates_a_view_is_recognised() {
    use crate::app::results::properties_edit::ddl_is_view;
    for view in [
        "CREATE VIEW v AS SELECT 1",
        "create or replace view public.v as select 1",
        "CREATE MATERIALIZED VIEW m AS SELECT 1",
        "CREATE TEMP VIEW t AS SELECT 1",
        "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW `v` AS select 1",
        "-- public.v definition\n\nCREATE OR REPLACE VIEW public.v AS SELECT 1",
        "/* generated */ CREATE VIEW v AS SELECT 1",
    ] {
        assert!(ddl_is_view(view), "{view}");
    }
    for other in ["CREATE TABLE t (id int)", "CREATE TABLE view_log (id int)", "CREATE UNIQUE INDEX i ON t (a)", "", "SELECT 1", "-- only a comment", "/* unterminated"] {
        assert!(!ddl_is_view(other), "{other}");
    }
}

#[gpui::test]
fn a_view_the_navigator_does_not_know_yet_is_recognised_by_its_ddl(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    // `unlisted` is in no navigator group: its kind is unknown.
    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().collection = "unlisted".into());
    settle(visual);
    let mut described = pe_described();
    described.ddl = Some("CREATE VIEW unlisted AS SELECT id FROM orders".into());
    pe_describe(&app, visual, tab_id, described);
    let reason = app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).expect("a table tab").expect_err("a view");
    assert!(reason.contains("Views and materialized views"), "{reason}");
    diagram_click(visual, "properties-edit-toggle");
    assert!(pe_edit(&app, visual, tab_id).is_none());
    // The same name described as a table edits.
    let mut table = pe_described();
    table.ddl = Some("CREATE TABLE unlisted (id integer)".into());
    pe_describe(&app, visual, tab_id, table);
    assert!(app.read_with(visual, |this, _| this.properties_edit_gate(tab_id)).unwrap().is_ok());
}

#[gpui::test]
fn a_read_only_connection_neither_undoes_redoes_nor_discards_and_records_no_history(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    visual.simulate_keystrokes("escape");
    settle(visual);
    pe_select(&app, visual, tab_id, "legacy");
    visual.simulate_keystrokes("delete");
    settle(visual);
    diagram_click(visual, "properties-edit-undo");
    assert_eq!(pe_pending(&app, visual, tab_id), 1);
    publish_types(&app, visual, true);
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let before = edit.read_with(visual, |edit, _| (edit.draft_changes().to_vec(), edit.session().history.undo_len(), edit.session().history.can_redo()));
    edit.update(visual, |edit, cx| {
        edit.undo(cx);
        edit.redo(cx);
        edit.discard_all(cx);
        ReviewHost::discard_change(edit, 0, cx);
        edit.restore_column("legacy", cx);
        edit.drop_column("legacy", cx);
    });
    let after = edit.read_with(visual, |edit, _| (edit.draft_changes().to_vec(), edit.session().history.undo_len(), edit.session().history.can_redo()));
    assert_eq!(before, after, "a frozen draft and its history are left as they were");
    // The keys do the same through the actions.
    visual.simulate_keystrokes("secondary-z");
    settle(visual);
    assert_eq!(edit.read_with(visual, |edit, _| edit.session().history.undo_len()), before.1);
}

#[gpui::test]
fn the_answer_of_an_apply_goes_to_the_editor_that_asked_not_to_the_tab_s_next_one(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    let release = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (log, released) = (ran.clone(), release.clone());
    app.update(visual, |this, _| {
        this.diagram.injected_exec = Some(std::sync::Arc::new(move |sql: &str, control: &crate::app::structure_edit::apply::ApplyControl| {
            log.lock().unwrap().push(sql.to_owned());
            while sql.starts_with("ALTER") && !released.load(std::sync::atomic::Ordering::SeqCst) {
                if control.is_cancelled() {
                    return Err("cancelled by the user".into());
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(())
        }));
    });
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    visual.simulate_keystrokes("escape");
    settle(visual);
    open_review(visual, "properties-edit-changes");
    diagram_click(visual, "changes-apply");
    wait_until(visual, "the apply to start", |_| ran.lock().unwrap().iter().any(|sql| sql.starts_with("ALTER TABLE")));
    let old = pe_edit(&app, visual, tab_id).unwrap();

    // While it runs the tab moves on to another table and starts its own edit mode.
    app.update(visual, |this, _| this.workspace.tab_mut(tab_id).unwrap().collection = "customers".into());
    settle(visual);
    assert!(pe_edit(&app, visual, tab_id).is_none(), "the draft of orders is dropped");
    pe_describe(&app, visual, tab_id, pe_described());
    pe_start(&app, visual, tab_id);
    let new = pe_edit(&app, visual, tab_id).unwrap();
    assert_ne!(old.entity_id(), new.entity_id());

    release.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_until(visual, "the apply to finish", |visual| app.read_with(visual, |this, _| this.diagram.applies.len() == 0));
    settle(visual);
    assert!(ran.lock().unwrap().iter().any(|sql| sql.contains("ALTER TABLE \"public\".\"orders\" ADD COLUMN")), "it ran on orders: {:?}", ran.lock().unwrap());
    assert!(new.read_with(visual, |edit, _| edit.review_report().is_none() && edit.render_state().notice.is_none() && edit.pending_count() == 0), "customers' editor heard nothing");
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().collection == "customers"));
}

// ---- the change list of the review dialog: a card that wraps is whole, in both list kinds ----

/// A pending column whose comment statement is far wider than the dialog (it wraps), then `more`
/// short ones. Added columns keep their order in the plan, so the wrapping card is the first.
fn pe_wrapping_changes(more: usize) -> Vec<Change> {
    let comment = "a comment that runs on and on so the COMMENT ON COLUMN statement of this change is far wider than the review dialog and has to wrap onto several lines ".repeat(2);
    let wide = ColumnSpec { comment: Some(comment), ..pe_spec("wide", "varchar(40)", true) };
    let mut changes = vec![Change::AddColumn { table: pe_orders(), column: wide }];
    changes.extend((0..more).map(|n| Change::AddColumn { table: pe_orders(), column: pe_spec(&format!("c{n}"), "integer", true) }));
    changes
}

/// What the dialog drew of a change list whose first card wraps.
struct PeCards {
    list: gpui::Bounds<gpui::Pixels>,
    first: gpui::Bounds<gpui::Pixels>,
    /// The wrapped statement of the first card, and a one-line statement of the second.
    wrapped: gpui::Bounds<gpui::Pixels>,
    line: gpui::Bounds<gpui::Pixels>,
    /// The last card, when it is drawn (a virtual list only builds the cards near its view).
    last: Option<gpui::Bounds<gpui::Pixels>>,
}

/// Opens the dialog over `changes` (at least two).
fn pe_card_geometry(cx: &mut TestAppContext, changes: Vec<Change>) -> PeCards {
    let count = changes.len();
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes, cx)));
    settle(visual);
    open_review(visual, "properties-edit-changes");
    PeCards {
        list: visual.debug_bounds("changes-list").expect("the change list"),
        first: visual.debug_bounds("changes-item-0").expect("the first card"),
        wrapped: visual.debug_bounds("changes-sql-0-1").expect("the comment statement of the first card"),
        line: visual.debug_bounds("changes-sql-1-0").expect("the one-line statement of the second card"),
        last: visual.debug_bounds(d3_sel(format!("changes-item-{}", count - 1))),
    }
}

fn pe_assert_whole(cards: &PeCards) {
    let PeCards { list, first, wrapped, line, .. } = cards;
    let slack = gpui::px(0.5);
    assert!(wrapped.size.height > line.size.height * 2.0, "the statement wraps at the dialog width: {:?} vs a line {:?}", wrapped.size.height, line.size.height);
    assert!(first.size.height >= wrapped.size.height, "the card holds its wrapped statement");
    assert!(list.size.height >= first.size.height, "the list ({:?}) is as tall as the wrapped card ({:?})", list.size.height, first.size.height);
    assert!(first.bottom() <= list.bottom() + slack && wrapped.bottom() <= list.bottom() + slack, "the card ends inside the list: {cards_first:?} in {list:?}", cards_first = first);
}

#[gpui::test]
fn a_wrapping_card_is_whole_in_the_plain_change_list(cx: &mut TestAppContext) {
    // Few changes: the cards are plain children of a scrolling div, as tall as they are up to 360px.
    let changes = pe_wrapping_changes(1);
    assert!(changes.len() < crate::app::structure_edit::VIRTUAL_FROM, "the plain path");
    let cards = pe_card_geometry(cx, changes);
    pe_assert_whole(&cards);
    let last = cards.last.expect("every card of a plain list is drawn");
    assert!(
        cards.list.size.height >= gpui::px(360.0) || last.bottom() <= cards.list.bottom() + gpui::px(0.5),
        "the list shows every card or is at its 360px limit: list {:?}, last card {last:?}",
        cards.list
    );
}

#[gpui::test]
fn a_wrapping_card_is_whole_in_the_virtual_change_list(cx: &mut TestAppContext) {
    let changes = pe_wrapping_changes(crate::app::structure_edit::VIRTUAL_FROM + 3);
    assert!(changes.len() >= crate::app::structure_edit::VIRTUAL_FROM, "the virtual path");
    let cards = pe_card_geometry(cx, changes);
    assert_eq!(cards.list.size.height, gpui::px(360.0), "a virtual list has its own fixed height, whatever its cards measure");
    pe_assert_whole(&cards);
    assert!(cards.last.is_none(), "only the cards near the view are built");
}

#[gpui::test]
fn a_card_is_never_shorter_than_the_virtual_list_assumes(cx: &mut TestAppContext) {
    use crate::sql_ddl::IndexSpec;
    // The shortest cards: one statement (an index, a drop), and a card whose change the dialect refuses
    // (an error line instead of a statement block, the smallest text a card carries).
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let changes = vec![
        Change::DropIndex { table: pe_orders(), name: "idx_old".into() },
        Change::CreateIndex { table: pe_orders(), index: IndexSpec { name: "idx_note".into(), columns: vec!["note".into()], unique: false } },
        Change::AddColumn { table: pe_orders(), column: pe_spec("bad", "not a type;", true) },
    ];
    edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes, cx)));
    settle(visual);
    open_review(visual, "properties-edit-changes");
    let heights: Vec<f32> = (0..3).map(|n| f32::from(visual.debug_bounds(d3_sel(format!("changes-item-{n}"))).expect("a card").size.height)).collect();
    let shortest = heights.iter().copied().fold(f32::MAX, f32::min);
    let assumed = crate::app::structure_edit::MIN_CARD_HEIGHT;
    assert!(visual.debug_bounds("changes-error-2").is_some(), "the refused change shows its error: {heights:?}");
    assert!(shortest >= assumed, "the shortest card is {shortest}px, the virtual list assumes {assumed}px: {heights:?}");
    assert!(shortest * crate::app::structure_edit::VIRTUAL_FROM as f32 >= 360.0, "VIRTUAL_FROM cards fill the list: {heights:?}");
}

// ---- the wheel over the change list, in a window the dialog does not fit ----

/// Opens the review dialog over `changes` in a window too short for it, so that the body of the
/// dialog (a `Scrollable`, whose `ScrollBounce` takes every wheel event while it overflows) scrolls
/// too, and wheels over the change list: the list goes first, and the dialog once the list is at
/// its end.
fn pe_wheel_over_the_change_list(cx: &mut TestAppContext, changes: Vec<Change>) {
    let count = changes.len();
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes, cx)));
    settle(visual);
    open_review(visual, "properties-edit-changes");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(440.0)));
    settle(visual);
    settle(visual);
    let bounds = |visual: &mut gpui::VisualTestContext, selector: &str| {
        visual.debug_bounds(d3_sel(selector.to_string())).unwrap_or_else(|| panic!("{selector} is drawn"))
    };
    // The first card against the top of the list: how far the list is scrolled from its start.
    let from_start = |visual: &mut gpui::VisualTestContext| bounds(visual, "changes-list").origin.y - bounds(visual, "changes-item-0").origin.y;
    // The last card against the bottom of the list: how far it is from its end.
    let last = format!("changes-item-{}", count - 1);
    let to_end = |visual: &mut gpui::VisualTestContext| bounds(visual, "changes-list").bottom() - bounds(visual, &last).bottom();
    let dialog = |visual: &mut gpui::VisualTestContext| bounds(visual, "changes-subject").origin.y;
    let wheel = |visual: &mut gpui::VisualTestContext, dy: f32| {
        let list = bounds(visual, "changes-list");
        visual.simulate_event(gpui::ScrollEvent {
            position: gpui::point(list.center().x, list.origin.y + gpui::px(60.0)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
            ..Default::default()
        });
        settle(visual);
    };
    let list = bounds(visual, "changes-list");
    assert!(list.size.height >= gpui::px(300.0), "a list with room to scroll: {list:?}");
    assert!(list.origin.y + gpui::px(60.0) < gpui::px(440.0), "the list is in view");
    let start_dialog = dialog(visual);
    assert_eq!(from_start(visual), gpui::px(0.0), "the list starts at its first card");

    wheel(visual, -60.0);
    assert!(from_start(visual) > gpui::px(0.0), "the list scrolled");
    assert_eq!(dialog(visual), start_dialog, "and the dialog stayed where it was");

    // A virtual list only knows the height of the cards it has drawn: it takes a few throws to reach the end.
    for _ in 0..6 {
        wheel(visual, -100_000.0);
        if visual.debug_bounds(d3_sel(last.clone())).is_some_and(|card| (bounds(visual, "changes-list").bottom() - card.bottom()).abs() < gpui::px(1.0)) {
            break;
        }
    }
    assert!(to_end(visual).abs() < gpui::px(1.0), "the list went to its end: {:?}", to_end(visual));
    assert_eq!(dialog(visual), start_dialog, "the dialog still did not move");
    wheel(visual, -60.0);
    assert!(to_end(visual).abs() < gpui::px(1.0), "the list is at its end");
    assert!(dialog(visual) < start_dialog, "so the dialog scrolls");
}

#[gpui::test]
fn the_wheel_scrolls_the_plain_change_list_before_the_dialog_around_it(cx: &mut TestAppContext) {
    let changes = pe_wrapping_changes(5);
    assert!(changes.len() < crate::app::structure_edit::VIRTUAL_FROM, "the plain path");
    pe_wheel_over_the_change_list(cx, changes);
}

#[gpui::test]
fn the_wheel_scrolls_the_virtual_change_list_before_the_dialog_around_it(cx: &mut TestAppContext) {
    let changes = pe_wrapping_changes(crate::app::structure_edit::VIRTUAL_FROM + 3);
    assert!(changes.len() >= crate::app::structure_edit::VIRTUAL_FROM, "the virtual path");
    pe_wheel_over_the_change_list(cx, changes);
}

// ---- undo and redo give back the exact changes, through the paths the edit mode uses ----

#[gpui::test]
fn undo_and_redo_restore_the_exact_changes_after_edits_that_share_them_and_a_reload(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let edit = pe_edit(&app, visual, tab_id).unwrap();
    let steps = vec![
        Change::RenameColumn { table: pe_orders(), from: "name".into(), to: "title".into() },
        Change::AlterColumn { table: pe_orders(), from: pe_spec("note", "text", true), to: pe_spec("note", "text", false) },
        Change::DropColumn { table: pe_orders(), column: "legacy".into() },
        Change::AddColumn { table: pe_orders(), column: pe_spec("region", "varchar(40)", true) },
    ];
    let mut seen = vec![pe_changes(&app, visual, tab_id)];
    for change in steps {
        edit.update(visual, |edit, cx| assert!(edit.push_for_test(change, cx)));
        seen.push(pe_changes(&app, visual, tab_id));
    }
    // The table is described again meanwhile (a refresh): the draft is checked against it, not replaced.
    pe_describe(&app, visual, tab_id, pe_described());
    assert_eq!(pe_changes(&app, visual, tab_id), *seen.last().unwrap());
    for expected in seen.iter().rev().skip(1) {
        diagram_click(visual, "properties-edit-undo");
        assert_eq!(pe_changes(&app, visual, tab_id), *expected);
    }
    for expected in seen.iter().skip(1).take(2) {
        diagram_click(visual, "properties-edit-redo");
        assert_eq!(pe_changes(&app, visual, tab_id), *expected);
    }
    // A new edit after two redos, then back: the steps still hold the very changes they had.
    let again = Change::AddColumn { table: pe_orders(), column: pe_spec("extra", "integer", true) };
    edit.update(visual, |edit, cx| assert!(edit.push_for_test(again, cx)));
    let with_extra = pe_changes(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-undo");
    assert_eq!(pe_changes(&app, visual, tab_id), seen[2]);
    diagram_click(visual, "properties-edit-undo");
    assert_eq!(pe_changes(&app, visual, tab_id), seen[1]);
    assert_ne!(with_extra, seen[2]);
}

fn pe_grid_focused(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) -> bool {
    let grid = app.update(visual, |this, cx| this.view_mut(tab_id).columns_focus(cx));
    visual.update(|window, _| grid.is_focused(window))
}

#[gpui::test]
fn a_drag_from_the_open_editor_to_another_cell_of_its_row_leaves_the_editor_open(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    pe_open_cell(&app, visual, tab_id, "note", Field::Name);
    pe_replace(visual, "not valid");
    let typed = pe_editor_text(&app, visual, tab_id);
    let position = pe_position(&app, visual, tab_id, "note");
    let editor = visual.debug_bounds("properties-edit-editor").expect("the editor is open");
    let over = visual.debug_bounds(d3_sel(format!("properties-edit-type-{position}"))).expect("the Type cell").center();
    let modifiers = gpui::Modifiers::default();
    visual.simulate_event(gpui::MouseDownEvent { position: editor.center(), modifiers, button: gpui::MouseButton::Left, click_count: 1, first_mouse: false });
    visual.simulate_event(gpui::MouseMoveEvent { position: over, pressed_button: Some(gpui::MouseButton::Left), modifiers });
    visual.simulate_event(gpui::MouseUpEvent { position: over, modifiers, button: gpui::MouseButton::Left, click_count: 1 });
    settle(visual);
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("note".to_owned(), Field::Name)), "the editor is still open");
    assert_eq!(pe_editor_text(&app, visual, tab_id), typed, "and its text is as it was");
}

#[gpui::test]
fn the_focus_returns_to_the_grid_when_the_real_controls_move_or_their_row_goes(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    let none = pe_tab_stops(visual);
    pe_select(&app, visual, tab_id, "name");
    let on_name = pe_tab_stops(visual);
    let controls: Vec<&String> = on_name.iter().filter(|stop| !none.contains(stop)).collect();
    assert_eq!(controls.len(), 3);
    // Tab to the real Nullable box, then Down twice: the selection moves twice, the grid has the focus.
    pe_focus(visual, controls[0]);
    assert!(!pe_grid_focused(&app, visual, tab_id));
    visual.simulate_keystrokes("down");
    settle(visual);
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("note"));
    assert!(pe_grid_focused(&app, visual, tab_id), "the focused box went with its row; the grid has the focus");
    visual.simulate_keystrokes("down");
    settle(visual);
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("legacy"));
    assert!(pe_grid_focused(&app, visual, tab_id));

    // Space on the real Remove button of a new row removes it: the grid has the focus and Up works.
    diagram_click(visual, "properties-edit-add-column");
    pe_select(&app, visual, tab_id, "column1");
    let before = pe_tab_stops(visual);
    pe_select(&app, visual, tab_id, "legacy");
    let on_legacy = pe_tab_stops(visual);
    pe_select(&app, visual, tab_id, "column1");
    let on_new = pe_tab_stops(visual);
    let new_controls: Vec<&String> = on_new.iter().filter(|stop| !on_legacy.contains(stop)).collect();
    assert!(before.len() >= 3 && new_controls.len() == 3, "{new_controls:?}");
    pe_focus(visual, new_controls[2]);
    pe_space(visual);
    assert!(pe_edit(&app, visual, tab_id).unwrap().read_with(visual, |edit, _| edit.rows().iter().all(|row| row.spec.name != "column1")), "Remove took the new column out");
    assert!(pe_grid_focused(&app, visual, tab_id), "the grid has the focus");
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("legacy"), "the neighbour is selected");
    visual.simulate_keystrokes("up");
    settle(visual);
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("note"), "Up still works");
}

#[gpui::test]
fn removing_a_new_row_by_click_does_not_write_its_open_editor_into_the_next_row(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    diagram_click(visual, "properties-edit-add-column");
    pe_select(&app, visual, tab_id, "column1");
    pe_open_cell(&app, visual, tab_id, "column1", Field::Name);
    pe_replace(visual, "zzz");
    assert_eq!(pe_editing(&app, visual, tab_id), Some(("column1".to_owned(), Field::Name)));
    let position = pe_position(&app, visual, tab_id, "column1");
    pe_click(visual, format!("properties-edit-remove-{position}"));
    let rows = pe_edit(&app, visual, tab_id).unwrap().read_with(visual, |edit, _| edit.rows().iter().map(|row| row.spec.name.clone()).collect::<Vec<_>>());
    assert_eq!(rows, ["id", "name", "note", "legacy", "column2"], "the neighbouring row keeps its name");
    assert!(pe_editing(&app, visual, tab_id).is_none());
    assert!(pe_changes(&app, visual, tab_id).iter().all(|change| !format!("{change:?}").contains("zzz")), "{:?}", pe_changes(&app, visual, tab_id));
}

#[gpui::test]
fn a_drawn_action_that_is_off_neither_acts_nor_selects(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tab_id) = pe_app(cx);
    let connection = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().connection_id.clone());
    publish_types(&app, visual, true);
    pe_start(&app, visual, tab_id);
    diagram_click(visual, "properties-edit-add-column");
    pe_select(&app, visual, tab_id, "name");
    give_profile(&app, visual, &connection, typed(PRODUCTION));
    assert!(!pe_edit(&app, visual, tab_id).unwrap().read_with(visual, |edit, _| edit.editable()));
    let pending = pe_pending(&app, visual, tab_id);
    let note = pe_position(&app, visual, tab_id, "note");
    pe_click(visual, format!("properties-edit-drop-{note}"));
    assert_eq!(pe_pending(&app, visual, tab_id), pending, "a frozen draft does not take the drop");
    assert_eq!(pe_selected(&app, visual, tab_id).as_deref(), Some("name"), "and the press is not a click on the row, as on the official disabled Button");
}

#[gpui::test]
fn a_drawn_click_acts_on_its_own_column_in_a_filtered_list_and_while_another_row_is_edited(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = pe_app(cx);
    pe_start(&app, visual, tab_id);
    // Filtered: position 0 is column `note`, index 2.
    let input = app.read_with(visual, |this, _| this.columns_filter_inputs.values().next().cloned().expect("filter input"));
    app.update_in(visual, |_, window, cx| input.update(cx, |state, cx| state.set_value("note", window, cx)));
    settle(visual);
    assert_eq!(pe_position(&app, visual, tab_id, "note"), 0);
    pe_click(visual, "properties-edit-nullable-0".into());
    assert_eq!(
        pe_changes(&app, visual, tab_id),
        [Change::AlterColumn { table: pe_orders(), from: pe_spec("note", "text", true), to: pe_spec("note", "text", false) }]
    );
    pe_select(&app, visual, tab_id, "note");
    pe_click(visual, "properties-edit-drop-0".into());
    assert_eq!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped);
    pe_click(visual, "properties-edit-restore-0".into());
    assert_ne!(pe_kind(&app, visual, tab_id, "note"), RowKind::Dropped);
    app.update_in(visual, |_, window, cx| input.update(cx, |state, cx| state.set_value("", window, cx)));
    settle(visual);

    // The editor is open on another row: the click commits it first and acts on its own row.
    pe_open_cell(&app, visual, tab_id, "name", Field::Name);
    pe_replace(visual, "name2");
    let legacy = pe_position(&app, visual, tab_id, "legacy");
    pe_click(visual, format!("properties-edit-nullable-{legacy}"));
    let changes = pe_changes(&app, visual, tab_id);
    assert!(changes.iter().any(|change| matches!(change, Change::RenameColumn { from, to, .. } if from == "name" && to == "name2")), "{changes:?}");
    assert!(changes.iter().any(|change| matches!(change, Change::AlterColumn { to, .. } if to.name == "legacy" && !to.nullable)), "{changes:?}");
    assert!(pe_editing(&app, visual, tab_id).is_none());
}
