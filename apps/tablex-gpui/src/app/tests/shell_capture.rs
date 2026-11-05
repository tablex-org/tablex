// Offscreen captures of the MongoDB shell screens, rendered through GPUI's headless Metal renderer with
// the real text system (no desktop session needed, unlike scripts/capture-gpui.py). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_shell_screens -- --ignored --nocapture --test-threads=1
//
// One PNG per screen and theme:
//   `shell-install-prompt`       a shell script was run without the plugin: "Install the MongoDB Shell plugin (3.6 MB)?"
//   `shell-run`                  a finished run: documents in the grid, the strip with More, the Output pane (a pretty `printjson` block over several lines)
//   `shell-run-error`            a run that failed: the error with line and column inline, what it printed kept
//   `driver-manager-shell-addon` the Driver Manager with the add-on row under MongoDB, Install offered
//   `driver-manager-shell-needs-driver` the same row while the installed driver is too old: the reason and
//                                the "Update MongoDB driver first" button
// The screens go through the real run, prompt and result code (the plugin's reply is a canned one).

const SHELL_SCRIPT: &str = "// Look at the paid orders, newest first\nconst paid = db.orders.find({ status: 'paid' }).sort({ _id: -1 }).toArray();\nprint('paid orders: ' + paid.length);\nprintjson(paid[0]);\ndb.orders.find({ status: 'paid' }).sort({ _id: -1 })";

fn shell_capture_window(
    mode: gpui_component::ThemeMode,
    scenario: UiScenario,
) -> (gpui::HeadlessAppContext, gpui::AnyWindowHandle, gpui::Entity<AppRoot>) {
    shell_capture_window_sized(mode, scenario, gpui::size(gpui::px(1280.0), gpui::px(800.0)))
}

fn shell_capture_window_sized(
    mode: gpui_component::ThemeMode,
    scenario: UiScenario,
    size: gpui::Size<gpui::Pixels>,
) -> (gpui::HeadlessAppContext, gpui::AnyWindowHandle, gpui::Entity<AppRoot>) {
    let mut cx = gpui::HeadlessAppContext::with_platform(
        MAIN_THREAD_TEXT_SYSTEM.get().expect("text system").clone(),
        Arc::new(crate::assets::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        theme::apply(mode, app);
        app.set_reduce_motion(true);
    });
    let window = cx
        .open_window(size, |window, cx| {
            let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        })
        .expect("the capture window opens");
    let handle: gpui::AnyWindowHandle = window.into();
    let root = window.entity(&cx).expect("root");
    let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
    (cx, handle, app)
}

fn shell_capture_frames(cx: &mut gpui::HeadlessAppContext, handle: gpui::AnyWindowHandle) {
    for _ in 0..30 {
        cx.run_until_parked();
        cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

fn orders_reply(output: &[&str], error: Option<engine_driver_api::mongo_shell::ShellFailure>) -> engine_driver_api::mongo_shell::ShellReply {
    let raw = |text: &str| serde_json::value::RawValue::from_string(text.into()).unwrap();
    let documents = [
        r#"{"_id":{"$numberInt":"9"},"sku":"A-1009","customer":"Mika Aalto","qty":{"$numberInt":"2"},"total":{"$numberDouble":"59.8"},"status":"paid","placedAt":{"$date":{"$numberLong":"1792137600000"}}}"#,
        r#"{"_id":{"$numberInt":"7"},"sku":"B-2210","customer":"Ines Duarte","qty":{"$numberInt":"1"},"total":{"$numberDouble":"129.0"},"status":"paid","placedAt":{"$date":{"$numberLong":"1792051200000"}}}"#,
        r#"{"_id":{"$numberInt":"4"},"sku":"A-1002","customer":"Tomas Novak","qty":{"$numberInt":"5"},"total":{"$numberDouble":"149.5"},"status":"paid","placedAt":{"$date":{"$numberLong":"1791964800000"}}}"#,
        r#"{"_id":{"$numberInt":"2"},"sku":"C-3301","customer":"Sade Okafor","qty":{"$numberInt":"3"},"total":{"$numberDouble":"87.0"},"status":"paid","placedAt":{"$date":{"$numberLong":"1791878400000"}}}"#,
    ];
    engine_driver_api::mongo_shell::ShellReply {
        output: output.iter().map(|line| line.to_string()).collect(),
        output_truncated: false,
        result: if error.is_some() {
            engine_driver_api::mongo_shell::ShellResult::None
        } else {
            engine_driver_api::mongo_shell::ShellResult::Cursor {
                documents: documents.iter().map(|text| raw(text)).collect(),
                cursor: Some(engine_driver_api::mongo_shell::CursorRef {
                    id: "8123456789".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    batch_size: 50,
                }),
            }
        },
        database_after: "shop".into(),
        error,
    }
}

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_shell_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for name in [
            "shell-install-prompt",
            "shell-run",
            "shell-run-error",
            "driver-manager-shell-addon",
            "driver-manager-shell-needs-driver",
        ] {
            let kind = if name.starts_with("driver-manager") { "driver-manager" } else { "connection-tree" };
            let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", kind, "--ui-fixture", "sample"])
                .unwrap()
                .unwrap();
            let (mut cx, handle, app) = shell_capture_window(mode, scenario);
            // The first frames: the window is known to the app (dialogs open on it).
            shell_capture_frames(&mut cx, handle);
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| match name {
                    "driver-manager-shell-addon" | "driver-manager-shell-needs-driver" => {
                        let seed = if name.ends_with("needs-driver") {
                            driver_manager::ShellAddonSeed::NeedsDriver
                        } else {
                            driver_manager::ShellAddonSeed::Available
                        };
                        this.seed_shell_addon_fixture(seed);
                        // Only the NoSQL drivers, so MongoDB and its add-on are in view; its
                        // queued update (part of the scenario) is dropped.
                        this.driver_ui.queue.cancel("mongodb");
                        this.driver_ui
                            .model
                            .set_filter(driver_manager::DriverFilter::Category(engine_driver_host::DriverCategory::Nosql));
                        cx.notify();
                    }
                    _ => {
                        this.seed_connection_tree_scenario();
                        this.seed_shell_addon_fixture(driver_manager::ShellAddonSeed::Available);
                        this.driver_ui.installed_loaded = true;
                        // The MongoDB driver the add-on needs is installed.
                        this.driver_ui.installed.insert("mongodb".into());
                        this.driver_ui.records.insert(
                            "mongodb".into(),
                            engine_driver_host::InstalledDriverRecord {
                                id: "mongodb".into(),
                                version: "2.0.3".into(),
                                ..Default::default()
                            },
                        );
                        let id = NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() };
                        assert!(this.new_query_for_sidebar_node(&id, cx));
                        let tab = this.workspace.active_tab().unwrap().id;
                        {
                            let tab = this.workspace.tab_mut(tab).unwrap();
                            tab.sql = SHELL_SCRIPT.into();
                            tab.database = "shop".into();
                            tab.collection = "orders".into();
                        }
                        match name {
                            "shell-install-prompt" => {
                                this.run_mongo_shell_script(tab, SHELL_SCRIPT.into(), false, "shop".into(), None, cx);
                            }
                            "shell-run" => this.show_shell_reply_for_capture(
                                tab,
                                SHELL_SCRIPT,
                                orders_reply(
                                    &[
                                        "paid orders: 4",
                                        // printjson is pretty: one entry, several lines.
                                        "{\n  \"_id\": 9,\n  \"sku\": \"A-1009\",\n  \"customer\": \"Mika Aalto\",\n  \"total\": 59.8\n}",
                                    ],
                                    None,
                                ),
                                cx,
                            ),
                            _ => this.show_shell_reply_for_capture(
                                tab,
                                SHELL_SCRIPT,
                                orders_reply(
                                    &["paid orders: 4"],
                                    Some(engine_driver_api::mongo_shell::ShellFailure {
                                        kind: engine_driver_api::mongo_shell::FailureKind::Script,
                                        message: "ReferenceError: paidd is not defined".into(),
                                        line: Some(4),
                                        column: Some(11),
                                        code: None,
                                        code_name: None,
                                    }),
                                ),
                                cx,
                            ),
                        }
                        cx.notify();
                    }
                });
            })
            .unwrap();
            shell_capture_frames(&mut cx, handle);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
