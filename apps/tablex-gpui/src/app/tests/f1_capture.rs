// Offscreen captures of dropping a .sql file: the SQL editor header with its Run SQL File button beside the open
// transaction's controls, the editor tab and the navigator row highlighted while a file is dragged over them (headless
// Metal renderer, real text system, fixture data only). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_file_drop_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_file_drop_screens() {
    use gpui::InputEvent as _;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("warehouse-dump.sql");
    std::fs::write(&file, "CREATE TABLE t(a int);\n").unwrap();

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
            .open_window(gpui::size(gpui::px(1280.0), gpui::px(700.0)), |window, cx| {
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
        let drag = |cx: &mut gpui::HeadlessAppContext, event: gpui::FileDropEvent| {
            cx.update_window(handle, |_, window, app_cx| window.dispatch_event(event.to_platform_input(), app_cx)).unwrap();
        };

        let postgres = crate::ui_fixture::POSTGRES_ID;
        let (tab, row) = cx
            .update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    this.seed_connection_tree_scenario();
                    let tab = this.workspace.open_query_tab_with_metadata("orders report", TabMetadata { connection_id: postgres.into(), ..Default::default() });
                    this.workspace.tab_mut(tab).unwrap().sql = "select id, total from orders where total > 100;".into();
                    this.workspace.activate_tab(tab);
                    this.transactions.set_for_capture(tab, open_tx_state());
                    cx.notify();
                    let row = this.navigator.visible_index(&NavigatorNodeId::Connection { connection: postgres.into() }).expect("the Postgres connection row");
                    (tab, row)
                })
            })
            .unwrap();
        let _ = tab;
        settle(&mut cx);
        shoot(&mut cx, "f1-toolbar");

        // The editor under the pointer.
        let editor = gpui::point(gpui::px(800.0), gpui::px(330.0));
        let paths = || gpui::ExternalPaths([file.clone()].into_iter().collect());
        drag(&mut cx, gpui::FileDropEvent::Entered { position: editor, paths: paths() });
        drag(&mut cx, gpui::FileDropEvent::Pending { position: editor });
        settle(&mut cx);
        shoot(&mut cx, "f1-editor-drop");

        // The navigator row under the pointer.
        let (bounds, offset) = cx.update(|app_cx| {
            let state = app.read(app_cx).sidebar.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        });
        let at = gpui::point(bounds.origin.x + gpui::px(80.0), bounds.origin.y + offset.y + gpui::px(theme::SIDEBAR_ROW_HEIGHT * (row as f32 + 0.5)));
        drag(&mut cx, gpui::FileDropEvent::Pending { position: at });
        settle(&mut cx);
        shoot(&mut cx, "f1-navigator-drop");

        // Out of the window: no highlight stays.
        drag(&mut cx, gpui::FileDropEvent::Exited);
        drag(&mut cx, gpui::FileDropEvent::Ended);
        settle(&mut cx);
        shoot(&mut cx, "f1-after-leave");
    }
}
