// Offscreen captures of the import wizard: the options and preview, the mapping, the settings, a load in
// progress, the summary of one with skipped rows, and the mapping of a new table (headless Metal renderer,
// real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_import_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_import_screens() {
    use crate::app::data_import::run::{Outcome, ProgressSnapshot, Report, RowError};
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("orders-2024.csv");
    let mut text = String::from("id,name,status,created,score,notes,big_number\n");
    for id in 1..=240 {
        let score = if id == 7 { "n/a".to_owned() } else { format!("{}.5", id % 9) };
        text.push_str(&format!("{id},Order {id:04},{},2024-03-{:02},{score},\"note, {id}\",{}\n", ["new", "paid", "shipped"][id % 3], id % 28 + 1, id * 1_000_003));
    }
    std::fs::write(&file, text).unwrap();

    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "connection-tree", "--ui-fixture", "sample"]).unwrap().unwrap();
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
            .open_window(gpui::size(gpui::px(1280.0), gpui::px(900.0)), |window, cx| {
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
        let shoot = |cx: &mut gpui::HeadlessAppContext, name: &str| {
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        };

        let postgres = crate::ui_fixture::POSTGRES_ID;
        let origin = |object: &str| crate::app::structure_edit::apply::TableOrigin {
            connection: postgres.into(),
            database: "warehouse".into(),
            schema: "public".into(),
            object: object.into(),
        };
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);

        // 1. Existing table: options and preview, mapping (with a flagged column), settings.
        cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_import_data(origin("orders"), window, cx))).unwrap();
        settle(&mut cx);
        let import = cx.update(|app_cx| app.read(app_cx).imports.values().next().cloned().expect("the wizard"));
        cx.update_window(handle, |_, window, app_cx| import.update(app_cx, |view, cx| view.set_file(file.clone(), window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-source");
        cx.update_window(handle, |_, window, app_cx| import.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-mapping");
        cx.update_window(handle, |_, window, app_cx| import.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-settings");
        cx.update_window(handle, |_, _, app_cx| {
            import.update(app_cx, |view, cx| {
                view.seed_progress(ProgressSnapshot { rows_read: 612_000, inserted: 611_000, skipped: 3, bytes: 31_400_000 }, 52_000_000, std::time::Duration::from_secs(14), cx)
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-progress");
        cx.update_window(handle, |_, _, app_cx| {
            import.update(app_cx, |view, cx| {
                view.seed_progress(ProgressSnapshot::default(), 0, std::time::Duration::ZERO, cx);
                view.seed_report(
                    Report {
                        outcome: Outcome::Completed,
                        inserted: 998_746,
                        rolled_back: 0,
                        skipped: 3,
                        rows_read: 998_749,
                        errors: vec![
                            RowError { row: 7, line: 8, message: "\"score\" is not a number: \"n/a\"".into() },
                            RowError { row: 5_021, line: 5_022, message: "duplicate key value violates unique constraint \"orders_name_key\"".into() },
                            RowError { row: 77_310, line: 77_311, message: "has 6 values, the header has 7".into() },
                        ],
                        errors_total: 3,
                        uncertain: 0,
                        bytes: 52_000_000,
                        elapsed: std::time::Duration::from_secs_f64(23.6),
                    },
                    cx,
                );
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-summary");

        // 2. A new table: the mapping rows are its columns.
        cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_import_data(origin(""), window, cx))).unwrap();
        settle(&mut cx);
        let import = cx.update(|app_cx| app.read(app_cx).imports.iter().max_by_key(|(tab, _)| **tab).map(|(_, view)| view.clone()).expect("the wizard"));
        cx.update_window(handle, |_, window, app_cx| import.update(app_cx, |view, cx| view.set_file(file.clone(), window, cx))).unwrap();
        settle(&mut cx);
        cx.update_window(handle, |_, window, app_cx| import.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "import-new-table");
    }
}
