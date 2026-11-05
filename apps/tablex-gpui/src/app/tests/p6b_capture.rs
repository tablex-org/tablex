// Offscreen captures of the navigator's object actions: the New table form, the review of a drop
// and the rename dialog (headless Metal renderer, real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_object_action_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_object_action_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "connection-tree", "--ui-fixture", "sample"])
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
            for _ in 0..30 {
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
        let close_dialog = |cx: &mut gpui::HeadlessAppContext| {
            cx.update_window(handle, |_, window, app_cx| window.close_dialog(app_cx)).unwrap();
        };

        let postgres = crate::ui_fixture::POSTGRES_ID;
        let orders = NavigatorNodeId::Object { connection: postgres.into(), database: "warehouse".into(), schema: "public".into(), object: "orders".into() };
        let schema = NavigatorNodeId::Schema { connection: postgres.into(), database: "warehouse".into(), schema: "public".into() };
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                // The Mongo sample tree above would fill the screenful: close it.
                this.navigator.toggle_expansion(&NavigatorNodeId::Connection { connection: crate::ui_fixture::MONGO_ID.into() });
                this.navigator.select_visible(&orders, false, false);
                this.navigator_ui.generation = this.navigator.generation();
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);

        // The drop review: the SQL, the warning, the CASCADE box and the acknowledgement.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| this.run_object_menu_item(ObjectMenuItem::Drop(ObjectKind::Table), &orders, window, cx))
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "drop-review");
        close_dialog(&mut cx);
        settle(&mut cx);

        // The rename dialog.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| this.run_object_menu_item(ObjectMenuItem::Rename(ObjectKind::Table), &orders, window, cx))
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "rename-dialog");
        close_dialog(&mut cx);
        settle(&mut cx);

        // The New table form, filled in.
        cx.update_window(handle, |_, window, app_cx| {
            let form = app.update(app_cx, |this, cx| this.open_new_table_form_for_test(&schema, window, cx)).expect("the form opens");
            form.update(app_cx, |form, cx| {
                form.fill(
                    "invoices",
                    "One row per invoice sent to a customer",
                    &[("id", "bigint", false, true), ("customer_id", "bigint", false, false), ("total", "numeric(12,2)", false, false), ("issued_at", "timestamptz", true, false)],
                    window,
                    cx,
                )
            });
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "new-table");
        close_dialog(&mut cx);
        settle(&mut cx);
    }
}
