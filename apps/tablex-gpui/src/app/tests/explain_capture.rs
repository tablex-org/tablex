// Offscreen captures of the Explain plan view, rendered through GPUI's headless Metal renderer with the
// real text system (no desktop session needed, unlike scripts/capture-gpui.py). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_explain_screens -- --ignored --nocapture --test-threads=1
//
// Each screen shows an `engine-plan` fixture (captured from the dedicated test containers) as the plan of a SQL
// tab, through the real completion path of an Explain run, and writes one PNG per screen and theme:
// `plan-postgres-analyze` (a node selected, with its detail pane), `plan-mysql-analyze` (nodes whose estimate is off by more than 10x, highlighted),
// `plan-mysql-estimate` (costs only, no actuals), `plan-postgres-raw` (the Raw view of the same plan).

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_explain_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    // (file name, kind, dialect, mode, fixture, selected node, Raw view)
    let screens = [
        (
            "plan-postgres-analyze",
            ConnectionKind::Postgres,
            ExplainDialect::Postgres,
            ExplainMode::Analyze,
            "postgres/cte_analyze.json",
            Some(3),
            false,
        ),
        (
            "plan-mysql-estimate",
            ConnectionKind::Mysql,
            ExplainDialect::MySql,
            ExplainMode::Estimate,
            "mysql/join_estimate.json",
            Some(1),
            false,
        ),
        (
            "plan-mysql-analyze",
            ConnectionKind::Mysql,
            ExplainDialect::MySql,
            ExplainMode::Analyze,
            "mysql/union_analyze.txt",
            Some(1),
            false,
        ),
        (
            "plan-postgres-raw",
            ConnectionKind::Postgres,
            ExplainDialect::Postgres,
            ExplainMode::Analyze,
            "postgres/cte_analyze.json",
            None,
            true,
        ),
    ];
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for (name, kind, dialect, explain_mode, fixture, selected, raw) in screens {
            let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "result-grid", "--ui-fixture", "sample"])
                .unwrap()
                .unwrap();
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
                .open_window(gpui::size(gpui::px(1280.0), gpui::px(800.0)), |window, cx| {
                    let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
                .expect("the capture window opens");
            let handle: gpui::AnyWindowHandle = window.into();
            let root = window.entity(&cx).expect("root");
            let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
            let plan = crate::plan_view::fixtures::plan(dialect, explain_mode, fixture);
            let statement = explain_sql(dialect, explain_mode, "SELECT * FROM orders o JOIN order_items i ON i.order_id = o.id", true)
                .unwrap()
                .sql;
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let saved = explain_connection(kind);
                    let key = saved.id.to_string();
                    this.saved_connections.push(saved);
                    let epoch = this.sessions.begin_connect(&key).unwrap();
                    this.sessions.finish_connect(&key, epoch, Ok(()));
                    let tab = this.workspace.open_query_tab_with_metadata(
                        "orders report",
                        TabMetadata {
                            connection_id: key.clone(),
                            ..Default::default()
                        },
                    );
                    this.workspace.tab_mut(tab).unwrap().sql =
                        "SELECT * FROM orders o JOIN order_items i ON i.order_id = o.id".into();
                    this.workspace.activate_tab(tab);
                    let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
                    this.query_tasks.registry.register(tab, run);
                    let explain = ExplainRun { dialect, mode: explain_mode, read_only: true };
                    let request = SqlRun {
                        tab_id: tab,
                        kind: RunKind::Explain(explain),
                        connection: key,
                        sql: statement.clone(),
                        database: String::new(),
                        collection: String::new(),
                        limit: drivers::EXPLAIN_ROW_LIMIT,
                        offset: 0,
                        confirmed: false,
                        read_only: false,
                    };
                    this.finish_explain_run(run, request, explain, Ok(plan), cx);
                    if let Some(node) = selected {
                        this.select_plan_node(tab, node, cx);
                    }
                    if raw {
                        this.set_plan_raw(tab, true, cx);
                    }
                    cx.notify();
                });
            })
            .unwrap();
            for _ in 0..30 {
                cx.run_until_parked();
                cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
