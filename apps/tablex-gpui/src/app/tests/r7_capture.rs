// Offscreen captures of Users & Privileges: the list with a role's attributes, the Privileges panel with the editor of one table, and the
// review dialog with the password hidden (headless Metal renderer, real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_accounts_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_accounts_screens() {
    use crate::app::accounts::Panel;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

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

        let reads = super::accounts_ui::ac_capture_server();
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                this.accounts_injected = Some(Arc::new(reads));
                this.open_accounts(crate::ui_fixture::POSTGRES_ID, window, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        let view = cx.update(|app_cx| app.read(app_cx).accounts.values().next().cloned().expect("the tab"));

        // The list and the attributes of a role.
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.capture_select("app", window, cx))).unwrap();
        settle(&mut cx);
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.capture_flag(|attributes| attributes.create_db = true, cx))).unwrap();
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.capture_password("a-new-secret-1", window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "accounts-attributes");

        // The privileges of that role: the tables, and the editor of one of them.
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.capture_panel(Panel::Privileges, window, cx))).unwrap();
        settle(&mut cx);
        cx.update_window(handle, |_, _, app_cx| {
            view.update(app_cx, |view, cx| {
                view.capture_object("public.orders", cx);
                view.capture_privilege("public.orders", "DELETE", true, false, cx);
                view.capture_privilege("public.orders", "SELECT", true, true, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "accounts-privileges");

        // The membership panel.
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.capture_panel(Panel::Membership, window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "accounts-membership");

        // The review: the password is hidden, and the tab holds none any more.
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.capture_review(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "accounts-review");
    }
}
