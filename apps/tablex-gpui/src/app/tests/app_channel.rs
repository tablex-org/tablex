// The app's end of the channel from `tablex-mcp`: a request goes through the real handler (and,
// for one test, the real socket) and ends in the same tabs, editor text and Run path the user's own
// clicks use. A temporary data dir and socket; never a running TableX.

use engine_connect::app_channel::{AppClient, ClientError, Reply, Request};

fn channel_request(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    request: Request,
) -> Reply {
    app.update(visual, |this, cx| this.handle_channel_request(request, cx))
}

fn show(connection: &str, database: Option<&str>, text: &str, run: bool) -> Request {
    Request::ShowQuery {
        connection_id: connection.into(),
        database: database.map(str::to_owned),
        text: text.into(),
        run,
    }
}

fn channel_error(reply: &Reply) -> String {
    assert!(!reply.ok, "{reply:?}");
    reply.error.clone().expect("an error says why")
}

/// A saved SQLite connection that is not in the fixture, as the driver tests make one.
fn push_sqlite_connection(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> String {
    app.update(visual, |this, _| {
        this.scenario_ui.service = None;
        let id = uuid::Uuid::new_v4();
        this.saved_connections.push(SavedConnection {
            id,
            name: "shop".into(),
            uri: format!("sqlite://{}/shop-{id}.db", std::env::temp_dir().display()),
            kind: ConnectionKind::Sqlite,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        });
        id.to_string()
    })
}

fn tab_status(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab: u64) -> crate::query::QueryStatus {
    app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().query.status().clone())
}

#[gpui::test]
fn open_object_reveals_the_table_in_the_sidebar_and_opens_its_data_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let reply = channel_request(
        &app,
        visual,
        Request::OpenObject {
            connection_id: crate::ui_fixture::POSTGRES_ID.into(),
            database: Some("warehouse".into()),
            schema: Some("public".into()),
            object: "orders".into(),
        },
    );
    assert!(reply.ok, "{reply:?}");
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
        assert!(matches!(this.navigator.selected(), Some(NavigatorNodeId::Object { database, schema, object, .. })
            if database == "warehouse" && schema == "public" && object == "orders"));
        let tab = this.workspace.active_tab().expect("a tab opened");
        assert!(tab.query_mode.is_table_data(), "{:?}", tab.query_mode);
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert_eq!((tab.database.as_str(), tab.schema.as_deref(), tab.collection.as_str()), ("warehouse", Some("public"), "orders"));
    });
    // Asking again activates the tab that is there instead of opening a second one.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    assert!(channel_request(
        &app,
        visual,
        Request::OpenObject {
            connection_id: crate::ui_fixture::POSTGRES_ID.into(),
            database: Some("warehouse".into()),
            schema: Some("public".into()),
            object: "orders".into(),
        },
    )
    .ok);
    visual.run_until_parked();
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), tabs);
}

#[gpui::test]
fn open_object_opens_a_mongodb_collection(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let reply = channel_request(
        &app,
        visual,
        Request::OpenObject {
            connection_id: crate::ui_fixture::MONGO_ID.into(),
            database: Some("fixture_db_0".into()),
            schema: None,
            object: "fixture_collection_01".into(),
        },
    );
    assert!(reply.ok, "{reply:?}");
    visual.run_until_parked();
    app.read_with(visual, |this, _| {
        assert!(this.sidebar.locate_error.is_none(), "{:?}", this.sidebar.locate_error);
        let tab = this.workspace.active_tab().expect("a tab opened");
        assert_eq!(tab.query_mode, Some(QueryMode::MongoFind));
        assert_eq!((tab.database.as_str(), tab.collection.as_str()), ("fixture_db_0", "fixture_collection_01"));
    });
}

#[gpui::test]
fn open_object_refuses_what_it_cannot_open(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let open = |connection: &str, database: Option<&str>, schema: Option<&str>| Request::OpenObject {
        connection_id: connection.into(),
        database: database.map(str::to_owned),
        schema: schema.map(str::to_owned),
        object: "orders".into(),
    };
    for (request, expected) in [
        (open("not-an-id", Some("warehouse"), Some("public")), "no saved connection"),
        (open("00000000-0000-4000-8000-000000000000", Some("warehouse"), Some("public")), "no saved connection"),
        (open(crate::ui_fixture::POSTGRES_ID, None, Some("public")), "`database` is required"),
        (open(crate::ui_fixture::POSTGRES_ID, Some("warehouse"), None), "`schema` is required"),
    ] {
        let message = channel_error(&channel_request(&app, visual, request));
        assert!(message.contains(expected), "{message}");
    }
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), before);
}

#[gpui::test]
fn show_query_opens_a_query_tab_with_the_text_and_does_not_run_it(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let reply = channel_request(
        &app,
        visual,
        show(crate::ui_fixture::POSTGRES_ID, Some("warehouse"), "SELECT id, total\nFROM orders", false),
    );
    assert!(reply.ok, "{reply:?}");
    settle(visual);
    let (tab, editor) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().expect("a tab opened");
        assert_eq!(tab.kind, TabKind::Query);
        assert_eq!(tab.connection_id, crate::ui_fixture::POSTGRES_ID);
        assert_eq!(tab.database, "warehouse");
        assert_eq!(tab.sql, "SELECT id, total\nFROM orders");
        (tab.id, this.query_editors.by_tab[&tab.id].clone())
    });
    assert_eq!(
        app.read_with(visual, |_, cx| editor.read(cx).value().to_string()),
        "SELECT id, total\nFROM orders",
        "the editor shows it"
    );
    assert_eq!(tab_status(&app, visual, tab), crate::query::QueryStatus::Idle, "run false does not run");
    // Two asks are two tabs: the first one is not overwritten.
    assert!(channel_request(&app, visual, show(crate::ui_fixture::POSTGRES_ID, None, "SELECT 2", false)).ok);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(tab).unwrap().sql, "SELECT id, total\nFROM orders");
        assert_eq!(this.workspace.active_tab().unwrap().sql, "SELECT 2");
    });
}

#[gpui::test]
fn show_query_on_mongodb_opens_a_script_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let reply = channel_request(
        &app,
        visual,
        show(crate::ui_fixture::MONGO_ID, Some("fixture_db_0"), "db.fixture_collection_01.find({})", false),
    );
    assert!(reply.ok, "{reply:?}");
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.query_mode, Some(QueryMode::MongoScript));
        assert_eq!(tab.database, "fixture_db_0");
        assert_eq!(tab.sql, "db.fixture_collection_01.find({})");
    });
}

#[gpui::test]
fn show_query_refuses_an_unknown_connection_and_opens_no_tab(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    for id in ["nope", "00000000-0000-4000-8000-000000000000"] {
        let message = channel_error(&channel_request(&app, visual, show(id, None, "SELECT 1", true)));
        assert!(message.contains("no saved connection"), "{message}");
    }
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), before);
}

#[gpui::test]
fn show_query_with_run_goes_through_the_run_button_and_a_production_connection_still_asks(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let id = push_sqlite_connection(&app, visual);
    app.update(visual, |this, cx| {
        this.connection_policy.set_profile(&id, typed(PRODUCTION));
        cx.notify();
    });
    settle(visual);
    let sql = "DELETE FROM items WHERE id = 1";
    let reply = channel_request(&app, visual, show(&id, None, sql, true));
    assert!(reply.ok, "{reply:?}");
    let mut asked = false;
    for _ in 0..200 {
        settle(visual);
        if visual.debug_bounds("execute-confirm-run").is_some() {
            asked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(asked, "the production connection asks the user in the app");
    assert!(visual.debug_bounds("execute-confirm-statement").is_some(), "the statement is shown");
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    assert_eq!(tab_status(&app, visual, tab), crate::query::QueryStatus::Idle, "nothing ran before the answer");
    // Cancel: the statement never runs, and the text stays in the editor for the user.
    click_debug_button(visual, "execute-confirm-cancel");
    settle(visual);
    assert!(visual.debug_bounds("execute-confirm-run").is_none());
    assert_eq!(tab_status(&app, visual, tab), crate::query::QueryStatus::Idle);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tab(tab).unwrap().sql.clone()), sql);
}

#[gpui::test]
fn show_query_with_run_on_a_read_only_connection_is_refused_by_the_app(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let id = push_sqlite_connection(&app, visual);
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.connection_types.iter_mut().find(|ty| ty.id == PRODUCTION).unwrap().read_only = true;
        this.settings = std::sync::Arc::new(settings);
        this.connection_policy.set_profile(&id, typed(PRODUCTION));
        cx.notify();
    });
    settle(visual);
    assert!(channel_request(&app, visual, show(&id, None, "DROP TABLE items", true)).ok);
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    // The app's own refusal, in the tab, and no question: the rule is applied, not bypassed.
    match tab_status(&app, visual, tab) {
        crate::query::QueryStatus::Failed(message) => assert!(message.contains("DROP"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(visual.debug_bounds("execute-confirm-run").is_none());
}

#[gpui::test]
fn show_query_with_run_starts_the_normal_run_on_an_unrestricted_connection(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    cx.update(|cx| cx.set_reduce_motion(true));
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let id = push_sqlite_connection(&app, visual);
    assert!(channel_request(&app, visual, show(&id, None, "SELECT 1", true)).ok);
    settle(visual);
    let tab = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().id);
    assert!(visual.debug_bounds("execute-confirm-run").is_none(), "no confirmation without a reason");
    // There is no driver in this test: the run was started and ended on the normal path.
    assert_ne!(tab_status(&app, visual, tab), crate::query::QueryStatus::Idle);
}

#[gpui::test]
fn connections_changed_reloads_the_saved_connections_from_the_store(cx: &mut TestAppContext) {
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    // A scenario has no store to reload.
    let message = channel_error(&channel_request(&app, visual, Request::ConnectionsChanged {}));
    assert!(message.contains("UI scenario"), "{message}");
    app.update(visual, |this, _| this.scenario_ui.service = None);
    let id = uuid::Uuid::new_v4().to_string();
    let record = engine_storage::connection::SavedConnectionRecord {
        id: id.clone(),
        name: "shop reports".into(),
        uri: "sqlite:///tmp/shop-reports.db".into(),
        db_type: "sqlite".into(),
        color_hex: "#34C759".into(),
        save_password: false,
        save_proxy_password: false,
        network_settings_json: "{}".into(),
        is_pinned: false,
        last_connected: None,
        created_at: 1.0,
        updated_at: 1.0,
        connection_type: String::new(),
        overrides_json: String::new(),
    };
    let listed = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, _| this.saved_connections.iter().any(|saved| saved.matches_id(&id)))
    };
    assert!(!listed(&app, visual));
    // Another process (the MCP server) adds it to the store: nothing changes until it says so.
    engine_storage::connection::upsert_connection(&record).unwrap();
    assert!(!listed(&app, visual));
    assert!(channel_request(&app, visual, Request::ConnectionsChanged {}).ok);
    assert!(listed(&app, visual), "the new connection is in the app");
    assert!(app.read_with(visual, |this, _| this.navigator.connection(&id).is_some()), "and in the sidebar");
    // A rename and a removal arrive the same way.
    engine_storage::connection::upsert_connection(&engine_storage::connection::SavedConnectionRecord {
        name: "shop reports 2".into(),
        ..record
    })
    .unwrap();
    assert!(channel_request(&app, visual, Request::ConnectionsChanged {}).ok);
    assert!(app.read_with(visual, |this, _| this
        .saved_connections
        .iter()
        .any(|saved| saved.matches_id(&id) && saved.name == "shop reports 2")));
    engine_storage::connection::delete_connection(&id).unwrap();
    assert!(channel_request(&app, visual, Request::ConnectionsChanged {}).ok);
    assert!(!listed(&app, visual), "the removed connection is gone");
}

#[gpui::test]
fn the_socket_hands_a_request_to_the_ui_and_the_reply_comes_back(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    settle(visual);
    let dir = tempfile::Builder::new().prefix("tx").tempdir().unwrap();
    let path = dir.path().join("app.sock");
    app.update(visual, |this, cx| {
        this.listen_on_app_channel(&path, cx).expect("the socket binds");
    });
    let client = AppClient::new(path.clone());
    let send = |request: Request| {
        let client = client.clone();
        std::thread::spawn(move || client.send(&request))
    };
    // The client waits on its own thread; the UI answers when it runs.
    let pump = |visual: &mut gpui::VisualTestContext, thread: std::thread::JoinHandle<Result<Reply, ClientError>>| {
        for _ in 0..2000 {
            visual.run_until_parked();
            if thread.is_finished() {
                return thread.join().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the app never answered");
    };
    let thread = send(show(crate::ui_fixture::POSTGRES_ID, Some("warehouse"), "SELECT 42", false));
    assert!(pump(visual, thread).unwrap().ok);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.connection_id.as_str(), tab.sql.as_str()), (crate::ui_fixture::POSTGRES_ID, "SELECT 42"));
    });
    // A refusal travels back as the app's own words.
    let thread = send(show("nope", None, "SELECT 1", false));
    match pump(visual, thread) {
        Err(ClientError::Refused(message)) => assert!(message.contains("no saved connection"), "{message}"),
        other => panic!("{other:?}"),
    }
    // The socket file goes away with the app.
    app.update(visual, |this, _| this.app_channel.stop());
    assert!(!path.exists());
}
