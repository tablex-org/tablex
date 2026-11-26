// Offscreen captures of the confirmation of an UPDATE / DELETE with its preview of the rows it will
// change (headless Metal renderer, real text system). The reads are fed to the preview by hand (the
// model a driver read produces), so the screens need no database. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_affected_preview_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_affected_preview_screens() {
    use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};

    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let orders = |rows: i64| {
        let customers = ["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Stark"];
        let ids: Vec<i64> = (1..=rows).collect();
        let batch = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(ids.clone())) as ArrayRef),
            (
                "customer",
                Arc::new(StringArray::from_iter_values(ids.iter().map(|i| customers[*i as usize % 6]))) as ArrayRef,
            ),
            (
                "total",
                Arc::new(Float64Array::from_iter_values(ids.iter().map(|i| 120.0 + (*i as f64) * 37.25))) as ArrayRef,
            ),
            ("status", Arc::new(StringArray::from_iter_values(ids.iter().map(|_| "pending"))) as ArrayRef),
        ])
        .unwrap();
        ArrowGridSource::new(vec![Arc::new(batch)])
    };
    for (theme_name, mode) in [("dark", gpui_component::ThemeMode::Dark), ("light", gpui_component::ThemeMode::Light)] {
        for (name, sql, count) in [
            ("where", "DELETE FROM orders WHERE status = 'pending'", 1204u64),
            ("whole-table", "DELETE FROM orders", 20),
        ] {
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
                .open_window(gpui::size(gpui::px(1000.0), gpui::px(720.0)), |window, cx| {
                    // The UI fixture, never the user's own connections.
                    let scenario =
                        UiScenario::from_args(["tablex-gpui", "--ui-scenario", "result-grid", "--ui-fixture", "populated"])
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
                for _ in 0..30 {
                    cx.run_until_parked();
                    cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(15));
                }
            };
            settle(&mut cx);
            cx.update_window(handle, |_, window, app_cx| {
                app.update(app_cx, |this, cx| {
                    let tab = this.active_id();
                    let scan = crate::affected_rows::scan(sql, &engine_driver_kit::sql::SqlLexOptions::postgres(), engine_plan::ExplainDialect::Postgres);
                    let root = cx.entity().downgrade();
                    let preview = cx.new(|_| affected_preview::AffectedPreview::fixture(scan, root));
                    preview.update(cx, |preview, cx| preview.landed(0, Ok((Some(count), orders(20))), window, cx));
                    this.driver_runtime.preview_fixture = Some(preview);
                    this.open_execute_confirmation(
                        tab,
                        sql.to_owned(),
                        "Production \u{00B7} shop-db".into(),
                        connection_policy::GatedRun::Run,
                        window,
                        cx,
                    );
                })
            })
            .unwrap();
            settle(&mut cx);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("affected-preview-{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
