// Offscreen captures of the Query History tab (dark and light), rendered through GPUI's headless Metal
// renderer with the real text system. The rows are made-up runs filed in the test binary's temporary data
// dir, never the user's history. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_query_history_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_query_history_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (shop, analytics) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let ago = |minutes: f64| minutes;
    let runs: Vec<(String, &str, &str, f64, Option<&str>, i64, i64)> = vec![
        (shop.to_string(), "shop", "SELECT o.id, c.name, o.total\n  FROM orders o\n  JOIN customers c ON c.id = o.customer_id\n WHERE o.created_at > now() - interval '7 days'\n ORDER BY o.total DESC\n LIMIT 100", ago(2.0), None, 100, 38),
        (shop.to_string(), "shop", "UPDATE orders SET status = 'shipped' WHERE id IN (1041, 1042, 1043)", ago(9.0), None, 3, 12),
        (shop.to_string(), "shop", "SELECT * FROM order_items WHERE order_id = 99999", ago(15.0), Some("relation \"order_items\" does not exist"), 0, 4),
        (analytics.to_string(), "reports", "SELECT date_trunc('day', created_at) AS day, count(*)\n  FROM events\n GROUP BY 1\n ORDER BY 1 DESC", ago(35.0), None, 90, 1420),
        (shop.to_string(), "shop", "ALTER TABLE customers ADD COLUMN loyalty_tier text", ago(60.0 * 3.0), None, 0, 87),
        (analytics.to_string(), "reports", "DELETE FROM events WHERE created_at < now() - interval '400 days'", ago(60.0 * 5.0), Some("canceling statement due to statement timeout"), 0, 30000),
        (shop.to_string(), "shop", "SELECT count(*) FROM customers", ago(60.0 * 26.0), None, 1, 6),
        (shop.to_string(), "shop", "INSERT INTO customers (name, email) VALUES ('Ada', 'ada@example.com')", ago(60.0 * 27.0), None, 1, 9),
    ];
    {
        let conn = engine_storage::app_db::open(&engine_storage::app_db::default_path()).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        for (connection, database, sql, minutes, error, rows, duration) in &runs {
            engine_storage::history::record_in(
                &conn,
                &engine_storage::history::Record {
                    conn_uuid: connection,
                    database,
                    collection: "",
                    filter: sql,
                    project: "",
                    sort: "",
                    skip: 0,
                    limit: 200,
                    max_time_ms: 0,
                    result_count: *rows,
                    error: *error,
                    duration_ms: Some(*duration),
                    kind: engine_storage::history::classify_sql(sql),
                },
                now - minutes * 60.0,
            )
            .unwrap();
        }
    }
    let audit_dir = std::env::temp_dir().join(format!("tablex-history-capture-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&audit_dir).unwrap();
    let audit = audit_dir.join("audit.jsonl");
    let stamp = |minutes: i64| (chrono::Utc::now() - chrono::Duration::minutes(minutes)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    std::fs::write(
        &audit,
        [
            audit_line(&stamp(5), "mcp", "execute_query", &shop.to_string(), "SELECT id, status FROM orders WHERE status = 'pending'", "ok"),
            audit_line(&stamp(20), "mcp", "list_tables", &shop.to_string(), "", "ok"),
            audit_line(&stamp(40), "mcp", "execute_query", &analytics.to_string(), "DELETE FROM events", "agent.write_refused"),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    for (theme_name, mode) in [("dark", gpui_component::ThemeMode::Dark), ("light", gpui_component::ThemeMode::Light)] {
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
            actions::bind_keys(app);
            app.set_reduce_motion(true);
        });
        let window = cx
            .open_window(gpui::size(gpui::px(1280.0), gpui::px(760.0)), |window, cx| {
                let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "new-connection", "--ui-fixture", "sample"])
                    .unwrap()
                    .unwrap();
                let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("the capture window opens");
        let handle: gpui::AnyWindowHandle = window.into();
        let root = window.entity(&cx).expect("root");
        let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
        let settle = |cx: &mut gpui::HeadlessAppContext| {
            for _ in 0..40 {
                cx.run_until_parked();
                cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        };
        settle(&mut cx);
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                for (id, name) in [(shop, "Shop"), (analytics, "Analytics")] {
                    this.saved_connections.push(SavedConnection {
                        id,
                        name: name.into(),
                        uri: "postgres://localhost/none".into(),
                        kind: ConnectionKind::Postgres,
                        save_password: false,
                        is_pinned: false,
                        disabled: false,
                        ssh_tunnel: false,
                    });
                }
                this.history.audit_path = Some(audit.clone());
                this.open_query_history(None, window, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        // The second row of the list is the failed one with its error: select it for the preview.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                let view = this.history.view.clone().expect("the tab");
                view.update(cx, |view, cx| view.select_row_for_test(2, window, cx));
            })
        })
        .unwrap();
        settle(&mut cx);
        let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
        let path = dir.join(format!("query-history-{theme_name}.png"));
        image.save(&path).expect("write the PNG");
        eprintln!("wrote {}", path.display());
    }
    std::fs::remove_dir_all(audit_dir).unwrap();
}
