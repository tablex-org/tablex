// Offscreen captures of the navigator's schema-object groups and a source tab (headless
// Metal renderer, real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_schema_object_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_schema_object_screens() {
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

        // The Postgres fixture schema with its groups opened: the sample tables are
        // closed so the groups share the first screenful.
        let function = crate::navigator::schema_object_id(
            crate::ui_fixture::POSTGRES_ID,
            "warehouse",
            "public",
            ObjectGroup::Functions,
            "order_total(bigint, date)",
        );
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                let group = |group| crate::navigator::object_group_id(crate::ui_fixture::POSTGRES_ID, "warehouse", "public", group);
                this.navigator.toggle_expansion(&group(ObjectGroup::Tables));
                // The Mongo sample tree above would fill the screenful: close it.
                this.navigator.toggle_expansion(&NavigatorNodeId::Connection {
                    connection: crate::ui_fixture::MONGO_ID.into(),
                });
                for opened in [
                    ObjectGroup::MaterializedViews,
                    ObjectGroup::Functions,
                    ObjectGroup::Procedures,
                    ObjectGroup::Sequences,
                    ObjectGroup::Triggers,
                    ObjectGroup::Types,
                ] {
                    this.apply_scenario_expansion(&group(opened));
                }
                this.navigator.select_visible(&function, false, false);
                this.navigator_ui.generation = this.navigator.generation();
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "navigator-groups");

        // View Source of the overload with the date argument.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.open_schema_object_source(&function, "order_total(bigint, date)", cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "source-tab");

        // A composite type, for a second shape of source.
        let money = crate::navigator::schema_object_id(
            crate::ui_fixture::POSTGRES_ID,
            "warehouse",
            "public",
            ObjectGroup::Types,
            "money_pair",
        );
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.open_schema_object_source(&money, "money_pair", cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "source-tab-type");
    }
}
