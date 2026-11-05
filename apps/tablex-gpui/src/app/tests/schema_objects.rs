// P6a: the schema objects of a database (functions, procedures, sequences, triggers,
// types, events, materialized views) in the navigator, and the read-only source tabs
// they open. The SQLite tests use the REAL SQLite driver behind a real host session and
// click the real sidebar rows; the fixture tests walk the Postgres fixture tree; the
// live tests at the end (`#[ignore]`d, dedicated `tablex-test-*` containers only) create
// every kind of object on PostgreSQL / MySQL / MariaDB and read them back.

const OBJECTS_SEED: &str = "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT); \
     CREATE TABLE audit(item INTEGER); \
     CREATE VIEW item_names AS SELECT name FROM items; \
     CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN INSERT INTO audit(item) VALUES (NEW.id); END; \
     CREATE TRIGGER items_gone AFTER DELETE ON items BEGIN DELETE FROM audit WHERE item = OLD.id; END;";

impl SqlRig<'_> {
    /// The loaded row of `group` that says `label` (a row's identity is the driver's node id,
    /// which the test does not spell out).
    fn schema_object(&self, group: &NavigatorNodeId, label: &str) -> NavigatorNodeId {
        self.app.read_with(self.visual, |this, _| {
            this.navigator
                .loaded_children(group)
                .unwrap_or_default()
                .iter()
                .find(|id| {
                    this.navigator
                        .visible_index(id)
                        .and_then(|at| this.navigator.visible_row_at(at))
                        .is_some_and(|row| row.label == label)
                })
                .cloned()
                .unwrap_or_else(|| panic!("{label} is a loaded row of {group:?}"))
        })
    }
}

/// The source tab on screen: (tab id, status, fetched text, editor text).
fn active_source(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> Option<(u64, source_tab::SourceStatus, String, Option<String>)> {
    app.read_with(visual, |this, cx| {
        let tab = this.workspace.active_tab().filter(|tab| tab.kind == TabKind::Source)?;
        let state = this.source_tabs.get(&tab.id)?;
        Some((
            tab.id,
            state.status.clone(),
            state.text.clone(),
            state.editor.as_ref().map(|editor| editor.read(cx).value().to_string()),
        ))
    })
}

fn source_tabs_open(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| {
        this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Source).count()
    })
}

/// Waits until the active tab is a source tab that has its text.
fn wait_source_ready(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> String {
    let app_for_wait = app.clone();
    wait_until(visual, "the source tab to load", |visual| {
        matches!(
            active_source(&app_for_wait, visual),
            Some((_, source_tab::SourceStatus::Ready, _, Some(_)))
        )
    });
    settle(visual);
    active_source(app, visual).expect("a source tab").2
}

impl SqlRig<'_> {
    fn double_click_node(&mut self, id: &NavigatorNodeId) {
        settle(self.visual);
        let index = self.node_index(id).unwrap_or_else(|| panic!("{id:?} is a visible row"));
        let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
        let center = self.visual.debug_bounds(selector).expect("row bounds").center();
        double_click(self.visual, center);
    }

    fn source(&mut self) -> String {
        wait_source_ready(&self.app.clone(), self.visual)
    }
}

#[gpui::test]
fn the_triggers_group_loads_when_opened_and_a_double_click_opens_the_trigger_source(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");

    // The schema shows the groups SQLite has; only the group that was opened asked the driver.
    assert_eq!(
        rig.tree_children(&schema_node(&key, "main", "main")),
        ["Tables", "Views", "Triggers"]
    );
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.navigator.load_state(&triggers).is_none(), "an unopened group costs no query");
        assert_eq!(this.navigator.loading_count(), 0);
    });
    rig.expand(&triggers);
    assert_eq!(rig.tree_children(&triggers), ["items_ai on items", "items_gone on items"]);

    // A double click opens the trigger's source: read-only, highlighted, the server's text.
    let ai = rig.schema_object(&triggers, "items_ai on items");
    rig.double_click_node(&ai);
    let text = rig.source();
    assert!(text.starts_with("CREATE TRIGGER items_ai AFTER INSERT ON items"), "{text}");
    assert!(text.contains("INSERT INTO audit(item) VALUES (NEW.id)") && text.ends_with("END;"), "{text}");
    let (tab_id, _, _, editor) = active_source(&rig.app, rig.visual).unwrap();
    assert_eq!(editor.as_deref(), Some(text.as_str()), "the editor shows the fetched text");
    rig.app.read_with(rig.visual, |this, cx| {
        let tab = this.workspace.tab(tab_id).unwrap();
        assert_eq!(tab.title, "items_ai on items");
        assert_eq!(tab.connection_id, key);
        let source = tab.source.as_ref().unwrap();
        assert_eq!(
            (source.kind, source.name.as_str(), source.table.as_deref()),
            (engine_driver_api::sql::SchemaObjectKind::Trigger, "items_ai", Some("items"))
        );
        let editor = this.source_tabs[&tab_id].editor.as_ref().unwrap().read(cx);
        assert_eq!(editor.language_name().as_ref(), "sql", "tree-sitter SQL highlighting");
        assert!(!editor.is_editable(), "read-only");
    });
    assert!(rig.visual.debug_bounds("source-editor").is_some());
    assert!(rig.visual.debug_bounds("source-tab").is_some());

    // Typing into the read-only editor changes nothing.
    let editor_bounds = rig.visual.debug_bounds("source-editor").unwrap();
    rig.visual.simulate_click(editor_bounds.center(), Default::default());
    rig.visual.simulate_input("zzz");
    settle(rig.visual);
    assert_eq!(active_source(&rig.app, rig.visual).unwrap().3.as_deref(), Some(text.as_str()));

    // Copy puts the whole statement on the clipboard.
    rig.click("source-copy");
    settle(rig.visual);
    assert_eq!(rig.visual.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some(text.as_str()));

    // Opening the same object again reuses its tab.
    rig.double_click_node(&ai);
    assert_eq!(source_tabs_open(&rig.app, rig.visual), 1);
    let gone = rig.schema_object(&triggers, "items_gone on items");
    rig.double_click_node(&gone);
    assert_eq!(source_tabs_open(&rig.app, rig.visual), 2, "another trigger, another tab");
    assert!(rig.source().starts_with("CREATE TRIGGER items_gone AFTER DELETE ON items"));

    // Open in SQL Editor: a new query tab on the same connection holds the text.
    rig.double_click_node(&ai);
    let text = rig.source();
    rig.click("source-open-in-editor");
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!(tab.sql, text);
        assert_eq!(tab.connection_id, key);
        assert_eq!(tab.database, "main");
        assert_eq!(this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::Source).count(), 2);
    });
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn the_context_menu_opens_the_source_of_a_trigger(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);
    rig.expand(&triggers);
    let gone = rig.schema_object(&triggers, "items_gone on items");
    let index = rig.node_index(&gone).expect("the trigger row");
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let center = rig.visual.debug_bounds(selector).expect("row bounds").center();
    rig.visual.simulate_mouse_down(center, gpui::MouseButton::Right, Default::default());
    rig.visual.simulate_mouse_up(center, gpui::MouseButton::Right, Default::default());
    rig.visual.run_until_parked();
    // The menu has one entry, View Source.
    rig.visual.simulate_keystrokes("down");
    rig.visual.simulate_keystrokes("enter");
    rig.visual.run_until_parked();
    let text = rig.source();
    assert!(text.starts_with("CREATE TRIGGER items_gone"), "{text}");
    assert_eq!(source_tabs_open(&rig.app, rig.visual), 1);
}

#[gpui::test]
fn a_restored_source_tab_fetches_the_source_again_and_the_session_holds_none_of_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);
    rig.expand(&triggers);
    let ai = rig.schema_object(&triggers, "items_ai on items");
    rig.double_click_node(&ai);
    assert!(rig.source().contains("INSERT INTO audit(item) VALUES (NEW.id)"));

    // What the session saves: the object, never its text.
    let session = rig.app.read_with(rig.visual, |this, _| this.workspace.snapshot());
    let saved = serde_json::to_string(&session).unwrap();
    assert!(saved.contains("\"kind\":\"source\""), "{saved}");
    assert!(saved.contains("\"name\":\"items_ai\"") && saved.contains("\"table\":\"items\""), "{saved}");
    assert!(!saved.contains("INSERT INTO audit"), "the source text is not persisted: {saved}");

    // The trigger changes while the app is closed; the relaunch shows what the database has now.
    db.sql("DROP TRIGGER items_ai; CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN SELECT 1; END;");
    rig.app.update(rig.visual, |this, cx| {
        this.source_tabs.clear();
        this.workspace.restore_snapshot(&session, |_| true);
        this.reset_active_view();
        cx.notify();
    });
    settle(rig.visual);
    let text = rig.source();
    assert!(text.contains("SELECT 1") && !text.contains("INSERT INTO audit"), "{text}");
    assert_eq!(source_tabs_open(&rig.app, rig.visual), 1);

    // Refresh fetches once more.
    db.sql("DROP TRIGGER items_ai; CREATE TRIGGER items_ai AFTER INSERT ON items BEGIN SELECT 2; END;");
    rig.click("source-refresh");
    let app = rig.app.clone();
    wait_until(rig.visual, "the refreshed source", |visual| {
        active_source(&app, visual).is_some_and(|(_, status, text, _)| {
            status == source_tab::SourceStatus::Ready && text.contains("SELECT 2")
        })
    });
    settle(rig.visual);
    assert!(active_source(&rig.app, rig.visual).unwrap().3.unwrap().contains("SELECT 2"), "the editor follows");
}

#[gpui::test]
fn a_driver_that_advertises_no_schema_object_kinds_shows_tables_and_views_only(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    // The driver of an older release: databases, tables, views and columns, no triggers.
    rig.app.update(rig.visual, |this, _| {
        let mut capabilities = (**this.driver_runtime.capabilities.get(&key).unwrap()).clone();
        capabilities.navigator_kinds = ["database", "table", "view", "column"].map(String::from).into();
        this.driver_runtime.capabilities.insert(key.clone(), std::sync::Arc::new(capabilities));
        this.sync_navigator_kinds(&key);
    });
    let tables = rig.expand_to_tables("main", "main");
    assert_eq!(rig.tree_children(&schema_node(&key, "main", "main")), ["Tables", "Views"]);
    assert_eq!(rig.tree_children(&tables), ["audit", "items"]);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.navigator.load_state(&group_node(&key, "main", "main", ObjectGroup::Triggers)).is_none());
        assert_eq!(this.navigator.loading_count(), 0, "no listing was issued for a group the driver cannot answer");
    });
    assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
}

#[gpui::test]
fn refresh_connects_a_source_tab_whose_connection_was_down(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);
    rig.expand(&triggers);
    let ai = rig.schema_object(&triggers, "items_ai on items");
    rig.double_click_node(&ai);
    assert!(rig.source().starts_with("CREATE TRIGGER items_ai"));

    // The session is gone (a lost connection, or a relaunch that could not connect): the tab
    // is rebuilt without its state and cannot reach the server.
    let session = rig.app.update(rig.visual, |this, _| {
        let session = this.driver_runtime.sessions.remove(&key).expect("a session");
        this.source_tabs.clear();
        session
    });
    settle(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the failed connect", |visual| {
        matches!(active_source(&app, visual), Some((_, source_tab::SourceStatus::Failed(_), ..)))
    });

    // The connection is available again: Refresh goes through the connect path and loads.
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.sessions.insert(key.clone(), session);
    });
    rig.click("source-refresh");
    let text = rig.source();
    assert!(text.starts_with("CREATE TRIGGER items_ai"), "{text}");

    // And with no session at all, Refresh starts the connect again instead of staying failed.
    rig.app.update(rig.visual, |this, _| {
        this.driver_runtime.sessions.remove(&key);
    });
    rig.click("source-refresh");
    settle(rig.visual);
    let state = rig.app.read_with(rig.visual, |this, _| this.source_tabs.values().next().map(|state| state.status.clone()));
    assert!(matches!(state, Some(source_tab::SourceStatus::Failed(_) | source_tab::SourceStatus::Connecting { .. })), "{state:?}");
}

#[gpui::test]
fn mariadb_sequences_keep_their_names_in_the_structure_catalog(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    let id = uuid::Uuid::new_v4();
    let key = id.to_string();
    app.update(visual, |this, _| {
        let mut saved = this.saved_connections.clone();
        saved.push(SavedConnection {
            id,
            name: "shop".into(),
            uri: "mariadb://localhost/app".into(),
            kind: ConnectionKind::Mariadb,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        this.saved_connections = saved.clone();
        this.navigator.set_connections(saved);
        let nav = &mut this.navigator;
        let load = |nav: &mut NavigatorState, parent: NavigatorNodeId, children: Vec<crate::navigator::LoadedChild>| {
            let request = nav.toggle_expansion(&parent).request.expect("a load");
            let generation = nav.load_generation(&request).unwrap();
            nav.apply_load(&request, generation, Ok(children));
        };
        load(nav, connection_node(&key), vec![crate::navigator::database_child(&key, "app")]);
        load(nav, database_node(&key, "app"), vec![crate::navigator::schema_child(&key, "app", "app")]);
        nav.toggle_expansion(&schema_node(&key, "app", "app"));
        let object = |name: &str, kind| crate::navigator::object_child(&key, "app", "app", name, kind);
        load(
            nav,
            group_node(&key, "app", "app", ObjectGroup::Tables),
            vec![object("orders", KindOfObject::Table)],
        );
        load(
            nav,
            group_node(&key, "app", "app", ObjectGroup::Sequences),
            vec![object("ticket_seq", KindOfObject::Sequence)],
        );
        let names = this.structure_catalog_names(&key, "app", Some("app")).expect("loaded names");
        assert_eq!(names.as_slice(), ["orders", "ticket_seq"], "a new table cannot take a sequence's name");
    });
}

#[gpui::test]
fn a_vanished_object_shows_the_driver_error_in_its_tab(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    rig.expand_to_tables("main", "main");
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);
    rig.expand(&triggers);
    db.sql("DROP TRIGGER items_gone");
    let gone = rig.schema_object(&triggers, "items_gone on items");
    rig.double_click_node(&gone);
    let app = rig.app.clone();
    wait_until(rig.visual, "the failed fetch", |visual| {
        matches!(active_source(&app, visual), Some((_, source_tab::SourceStatus::Failed(_), _, _)))
    });
    settle(rig.visual);
    let Some((_, source_tab::SourceStatus::Failed(error), ..)) = active_source(&rig.app, rig.visual) else {
        panic!("the fetch failed")
    };
    assert!(error.contains("describe.not_found"), "{error}");
    assert!(rig.visual.debug_bounds("source-note").is_some(), "the tab says so");
    // Nothing to copy or open, but Refresh is there for the retry.
    rig.click("source-copy");
    assert!(rig.visual.read_from_clipboard().and_then(|item| item.text()).is_none_or(|text| text.is_empty()));
    assert!(rig.visual.debug_bounds("source-refresh").is_some());
    assert!(rig.log().iter().any(|(code, ..)| code.as_deref() == Some("describe.not_found")), "{:?}", rig.log());
}

#[gpui::test]
fn a_remote_search_never_loads_the_groups_that_are_not_open_and_local_search_finds_loaded_ones(cx: &mut TestAppContext) {
    let db = SqliteDb::new(OBJECTS_SEED);
    let mut rig = nav_rig_sqlite(cx, &db);
    let key = rig.key.clone();
    let tables = rig.expand_to_tables("main", "main");
    assert_eq!(rig.tree_children(&tables), ["audit", "items"]);
    let triggers = group_node(&key, "main", "main", ObjectGroup::Triggers);

    rig.app.update(rig.visual, |this, cx| this.set_sidebar_local_search(false, cx));
    search_query(&rig.app, rig.visual, "names");
    settle_search(rig.visual);
    let app = rig.app.clone();
    wait_until(rig.visual, "the remote search", |visual| {
        app.read_with(visual, |this, _| !this.sidebar.remote.loading)
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| {
        assert!(this.navigator.load_state(&triggers).is_none(), "the search did not load the Triggers group");
    });
    assert!(rig.labels().contains(&"item_names".to_string()), "{:?}", rig.labels());
    search_query(&rig.app, rig.visual, "");
    settle_search(rig.visual);
    rig.app.update(rig.visual, |this, cx| this.set_sidebar_local_search(true, cx));
    settle(rig.visual);

    // Once the group is open, the sidebar search matches its objects like any other row.
    rig.expand(&triggers);
    search_query(&rig.app, rig.visual, "gone");
    settle_search(rig.visual);
    let labels = rig.labels();
    assert!(labels.contains(&"items_gone on items".to_string()), "{labels:?}");
    assert!(!labels.contains(&"items_ai on items".to_string()), "{labels:?}");
}

/// The Postgres fixture tree opened to its public schema (every Postgres group exists).
fn fixture_postgres_schema(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    let (app, visual) = ns_app(cx);
    app.update(visual, |this, _| this.seed_connection_tree_scenario());
    settle(visual);
    (app, visual)
}

fn fixture_group(group: ObjectGroup) -> NavigatorNodeId {
    crate::navigator::object_group_id(crate::ui_fixture::POSTGRES_ID, "warehouse", "public", group)
}

fn fixture_children(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, group: ObjectGroup) -> Vec<String> {
    app.update(visual, |this, _| {
        this.apply_scenario_expansion(&fixture_group(group));
        let children = this.navigator.loaded_children(&fixture_group(group)).unwrap_or_default().to_vec();
        children
            .iter()
            .filter_map(|id| this.navigator.visible_index(id).and_then(|at| this.navigator.visible_row_at(at)))
            .map(|row| row.label)
            .collect()
    })
}

#[gpui::test]
fn the_postgres_tree_lists_every_group_with_overloads_told_apart_and_opens_their_sources(cx: &mut TestAppContext) {
    let (app, visual) = fixture_postgres_schema(cx);
    let public = NavigatorNodeId::Schema {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
    };
    let groups = app.read_with(visual, |this, _| {
        this.navigator.loaded_children(&public).unwrap().iter().filter_map(|id| match id {
            NavigatorNodeId::ObjectGroup { group, .. } => Some(group.title()),
            _ => None,
        }).collect::<Vec<_>>()
    });
    assert_eq!(
        groups,
        ["Tables", "Views", "Materialized Views", "Functions", "Procedures", "Sequences", "Triggers", "Types"]
    );
    assert_eq!(
        fixture_children(&app, visual, ObjectGroup::Functions),
        ["add_tax(numeric, numeric)", "order_total(bigint)", "order_total(bigint, date)", "touch_updated_at()"],
        "overloads of order_total are two rows, each with its argument types"
    );
    assert_eq!(fixture_children(&app, visual, ObjectGroup::Procedures), ["archive_orders(date)"]);
    assert_eq!(fixture_children(&app, visual, ObjectGroup::Sequences), ["orders_id_seq", "invoice_no_seq"]);
    assert_eq!(fixture_children(&app, visual, ObjectGroup::Triggers), ["orders_touch on orders", "orders_audit on orders"]);
    assert_eq!(fixture_children(&app, visual, ObjectGroup::Types), ["order_status", "money_pair", "positive_int"]);
    assert_eq!(fixture_children(&app, visual, ObjectGroup::MaterializedViews), ["monthly_revenue"]);

    // Each overload opens its own tab, the reference carries the signature.
    let first = crate::navigator::schema_object_id(crate::ui_fixture::POSTGRES_ID, "warehouse", "public", ObjectGroup::Functions, "order_total(bigint)");
    let second = crate::navigator::schema_object_id(crate::ui_fixture::POSTGRES_ID, "warehouse", "public", ObjectGroup::Functions, "order_total(bigint, date)");
    for (id, label) in [(&first, "order_total(bigint)"), (&second, "order_total(bigint, date)")] {
        app.update(visual, |this, cx| this.open_schema_object_source(id, label, cx));
        let text = wait_source_ready(&app, visual);
        assert!(text.contains(&format!("-- order_total({})", label.trim_start_matches("order_total(").trim_end_matches(')'))), "{text}");
    }
    assert_eq!(source_tabs_open(&app, visual), 2);
    app.read_with(visual, |this, _| {
        let arguments: Vec<_> = this
            .workspace
            .tabs()
            .iter()
            .filter_map(|tab| tab.source.as_ref().and_then(|source| source.arguments.clone()))
            .collect();
        assert_eq!(arguments, ["bigint", "bigint, date"]);
    });
}

#[gpui::test]
fn a_materialized_view_opens_its_data_and_offers_view_source(cx: &mut TestAppContext) {
    let (app, visual) = fixture_postgres_schema(cx);
    assert_eq!(fixture_children(&app, visual, ObjectGroup::MaterializedViews), ["monthly_revenue"]);
    let fixture_object = |object: &str| NavigatorNodeId::Object {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
        object: object.into(),
    };
    let view = fixture_object("monthly_revenue");
    app.read_with(visual, |this, _| {
        assert_eq!(this.navigator.object_kind(&view), Some(KindOfObject::MaterializedView));
        assert!(this.navigator.available_actions(&view, true).contains(&crate::navigator::NavigatorAction::ViewSource));
        let table = fixture_object("orders");
        assert!(!this.navigator.available_actions(&table, true).contains(&crate::navigator::NavigatorAction::ViewSource));
    });
    app.update(visual, |this, cx| this.open_schema_object_source(&view, "monthly_revenue", cx));
    let text = wait_source_ready(&app, visual);
    assert!(text.starts_with("CREATE MATERIALIZED VIEW public.monthly_revenue AS"), "{text}");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.source.as_ref().unwrap().kind, engine_driver_api::sql::SchemaObjectKind::MaterializedView);
    });
    // A table has no source tab to open.
    let orders = fixture_object("orders");
    app.update(visual, |this, cx| this.open_schema_object_source(&orders, "orders", cx));
    assert_eq!(source_tabs_open(&app, visual), 1);
}

#[gpui::test]
fn a_group_that_failed_to_load_shows_the_triangle_and_retry_reloads_it(cx: &mut TestAppContext) {
    let (app, visual) = fixture_postgres_schema(cx);
    let functions = fixture_group(ObjectGroup::Functions);
    app.update(visual, |this, cx| {
        // The 20 sample tables push the group below the first screenful: close them.
        this.navigator.toggle_expansion(&fixture_group(ObjectGroup::Tables));
        let request = this.navigator.toggle_expansion(&functions).request.expect("the group's load");
        let generation = this.navigator.load_generation(&request).unwrap();
        this.navigator.apply_load(&request, generation, Err("permission denied for schema public".into()));
        this.navigator_ui.generation = this.navigator.generation();
        cx.notify();
    });
    settle(visual);
    let index = ns_index(&app, visual, "Functions");
    assert!(ns_has(visual, format!("sidebar-problem-{index}")), "the same triangle on a group");
    let tables = ns_index(&app, visual, "Tables");
    assert!(!ns_has(visual, format!("sidebar-problem-{tables}")), "only the failed group");
    ns_click(visual, format!("sidebar-problem-{index}"));
    assert!(ns_has(visual, format!("sidebar-problem-popover-{index}")));
    ns_click(visual, format!("sidebar-problem-retry-{index}"));
    let state = app.update(visual, |this, _| this.navigator.load_state(&functions).cloned());
    assert_eq!(state, Some(crate::navigator::NodeLoadState::Loading), "Retry reloads the group");
}

#[test]
fn a_source_tab_round_trips_through_the_session_and_is_found_again() {
    use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};
    let reference = SchemaObjectRef {
        database: Some("app".into()),
        schema: Some("public".into()),
        kind: SchemaObjectKind::Function,
        name: "add".into(),
        arguments: Some("integer, integer".into()),
        table: None,
        identity: Some("23 23".into()),
    };
    let mut workspace = WorkspaceState::new();
    let first = workspace.open_or_activate_source_tab("conn", reference.clone(), "add(integer, integer)");
    let again = workspace.open_or_activate_source_tab("conn", reference.clone(), "add(integer, integer)");
    let other = SchemaObjectRef { arguments: Some("text, text".into()), ..reference.clone() };
    let overload = workspace.open_or_activate_source_tab("conn", other.clone(), "add(text, text)");
    assert_eq!(first, again, "the same object, the same tab");
    assert_ne!(first, overload, "an overload is another object");
    let elsewhere = workspace.open_or_activate_source_tab("other-conn", reference.clone(), "add(integer, integer)");
    assert_ne!(first, elsewhere, "another connection, another tab");

    let session = workspace.snapshot();
    let saved: Vec<_> = session.tabs.iter().filter(|tab| tab.kind == "source").collect();
    assert_eq!(saved.len(), 3);
    assert_eq!(saved[0].source.as_ref(), Some(&reference));
    assert_eq!((saved[0].database.as_str(), saved[0].schema.as_deref()), ("app", Some("public")));
    let json = serde_json::to_string(&session).unwrap();
    let back: crate::gpui_session::GpuiSession = serde_json::from_str(&json).unwrap();
    let mut restored = WorkspaceState::new();
    restored.restore_snapshot(&back, |_| true);
    let sources: Vec<_> = restored.tabs().iter().filter(|tab| tab.kind == TabKind::Source).collect();
    assert_eq!(sources.len(), 3);
    assert_eq!(sources[1].source.as_ref(), Some(&other));
    assert_eq!(sources[0].title, "add(integer, integer)");
    // A source tab saved without its object has nothing to show: it is dropped.
    let mut broken = back.clone();
    broken.tabs.iter_mut().filter(|tab| tab.kind == "source").for_each(|tab| tab.source = None);
    let mut restored = WorkspaceState::new();
    restored.restore_snapshot(&broken, |_| true);
    assert!(restored.tabs().iter().all(|tab| tab.kind != TabKind::Source));
    // And an older session file (no `source` key at all) still loads.
    let old = r#"{"tabs":[{"kind":"query","title":"Query"}]}"#;
    assert!(serde_json::from_str::<crate::gpui_session::GpuiSession>(old).is_ok());
}

// ------------------------------------------------------------------ live servers

/// Runs `statements` as another account of the same server (root for MySQL / MariaDB, whose
/// test account may not create triggers, events or functions under binary logging).
fn live_admin_sql(rig: &SqlRig<'_>, uri: &str, statements: &[String]) {
    let mut session = rig
        .handle
        .connect(&serde_json::json!({ "uri": uri }).to_string())
        .expect("the admin account connects");
    for sql in statements {
        let operation = session.begin_operation().expect("operation");
        let request = serde_json::json!({
            "kind": "sql", "operation_id": operation.id(), "database": null, "object": null,
            "offset": 0, "limit": 100, "payload": { "sql": sql, "confirmed": true }
        });
        let page = session
            .open_query(&request.to_string())
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        let _ = session.release_page(page.page_id);
    }
    let _ = session.disconnect();
}

/// Opens the source of `id` with a double click and returns its text.
fn live_open_source(rig: &mut SqlRig<'_>, id: &NavigatorNodeId) -> String {
    rig.double_click_node(id);
    rig.source()
}

fn live_unique() -> String {
    uuid::Uuid::new_v4().to_simple().to_string()[..8].to_string()
}

/// Walks to the schema node `schema` of `database` and returns it.
fn live_walk_to_schema(rig: &mut SqlRig<'_>, database: &str, schema: &str) -> NavigatorNodeId {
    let key = rig.key.clone();
    rig.click_node(&connection_node(&key), "sidebar-row");
    rig.wait_children(&connection_node(&key));
    rig.expand(&database_node(&key, database));
    let schema_id = schema_node(&key, database, schema);
    rig.expand(&schema_id);
    schema_id
}

fn live_postgres_objects(cx: &mut TestAppContext) {
    let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
        eprintln!("skipped: set TABLEX_TEST_PG_URI to a dedicated tablex-test-* container URI");
        return;
    };
    let database = uri.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let scratch = format!("p6a_{}", live_unique());
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &uri);
    let key = rig.key.clone();
    let setup = [
        format!("CREATE SCHEMA {scratch}"),
        format!("CREATE TABLE {scratch}.orders (id int PRIMARY KEY, total int)"),
        format!("CREATE FUNCTION {scratch}.add(a int, b int) RETURNS int LANGUAGE sql AS 'SELECT a + b'"),
        format!("CREATE FUNCTION {scratch}.add(a text, b text) RETURNS text LANGUAGE sql AS 'SELECT a || b'"),
        format!("CREATE PROCEDURE {scratch}.touch(n int) LANGUAGE sql AS 'SELECT n'"),
        format!("CREATE FUNCTION {scratch}.audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$"),
        format!("CREATE TRIGGER orders_audit BEFORE INSERT ON {scratch}.orders FOR EACH ROW EXECUTE FUNCTION {scratch}.audit()"),
        format!("CREATE SEQUENCE {scratch}.ticket_seq AS integer INCREMENT BY 5 START WITH 100 MINVALUE 100 MAXVALUE 1000 CACHE 3 CYCLE"),
        format!("CREATE MATERIALIZED VIEW {scratch}.totals AS SELECT total FROM {scratch}.orders"),
        format!("CREATE TYPE {scratch}.mood AS ENUM ('sad', 'it''s ok', 'happy')"),
    ];
    for sql in &setup {
        run_confirmed(&mut rig, sql);
    }
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let schema = live_walk_to_schema(&mut rig, &database, &scratch);
        let groups = rig.tree_children(&schema);
        assert_eq!(
            groups,
            ["Tables", "Views", "Materialized Views", "Functions", "Procedures", "Sequences", "Triggers", "Types"]
        );
        let listed = |rig: &mut SqlRig<'_>, group: ObjectGroup| {
            let id = group_node(&key, &database, &scratch, group);
            rig.expand(&id);
            rig.tree_children(&id)
        };
        assert_eq!(listed(&mut rig, ObjectGroup::Tables), ["orders"]);
        assert_eq!(listed(&mut rig, ObjectGroup::Views), Vec::<String>::new());
        assert_eq!(listed(&mut rig, ObjectGroup::MaterializedViews), ["totals"]);
        // Two functions of one name are two rows with their signatures; the trigger function is one too.
        assert_eq!(
            listed(&mut rig, ObjectGroup::Functions),
            ["add(integer, integer)", "add(text, text)", "audit()"]
        );
        assert_eq!(listed(&mut rig, ObjectGroup::Procedures), ["touch(integer)"]);
        assert_eq!(listed(&mut rig, ObjectGroup::Sequences), ["ticket_seq"]);
        assert_eq!(listed(&mut rig, ObjectGroup::Triggers), ["orders_audit on orders"]);
        assert_eq!(listed(&mut rig, ObjectGroup::Types), ["mood"]);

        let open = |rig: &mut SqlRig<'_>, group, label: &str| {
            let group = group_node(&key, &database, &scratch, group);
            let id = rig.schema_object(&group, label);
            live_open_source(rig, &id)
        };
        let ints = open(&mut rig, ObjectGroup::Functions, "add(integer, integer)");
        assert!(ints.contains("a integer, b integer") && ints.contains("SELECT a + b") && ints.ends_with(';'), "{ints}");
        let texts = open(&mut rig, ObjectGroup::Functions, "add(text, text)");
        assert!(texts.contains("a text, b text") && texts.contains("a || b"), "{texts}");
        assert!(open(&mut rig, ObjectGroup::Procedures, "touch(integer)").contains("PROCEDURE"));
        let trigger = open(&mut rig, ObjectGroup::Triggers, "orders_audit on orders");
        assert!(trigger.starts_with("CREATE TRIGGER orders_audit BEFORE INSERT ON"), "{trigger}");
        let sequence = open(&mut rig, ObjectGroup::Sequences, "ticket_seq");
        assert!(sequence.contains("AS integer INCREMENT BY 5 MINVALUE 100 MAXVALUE 1000 START WITH 100 CACHE 3 CYCLE;"), "{sequence}");
        let mood = open(&mut rig, ObjectGroup::Types, "mood");
        assert!(mood.ends_with("AS ENUM ('sad', 'it''s ok', 'happy');"), "{mood}");
        // The materialized view is a table-like row: its menu entry opens the definition.
        let matview = object_node(&key, &database, &scratch, "totals");
        rig.app.update(rig.visual, |this, cx| this.open_schema_object_source(&matview, "totals", cx));
        let definition = rig.source();
        assert!(definition.starts_with("CREATE MATERIALIZED VIEW") && definition.ends_with("WITH DATA;"), "{definition}");
        assert_eq!(source_tabs_open(&rig.app, rig.visual), 7);

        // Open in SQL Editor hands the function to a query tab that can run it again.
        open(&mut rig, ObjectGroup::Functions, "add(integer, integer)");
        let text = rig.source();
        rig.click("source-open-in-editor");
        settle(rig.visual);
        rig.app.read_with(rig.visual, |this, _| {
            let tab = this.workspace.active_tab().unwrap();
            assert_eq!((tab.kind, tab.sql.as_str()), (TabKind::Query, text.as_str()));
            assert_eq!(tab.schema.as_deref(), Some(scratch.as_str()));
        });
        assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    }));
    run_confirmed(&mut rig, &format!("DROP SCHEMA IF EXISTS {scratch} CASCADE"));
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn live_family_objects(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
    root_env: &str,
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let (scheme, rest) = uri.split_once("://").unwrap();
    let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host).to_string();
    let database = host.rsplit('/').next().unwrap().split('?').next().unwrap().to_string();
    let user = rest.split(':').next().unwrap().to_string();
    let root_uri = std::env::var(root_env).unwrap_or_else(|_| format!("{scheme}://root:tablex@{host}"));
    let mariadb = kind == ConnectionKind::Mariadb;
    let tag = live_unique();
    let (table, function, procedure, trigger, event, sequence) = (
        format!("p6a_t_{tag}"),
        format!("p6a_f_{tag}"),
        format!("p6a_p_{tag}"),
        format!("p6a_trg_{tag}"),
        format!("p6a_ev_{tag}"),
        format!("p6a_seq_{tag}"),
    );
    let mut rig = nav_rig(cx, kind, driver, &uri);
    let key = rig.key.clone();
    let definer = format!("`{user}`@`%`");
    let mut setup = vec![
        format!("CREATE TABLE `{database}`.`{table}` (id INT PRIMARY KEY, qty INT)"),
        format!("CREATE DEFINER = {definer} FUNCTION `{database}`.`{function}`(a INT) RETURNS INT DETERMINISTIC RETURN a + 1"),
        format!("CREATE DEFINER = {definer} PROCEDURE `{database}`.`{procedure}`(IN n INT) SELECT n"),
        format!("CREATE TRIGGER `{database}`.`{trigger}` BEFORE INSERT ON `{database}`.`{table}` FOR EACH ROW SET NEW.qty = IFNULL(NEW.qty, 0)"),
        format!("CREATE EVENT `{database}`.`{event}` ON SCHEDULE EVERY 1 DAY STARTS CURRENT_TIMESTAMP + INTERVAL 1 DAY DO SELECT 1"),
    ];
    if mariadb {
        setup.push(format!("CREATE SEQUENCE `{database}`.`{sequence}` START WITH 7 INCREMENT BY 2"));
    }
    live_admin_sql(&rig, &root_uri, &setup);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // The schema level of MySQL / MariaDB is the database itself.
        let schema = live_walk_to_schema(&mut rig, &database, &database);
        let mut expected = vec!["Tables", "Views", "Functions", "Procedures"];
        if mariadb {
            expected.push("Sequences");
        }
        expected.extend(["Triggers", "Events"]);
        assert_eq!(rig.tree_children(&schema), expected);
        let ours = |rig: &mut SqlRig<'_>, group: ObjectGroup, name: &str| {
            let id = group_node(&key, &database, &database, group);
            rig.expand(&id);
            rig.tree_children(&id).into_iter().filter(|label| label.starts_with(name)).collect::<Vec<_>>()
        };
        let open = |rig: &mut SqlRig<'_>, group, label: &str| {
            let group = group_node(&key, &database, &database, group);
            let id = rig.schema_object(&group, label);
            live_open_source(rig, &id)
        };
        assert_eq!(ours(&mut rig, ObjectGroup::Tables, &table), [table.as_str()]);
        let function_label = ours(&mut rig, ObjectGroup::Functions, &function)[0].clone();
        let function_text = open(&mut rig, ObjectGroup::Functions, &function_label);
        assert!(function_text.contains("FUNCTION") && function_text.contains(&function) && function_text.ends_with(';'), "{function_text}");
        assert_eq!(ours(&mut rig, ObjectGroup::Procedures, &procedure), [procedure.as_str()]);
        assert!(open(&mut rig, ObjectGroup::Procedures, &procedure).contains("PROCEDURE"));
        let trigger_label = format!("{trigger} on {table}");
        assert_eq!(ours(&mut rig, ObjectGroup::Triggers, &trigger), [trigger_label.as_str()]);
        assert!(open(&mut rig, ObjectGroup::Triggers, &trigger_label).contains("IFNULL(NEW.qty, 0)"));
        assert_eq!(ours(&mut rig, ObjectGroup::Events, &event), [event.as_str()]);
        assert!(open(&mut rig, ObjectGroup::Events, &event).contains("CREATE"));
        if mariadb {
            // A sequence is a row that opens as data, like a table, and also has a source.
            assert_eq!(ours(&mut rig, ObjectGroup::Sequences, &sequence), [sequence.as_str()]);
            let row = object_node(&key, &database, &database, &sequence);
            rig.app.read_with(rig.visual, |this, _| {
                assert_eq!(this.navigator.object_kind(&row), Some(KindOfObject::Sequence));
                assert!(this.navigator.available_actions(&row, true).contains(&crate::navigator::NavigatorAction::ViewSource));
            });
            rig.double_click_node(&row);
            let app = rig.app.clone();
            wait_until(rig.visual, "the sequence's data", |visual| {
                app.read_with(visual, |this, _| {
                    let tab = this.workspace.active_tab().unwrap();
                    tab.collection == sequence
                        && matches!(this.workspace.runtime(tab.id).unwrap().query.status(), crate::query::QueryStatus::Ready { .. })
                })
            });
            rig.app.read_with(rig.visual, |this, _| {
                assert!(this.workspace.active_tab().unwrap().query_mode.is_table_data(), "opened as data");
            });
            rig.app.update(rig.visual, |this, cx| this.open_schema_object_source(&row, &sequence, cx));
            let text = rig.source();
            assert!(text.contains("SEQUENCE") && text.contains("start with 7"), "{text}");
            assert!(
                !rig.tree_children(&group_node(&key, &database, &database, ObjectGroup::Tables)).contains(&sequence),
                "a sequence is not also a table"
            );
        }
        rig.click("source-open-in-editor");
        settle(rig.visual);
        rig.app.read_with(rig.visual, |this, _| {
            assert_eq!(this.workspace.active_tab().unwrap().kind, TabKind::Query);
        });
        assert!(rig.navigator_errors().is_empty(), "{:?}", rig.navigator_errors());
    }));
    let mut drops = vec![
        format!("DROP EVENT IF EXISTS `{database}`.`{event}`"),
        format!("DROP TRIGGER IF EXISTS `{database}`.`{trigger}`"),
        format!("DROP PROCEDURE IF EXISTS `{database}`.`{procedure}`"),
        format!("DROP FUNCTION IF EXISTS `{database}`.`{function}`"),
        format!("DROP TABLE IF EXISTS `{database}`.`{table}`"),
    ];
    if mariadb {
        drops.push(format!("DROP SEQUENCE IF EXISTS `{database}`.`{sequence}`"));
    }
    live_admin_sql(&rig, &root_uri, &drops);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_schema_objects_list_and_show_their_source(cx: &mut TestAppContext) {
    live_postgres_objects(cx);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_schema_objects_list_and_show_their_source(cx: &mut TestAppContext) {
    live_family_objects(cx, ConnectionKind::Mysql, ("dbengine-mysql", "abi-v2"), "TABLEX_TEST_MYSQL_URI", "TABLEX_TEST_MYSQL_ROOT_URI");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_schema_objects_list_and_show_their_source(cx: &mut TestAppContext) {
    live_family_objects(cx, ConnectionKind::Mariadb, ("dbengine-mariadb", "abi-v2"), "TABLEX_TEST_MARIADB_URI", "TABLEX_TEST_MARIADB_ROOT_URI");
}
