// Pin / Unpin from the sidebar's row menus: the real right-click menu, the row moving to the top of
// its siblings with a mark, the pin kept in the app database and read again by a new window.

/// A fresh app database file for one test (the tests never use the user's data).
fn pins_db_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tablex-sidebar-pins-{}.db", uuid::Uuid::new_v4()))
}

fn remove_pins_db(path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

/// The bounds of a debug selector built at run time (the probe wants a `'static` name; a test
/// leaks a few bytes).
fn bounds_of(visual: &mut gpui::VisualTestContext, name: String) -> Option<gpui::Bounds<gpui::Pixels>> {
    visual.debug_bounds(Box::leak(name.into_boxed_str()))
}

/// Right-clicks the row of `id`, takes the menu's first item and confirms it.
fn choose_first_menu_item(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &NavigatorNodeId,
) {
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let index = app
        .read_with(visual, |this, _| this.navigator.visible_index(id))
        .expect("the row is visible");
    let row = bounds_of(visual, format!("sidebar-row-{index}")).expect("row bounds");
    visual.simulate_mouse_down(row.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(row.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    visual.run_until_parked();
}

/// Waits for the background writes of the pins to reach the database.
fn stored_pins(
    visual: &mut gpui::VisualTestContext,
    db: &std::path::Path,
    expected: usize,
) -> Vec<(String, String)> {
    for _ in 0..200 {
        visual.run_until_parked();
        let pins = engine_storage::pinned_nodes::list_all(db).unwrap();
        if pins.len() == expected {
            return pins;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    engine_storage::pinned_nodes::list_all(db).unwrap()
}

/// Labels of the visible rows that are children of `parent` (by depth, until the next row of its
/// depth or shallower).
fn sibling_labels(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    parent: &NavigatorNodeId,
) -> Vec<String> {
    app.read_with(visual, |this, _| {
        let rows = this.navigator.visible_rows();
        let Some(start) = rows.iter().position(|row| &row.id == parent) else {
            return Vec::new();
        };
        let depth = rows[start].depth;
        rows[start + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .filter(|row| row.depth == depth + 1)
            .map(|row| row.label.clone())
            .collect()
    })
}

fn marked_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    visual.update(|window, _| window.refresh());
    visual.run_until_parked();
    let count = app.read_with(visual, |this, _| this.navigator.visible_row_count());
    let labels = visible_labels(app, visual);
    (0..count)
        .filter(|index| bounds_of(visual, format!("sidebar-pin-{index}")).is_some())
        .map(|index| labels[index].clone())
        .collect()
}

fn postgres_tables_group() -> NavigatorNodeId {
    crate::navigator::object_group_id(
        crate::ui_fixture::POSTGRES_ID,
        "warehouse",
        "public",
        ObjectGroup::Tables,
    )
}

fn postgres_table(name: &str) -> NavigatorNodeId {
    NavigatorNodeId::Object {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: name.into(),
    }
}

fn postgres_root() -> NavigatorNodeId {
    NavigatorNodeId::Connection {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
    }
}

fn seeded_pins_app<'a>(
    cx: &'a mut TestAppContext,
    db: &std::path::Path,
) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext) {
    // The pins are written on a thread of the pool, which the test scheduler has to allow.
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    app.update(visual, |this, _| {
        this.sidebar.pins_db = Some(db.to_path_buf());
        this.seed_connection_tree_scenario();
    });
    (app, visual)
}

#[gpui::test]
fn a_table_pinned_from_its_menu_moves_to_the_top_with_a_mark_and_unpin_puts_it_back(
    cx: &mut TestAppContext,
) {
    let db = pins_db_path();
    let (app, visual) = seeded_pins_app(cx, &db);
    let usual = sibling_labels(&app, visual, &postgres_tables_group());
    assert_eq!(usual[0], "orders");
    assert_eq!(usual[3], "fixture_table_03");
    assert!(marked_rows(&app, visual).is_empty());

    // Pin the fourth table, then the third: pinned rows keep the order they were pinned in.
    choose_first_menu_item(&app, visual, &postgres_table("fixture_table_03"));
    choose_first_menu_item(&app, visual, &postgres_table("fixture_table_02"));
    let pinned = sibling_labels(&app, visual, &postgres_tables_group());
    assert_eq!(&pinned[..4], ["fixture_table_03", "fixture_table_02", "orders", "fixture_table_01"]);
    assert_eq!(pinned.len(), usual.len(), "no row is lost or duplicated");
    assert_eq!(marked_rows(&app, visual), ["fixture_table_03", "fixture_table_02"]);
    let key = postgres_table("fixture_table_03").pin_key().unwrap();
    let stored = stored_pins(visual, &db, 2);
    assert_eq!(stored[0].0, crate::connections::storage_connection_id(crate::ui_fixture::POSTGRES_ID));
    assert_eq!(stored[0].1, key);

    // Unpin puts a row back in its usual place; the other stays on top.
    choose_first_menu_item(&app, visual, &postgres_table("fixture_table_03"));
    let after = sibling_labels(&app, visual, &postgres_tables_group());
    assert_eq!(&after[..3], ["fixture_table_02", "orders", "fixture_table_01"]);
    assert_eq!(after[3], "fixture_table_03");
    assert_eq!(marked_rows(&app, visual), ["fixture_table_02"]);
    stored_pins(visual, &db, 1);
    choose_first_menu_item(&app, visual, &postgres_table("fixture_table_02"));
    assert_eq!(sibling_labels(&app, visual, &postgres_tables_group()), usual);
    assert!(marked_rows(&app, visual).is_empty());
    assert!(stored_pins(visual, &db, 0).is_empty());
    remove_pins_db(&db);
}

#[gpui::test]
fn a_pin_is_read_again_by_a_new_window(cx: &mut TestAppContext) {
    let db = pins_db_path();
    let (first, view) = seeded_pins_app(cx, &db);
    choose_first_menu_item(&first, view, &postgres_table("fixture_table_05"));
    stored_pins(view, &db, 1);

    // A restart: a new app over the same database reads the pin before the tree is built.
    let (second, view2) = sidebar_test_app(cx);
    second.update(view2, |this, cx| {
        this.sidebar.pins_db = Some(db.clone());
        this.load_node_pins(cx);
    });
    for _ in 0..200 {
        view2.run_until_parked();
        if second.read_with(view2, |this, _| this.navigator.is_pinned(&postgres_table("fixture_table_05"))) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    second.update(view2, |this, _| this.seed_connection_tree_scenario());
    let labels = sibling_labels(&second, view2, &postgres_tables_group());
    assert_eq!(&labels[..2], ["fixture_table_05", "orders"]);
    assert_eq!(marked_rows(&second, view2), ["fixture_table_05"]);
    remove_pins_db(&db);
}

#[gpui::test]
fn a_database_pinned_from_its_menu_moves_to_the_top_of_its_connection(cx: &mut TestAppContext) {
    let db = pins_db_path();
    let (app, visual) = seeded_pins_app(cx, &db);
    // Collapse the schema so the databases are next to each other.
    app.update(visual, |this, _| {
        let warehouse = NavigatorNodeId::Database {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
        };
        this.navigator.toggle_expansion(&warehouse);
    });
    let usual = sibling_labels(&app, visual, &postgres_root());
    assert_eq!(usual, ["warehouse", "reporting"]);
    let reporting = NavigatorNodeId::Database {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "reporting".into(),
    };
    choose_first_menu_item(&app, visual, &reporting);
    assert_eq!(
        sibling_labels(&app, visual, &postgres_root()),
        ["reporting", "warehouse"]
    );
    assert_eq!(marked_rows(&app, visual), ["reporting"]);
    assert_eq!(stored_pins(visual, &db, 1).len(), 1);
    choose_first_menu_item(&app, visual, &reporting);
    assert_eq!(sibling_labels(&app, visual, &postgres_root()), usual);
    assert!(marked_rows(&app, visual).is_empty());
    remove_pins_db(&db);
}

#[gpui::test]
fn a_collection_pinned_from_its_menu_moves_to_the_top_of_its_database(cx: &mut TestAppContext) {
    let db = pins_db_path();
    let (app, visual) = seeded_pins_app(cx, &db);
    // Fold the SQL connection away so the collections are on screen.
    app.update(visual, |this, _| {
        this.navigator.toggle_expansion(&postgres_root());
    });
    let mongo = NavigatorNodeId::Connection {
        connection: crate::ui_fixture::MONGO_ID.into(),
    };
    let database = app.read_with(visual, |this, _| {
        this.navigator.find_provider_child(&mongo, "database", "fixture_db_0", None)
    });
    let database = database.expect("the first Mongo database is listed");
    let usual = sibling_labels(&app, visual, &database);
    assert_eq!(usual[2], "fixture_collection_02");
    let collection = app
        .read_with(visual, |this, _| {
            this.navigator.find_provider_child(
                &database,
                "collection",
                "fixture_db_0",
                Some("fixture_collection_02"),
            )
        })
        .expect("the collection is listed");
    choose_first_menu_item(&app, visual, &collection);
    let pinned = sibling_labels(&app, visual, &database);
    assert_eq!(&pinned[..3], ["fixture_collection_02", "fixture_collection_00", "fixture_collection_01"]);
    assert_eq!(marked_rows(&app, visual), ["fixture_collection_02"]);
    stored_pins(visual, &db, 1);
    choose_first_menu_item(&app, visual, &collection);
    assert_eq!(sibling_labels(&app, visual, &database), usual);
    assert!(marked_rows(&app, visual).is_empty());
    remove_pins_db(&db);
}

#[gpui::test]
fn a_connection_pinned_from_its_menu_moves_to_the_top_with_a_mark(cx: &mut TestAppContext) {
    // Opening a connection's menu reads its query history on a thread of the pool.
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    let connections = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<String> {
        app.read_with(visual, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .filter(|row| row.depth == 0)
                .map(|row| row.label)
                .collect()
        })
    };
    let id = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &str| {
        app.read_with(visual, |this, _| {
            this.navigator
                .visible_rows()
                .into_iter()
                .find(|row| row.label == label)
                .unwrap()
                .id
        })
    };
    // Nothing is pinned: the connections are in their usual order.
    let usual = connections(&app, visual);
    assert_eq!(usual, ["Mongo fixture", "Postgres fixture", "SQLite fixture"]);
    assert!(marked_rows(&app, visual).is_empty());
    let sqlite = id(&app, visual, "SQLite fixture");
    choose_first_menu_item(&app, visual, &sqlite);
    assert_eq!(connections(&app, visual), ["SQLite fixture", "Mongo fixture", "Postgres fixture"]);
    assert_eq!(marked_rows(&app, visual), ["SQLite fixture"]);
    // A second pin follows the first, in the usual order among the pinned.
    let mongo = id(&app, visual, "Mongo fixture");
    choose_first_menu_item(&app, visual, &mongo);
    assert_eq!(connections(&app, visual), ["Mongo fixture", "SQLite fixture", "Postgres fixture"]);
    assert_eq!(marked_rows(&app, visual), ["Mongo fixture", "SQLite fixture"]);
    choose_first_menu_item(&app, visual, &sqlite);
    choose_first_menu_item(&app, visual, &mongo);
    assert_eq!(connections(&app, visual), usual);
    assert!(marked_rows(&app, visual).is_empty());
}

#[gpui::test]
fn the_same_name_in_two_databases_is_pinned_independently(cx: &mut TestAppContext) {
    let db = pins_db_path();
    let (app, visual) = seeded_pins_app(cx, &db);
    let orders = |connection: &str, database: &str| NavigatorNodeId::Object {
        connection: connection.into(),
        database: database.into(),
        schema: "public".into(),
        object: "orders".into(),
    };
    let (a, b) = (
        orders(crate::ui_fixture::POSTGRES_ID, "warehouse"),
        orders(crate::ui_fixture::POSTGRES_ID, "analytics"),
    );
    let elsewhere = orders(crate::ui_fixture::MONGO_ID, "warehouse");
    app.update(visual, |this, cx| {
        this.toggle_node_pin(&a, cx);
        assert!(this.navigator.is_pinned(&a));
        assert!(!this.navigator.is_pinned(&b), "another database");
        assert!(!this.navigator.is_pinned(&elsewhere), "another connection");
        this.toggle_node_pin(&b, cx);
        this.toggle_node_pin(&a, cx);
        assert!(!this.navigator.is_pinned(&a));
        assert!(this.navigator.is_pinned(&b));
    });
    // The three writes land one after the other, in the order the pins were made.
    for _ in 0..200 {
        visual.run_until_parked();
        if engine_storage::pinned_nodes::list_all(&db).unwrap().first().is_some_and(|pin| Some(&pin.1) == b.pin_key().as_ref()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let stored = stored_pins(visual, &db, 1);
    assert_eq!(stored[0].1, b.pin_key().unwrap());
    assert_ne!(a.pin_key(), b.pin_key());
    // The stored key reads back as the same row, and only that one.
    assert_eq!(NavigatorNodeId::from_pin_key(crate::ui_fixture::POSTGRES_ID, &stored[0].1), Some(b));
    remove_pins_db(&db);
}

#[gpui::test]
fn a_pin_of_a_row_that_no_longer_exists_is_ignored(cx: &mut TestAppContext) {
    let db = pins_db_path();
    let (app, visual) = seeded_pins_app(cx, &db);
    let before = visible_labels(&app, visual);
    app.update(visual, |this, _| {
        this.navigator.set_node_pins([
            postgres_table("dropped_table"),
            NavigatorNodeId::Database {
                connection: "11111111-1111-4111-8111-111111111111".into(),
                database: "gone".into(),
            },
        ]);
    });
    assert_eq!(visible_labels(&app, visual), before);
    assert!(marked_rows(&app, visual).is_empty());
    // A key this build cannot read is dropped on load, not an error.
    assert_eq!(NavigatorNodeId::from_pin_key("c", "not json"), None);
    assert_eq!(NavigatorNodeId::from_pin_key("c", r#"["unknown","x"]"#), None);
    remove_pins_db(&db);
}

#[gpui::test]
fn a_stored_connection_pin_is_kept_by_the_store_and_read_again_by_a_new_window(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let record = |name: &str| {
        let id = uuid::Uuid::new_v4();
        engine_storage::connection::SavedConnectionRecord {
            id: id.to_string().to_ascii_uppercase(),
            name: name.to_string(),
            uri: "sqlite:///tmp/pins-sample.db".to_string(),
            db_type: crate::connections::DB_TYPE_SQLITE.to_string(),
            color_hex: "#34C759".to_string(),
            save_password: false,
            save_proxy_password: false,
            network_settings_json: "{}".to_string(),
            is_pinned: false,
            last_connected: None,
            created_at: 1.0,
            updated_at: 1.0,
            connection_type: String::new(),
            overrides_json: String::new(),
        }
    };
    let (alpha, zulu) = (record("Pins sample alpha"), record("Pins sample zulu"));
    for record in [&alpha, &zulu] {
        engine_storage::connection::upsert_connection(record).unwrap();
    }
    let order = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| -> Vec<String> {
        visible_labels(app, visual)
            .into_iter()
            .filter(|label| label.starts_with("Pins sample"))
            .collect()
    };
    fn open(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
        let (app, visual) = sidebar_test_app(cx);
        app.update(visual, |this, cx| {
            this.scenario_ui.service = None;
            this.refresh_saved_connections(cx);
        });
        (app, visual)
    }
    let (app, visual) = open(cx);
    assert_eq!(order(&app, visual), ["Pins sample alpha", "Pins sample zulu"]);
    let zulu_id = NavigatorNodeId::Connection {
        connection: zulu.id.to_ascii_lowercase(),
    };
    choose_first_menu_item(&app, visual, &zulu_id);
    assert_eq!(order(&app, visual), ["Pins sample zulu", "Pins sample alpha"]);
    assert!(engine_storage::connection::get_connection(&zulu.id).unwrap().unwrap().is_pinned);
    assert!(marked_rows(&app, visual).contains(&"Pins sample zulu".to_string()));

    // A restart: a new window reads the stored pin.
    let (second, view2) = open(cx);
    assert_eq!(order(&second, view2), ["Pins sample zulu", "Pins sample alpha"]);
    choose_first_menu_item(&second, view2, &zulu_id);
    assert_eq!(order(&second, view2), ["Pins sample alpha", "Pins sample zulu"]);
    assert!(!engine_storage::connection::get_connection(&zulu.id).unwrap().unwrap().is_pinned);
    for record in [&alpha, &zulu] {
        engine_storage::connection::delete_connection(&record.id).unwrap();
    }
}
