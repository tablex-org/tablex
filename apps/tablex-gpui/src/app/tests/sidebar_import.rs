// Real-interaction tests of the Import / Export menu importers: click the menu
// row, answer the file picker, work the selection dialog, confirm or cancel.
// They run against an in-memory import store, so neither the user's saved
// connections nor the keychain is touched, and read only the fixtures.

fn import_fixture(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/connection_import/fixtures")
        .join(relative)
}

fn import_test_app(
    cx: &mut TestAppContext,
) -> (
    gpui::Entity<AppRoot>,
    &mut gpui::VisualTestContext,
    Arc<crate::connection_import::store::MemoryImportStore>,
) {
    // The file reads run on real threads (`smol::unblock`).
    cx.dispatcher.allow_parking();
    let (app, visual) = sidebar_test_app(cx);
    // Dialogs slide in with a wall-clock animation; with motion reduced they
    // settle on their first frame (gpui-component's own dialog tests do the
    // same), so a click never aims at a control that is still moving.
    visual.update(|_, cx| cx.set_reduce_motion(true));
    let store = Arc::new(crate::connection_import::store::MemoryImportStore::default());
    app.update(visual, |this, _| this.import_store = store.clone());
    (app, visual, store)
}

/// Lets background reads and the window catch up. The reads run on real
/// threads (`smol::unblock`), so the budget is a wall-clock deadline.
#[track_caller]
fn import_wait(visual: &mut gpui::VisualTestContext, mut done: impl FnMut(&mut gpui::VisualTestContext) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        if done(visual) {
            return;
        }
        visual.cx.executor().advance_clock(Duration::from_millis(50));
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("import step did not finish within 60s");
}

/// Menu row -> file picker answered with `paths`.
fn import_pick(visual: &mut gpui::VisualTestContext, item: &'static str, paths: Vec<std::path::PathBuf>) {
    click_debug_button(visual, "sidebar-import-export");
    click_debug_button(visual, item);
    assert!(visual.did_prompt_for_paths(), "the menu row asks the platform for files");
    visual.simulate_path_prompt_response(move |_| Some(paths.clone()));
}

fn import_selection_dialog(visual: &mut gpui::VisualTestContext) {
    import_wait(visual, |v| v.debug_bounds("import-selection-list").is_some());
}

fn import_checked(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    app.read_with(visual, |this, cx| {
        let selection = this.import_selection.as_ref().expect("selection dialog open").read(cx);
        selection
            .names()
            .into_iter()
            .enumerate()
            .filter(|(index, _)| selection.is_checked(*index))
            .map(|(_, name)| name.to_string())
            .collect()
    })
}

fn saved_count(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.saved_connections.len())
}

#[gpui::test]
fn navicat_import_selects_confirms_and_saves_with_the_password_in_the_credential_store(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    let before = saved_count(&app, visual);
    import_pick(visual, "import-menu-navicat", vec![import_fixture("navicat.ncx")]);
    import_selection_dialog(visual);
    assert!(store.records().is_empty(), "nothing is saved before confirming");
    // Supported rows start checked, unsupported ones (Oracle, SQL Server) do not.
    assert_eq!(
        import_checked(&app, visual),
        ["Local MySQL", "Prod PG", "Notes", "Maria", "Replica", "Cache"]
    );
    // Unsupported rows (listed after the supported ones) are not checked.
    assert!(!import_checked(&app, visual).iter().any(|name| name == "Ledger" || name == "Warehouse"));
    // Unchecking a row through a real click drops it from the import.
    click_debug_button(visual, "import-row-2");
    assert!(!import_checked(&app, visual).contains(&"Notes".to_string()));
    click_debug_button(visual, "import-confirm");
    import_wait(visual, |_| !store.records().is_empty());
    import_wait(visual, |v| v.debug_bounds("import-selection-list").is_none());

    let records = store.records();
    assert_eq!(records.len(), 5);
    assert_eq!(saved_count(&app, visual), before + 5, "imported connections appear in the sidebar");
    let mysql = records.iter().find(|r| r.name == "Local MySQL").unwrap();
    assert_eq!(mysql.uri, "mysql://root@127.0.0.1:3306/shop");
    assert!(mysql.save_password);
    assert_eq!(store.password(&mysql.id).as_deref(), Some("navicat-secret"));
    // The password lives only in the credential store.
    let json = serde_json::to_string(&records).unwrap();
    assert!(!json.contains("navicat-secret"));
    let pg = records.iter().find(|r| r.name == "Prod PG").unwrap();
    assert!(pg.network_settings_json.contains("bastion.example.com"));
    // The key file is all that tunnel needs, so it comes in switched on (and with no secret).
    let ssh = crate::ssh_tunnel::SshSettings::from_network_json(&pg.network_settings_json);
    assert!(ssh.enabled);
    assert_eq!(ssh.auth, crate::ssh_tunnel::SshAuthMethod::KeyFile);
    assert_eq!(ssh.key_path, "/keys/id_ed25519");
    assert!(!pg.network_settings_json.contains("ssh-secret"));
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone().unwrap());
    assert!(notice.starts_with("Imported 5 connections from Navicat NCX, skipped unsupported: Oracle, SQL Server"), "{notice}");
    assert!(!notice.contains("navicat-secret"));
    // A successful import reports nothing to the error log.
    app.read_with(visual, |this, _| assert_eq!(this.error_log.log.len(), 0));
}

#[gpui::test]
fn the_wheel_scrolls_the_selection_list_before_the_dialog_around_it(cx: &mut TestAppContext) {
    let (_app, visual, _store) = import_test_app(cx);
    import_pick(visual, "import-menu-navicat", vec![import_fixture("navicat.ncx")]);
    import_selection_dialog(visual);
    // A window the dialog does not fit: its body (a `Scrollable`, whose `ScrollBounce` takes every
    // wheel event over it) scrolls too.
    visual.simulate_resize(gpui::size(gpui::px(900.0), gpui::px(420.0)));
    settle(visual);
    settle(visual);
    let bounds = |visual: &mut gpui::VisualTestContext, selector: &str| {
        visual.debug_bounds(d3_sel(selector.to_string())).unwrap_or_else(|| panic!("{selector} is drawn"))
    };
    let from_start = |visual: &mut gpui::VisualTestContext| bounds(visual, "import-selection-list").origin.y - bounds(visual, "import-row-0").origin.y;
    let to_end = |visual: &mut gpui::VisualTestContext| bounds(visual, "import-selection-list").bottom() - bounds(visual, "import-row-7").bottom();
    let dialog = |visual: &mut gpui::VisualTestContext| bounds(visual, "import-selection-summary").origin.y;
    let wheel = |visual: &mut gpui::VisualTestContext, dy: f32| {
        let list = bounds(visual, "import-selection-list");
        visual.simulate_event(gpui::ScrollEvent {
            position: gpui::point(list.center().x, list.origin.y + gpui::px(60.0)),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(dy))),
            ..Default::default()
        });
        settle(visual);
    };
    assert!(bounds(visual, "import-selection-list").origin.y + gpui::px(60.0) < gpui::px(420.0), "the list is in view");
    let start_dialog = dialog(visual);
    assert!(from_start(visual).abs() <= gpui::px(1.0), "the list starts at its first row");

    wheel(visual, -30.0);
    assert!(from_start(visual) > gpui::px(10.0), "the list scrolled");
    assert_eq!(dialog(visual), start_dialog, "and the dialog stayed where it was");

    wheel(visual, -100_000.0);
    assert!(to_end(visual).abs() <= gpui::px(2.0), "the list went to its end: {:?}", to_end(visual));
    assert_eq!(dialog(visual), start_dialog, "the dialog still did not move");
    wheel(visual, -30.0);
    assert!(to_end(visual).abs() <= gpui::px(2.0), "the list is at its end");
    assert!(dialog(visual) < start_dialog, "so the dialog scrolls");
}

#[gpui::test]
fn cancelling_the_selection_dialog_saves_nothing(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    let before = saved_count(&app, visual);
    import_pick(visual, "import-menu-navicat", vec![import_fixture("navicat.ncx")]);
    import_selection_dialog(visual);
    click_debug_button(visual, "import-cancel");
    import_wait(visual, |v| v.debug_bounds("import-selection-list").is_none());
    assert!(store.records().is_empty());
    assert_eq!(saved_count(&app, visual), before);
    assert!(app.read_with(visual, |this, _| this.import_selection.is_none()));

    // Escape cancels too, and Import is off when nothing is checked.
    import_pick(visual, "import-menu-navicat", vec![import_fixture("navicat.ncx")]);
    import_selection_dialog(visual);
    click_debug_button(visual, "import-select-none");
    assert!(import_checked(&app, visual).is_empty());
    click_debug_button(visual, "import-confirm");
    assert!(store.records().is_empty(), "a disabled Import saves nothing");
    visual.simulate_keystrokes("escape");
    import_wait(visual, |v| v.debug_bounds("import-selection-list").is_none());
    assert!(store.records().is_empty());
    assert_eq!(saved_count(&app, visual), before);

    // A cancelled file picker does nothing either.
    click_debug_button(visual, "sidebar-import-export");
    click_debug_button(visual, "import-menu-dbeaver");
    visual.simulate_path_prompt_response(|_| None);
    visual.run_until_parked();
    assert!(visual.debug_bounds("import-selection-list").is_none());
}

#[gpui::test]
fn dbeaver_import_reads_the_workspace_folder_and_decrypts_credentials(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    let root = std::env::temp_dir().join(format!("tablex-import-dbeaver-{}", uuid::Uuid::new_v4()));
    let dot = root.join("General/.dbeaver");
    std::fs::create_dir_all(&dot).unwrap();
    for name in ["data-sources.json", "credentials-config.json"] {
        std::fs::copy(import_fixture(&format!("dbeaver/{name}")), dot.join(name)).unwrap();
    }
    import_pick(visual, "import-menu-dbeaver", vec![root.clone()]);
    import_selection_dialog(visual);
    let checked = import_checked(&app, visual);
    assert_eq!(checked.len(), 7, "{checked:?}");
    assert!(!checked.contains(&"Finance Oracle".to_string()));
    click_debug_button(visual, "import-confirm");
    import_wait(visual, |_| !store.records().is_empty());
    let records = store.records();
    let pg = records.iter().find(|r| r.name == "Local Postgres").unwrap();
    assert_eq!(pg.uri, "postgresql://postgres@localhost:5432/appdb");
    assert_eq!(store.password(&pg.id).as_deref(), Some("pg-secret"));
    let mysql = records.iter().find(|r| r.name == "Reporting MySQL").unwrap();
    assert_eq!(store.password(&mysql.id).as_deref(), Some("mysql-secret"));
    assert!(!serde_json::to_string(&records).unwrap().contains("pg-secret"));
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone().unwrap());
    assert!(notice.contains("skipped unsupported: Oracle, acme-custom"), "{notice}");
    std::fs::remove_dir_all(root).unwrap();
}

#[gpui::test]
fn datagrip_import_takes_the_files_and_says_passwords_are_not_imported(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    import_pick(
        visual,
        "import-menu-datagrip",
        vec![import_fixture("datagrip/dataSources.xml"), import_fixture("datagrip/dataSources.local.xml")],
    );
    import_selection_dialog(visual);
    assert_eq!(import_checked(&app, visual).len(), 6);
    click_debug_button(visual, "import-confirm");
    import_wait(visual, |_| !store.records().is_empty());
    let records = store.records();
    let pg = records.iter().find(|r| r.name == "prod-postgres").unwrap();
    assert_eq!(pg.uri, "postgresql://deploy@pg.example.com:5432/orders");
    assert!(!pg.save_password);
    assert_eq!(store.password(&pg.id), None);
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone().unwrap());
    assert!(notice.contains("no passwords were imported"), "{notice}");
    assert!(notice.contains("DataGrip keeps passwords in its own keychain entries"), "{notice}");
}

#[gpui::test]
fn dbx_config_import_confirms_and_stores_passwords(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    import_pick(visual, "import-menu-dbx", vec![import_fixture("dbx/plain.json")]);
    import_selection_dialog(visual);
    // Redis is importable now; the two unsupported products are not checked.
    assert_eq!(import_checked(&app, visual).len(), 7);
    click_debug_button(visual, "import-confirm");
    import_wait(visual, |_| !store.records().is_empty());
    let records = store.records();
    assert_eq!(records.len(), 7);
    let pg = records.iter().find(|r| r.name == "Prod PG").unwrap();
    assert_eq!(store.password(&pg.id).as_deref(), Some("pg-pass"));
    let mongo = records.iter().find(|r| r.name == "Replica set").unwrap();
    assert_eq!(store.password(&mongo.id).as_deref(), Some("s3cr:t"));
    let json = serde_json::to_string(&records).unwrap();
    assert!(!json.contains("pg-pass") && !json.contains("s3cr") && !json.contains("ssh-pass"));
}

#[gpui::test]
fn encrypted_dbx_config_asks_for_the_passphrase(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    import_pick(visual, "import-menu-dbx", vec![import_fixture("dbx/encrypted.json")]);
    import_wait(visual, |v| v.debug_bounds("import-passphrase-input").is_some());
    import_wait(visual, |v| v.debug_bounds("import-passphrase-ok").is_some());
    // Wrong passphrase: the dialog comes back with the reason.
    let input = visual.debug_bounds("import-passphrase-input").unwrap();
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_input("not the passphrase");
    click_debug_button(visual, "import-passphrase-ok");
    import_wait(visual, |v| v.debug_bounds("import-passphrase-error").is_some());
    import_wait(visual, |v| v.debug_bounds("import-passphrase-ok").is_some());
    assert!(store.records().is_empty());
    // Right passphrase: the selection dialog opens.
    let input = visual.debug_bounds("import-passphrase-input").unwrap();
    visual.simulate_click(input.center(), Default::default());
    visual.simulate_input("correct horse battery");
    click_debug_button(visual, "import-passphrase-ok");
    import_selection_dialog(visual);
    assert_eq!(import_checked(&app, visual).len(), 7);
    click_debug_button(visual, "import-confirm");
    import_wait(visual, |_| !store.records().is_empty());
    assert_eq!(store.records().len(), 7);
}

fn import_notice(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Option<String> {
    app.read_with(visual, |this, _| this.sidebar.notice.clone())
}

#[gpui::test]
fn foreign_files_report_an_error_without_their_content_and_save_nothing(cx: &mut TestAppContext) {
    let (app, visual, store) = import_test_app(cx);
    // A DBX export is not Navicat XML.
    import_pick(visual, "import-menu-navicat", vec![import_fixture("dbx/plain.json")]);
    import_wait(visual, |v| {
        import_notice(&app, v).is_some_and(|notice| notice.contains("import failed"))
    });
    assert_eq!(
        import_notice(&app, visual).as_deref(),
        Some("Navicat NCX import failed: The file is not valid XML")
    );
    assert!(visual.debug_bounds("import-selection-list").is_none());
    assert!(store.records().is_empty());
    let line = app.read_with(visual, |this, _| this.error_log.log.last().map(|entry| entry.to_line()));
    let line = line.expect("the failure is in the error log");
    assert!(line.contains("not valid XML") && !line.contains("pg-pass"), "{line}");

    // A folder without data-sources.json.
    let empty = std::env::temp_dir().join(format!("tablex-import-empty-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&empty).unwrap();
    import_pick(visual, "import-menu-dbeaver", vec![empty.clone()]);
    import_wait(visual, |v| {
        import_notice(&app, v).is_some_and(|notice| notice.starts_with("DBeaver import failed"))
    });
    assert!(import_notice(&app, visual).unwrap().contains("data-sources.json"));
    std::fs::remove_dir_all(empty).unwrap();
    assert!(store.records().is_empty());
}
