// Offscreen capture of the approval dialog of an MCP client that cannot ask the user itself (headless
// Metal renderer, real text system, fixture data only), dark and light. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_approval_dialog_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_approval_dialog_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
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
        let statement = "UPDATE orders\nSET status = 'cancelled'\nWHERE created_at < '2024-01-01'\n  AND status = 'pending';";
        let (reply, _answers) = std::sync::mpsc::channel();
        let ask = engine_connect::app_channel::ApprovalAsk {
            id: "0123456789abcdef0123456789abcdef".into(),
            title: "Run this statement".into(),
            client: Some("example-agent".into()),
            connection: Some("Warehouse".into()),
            connection_type: Some("Development".into()),
            database: Some("shop".into()),
            danger: Some("UPDATE changes rows that a rollback may not bring back".into()),
            notes: vec!["The statement runs inside transaction tx-1. It is NOT committed until you approve the commit of the transaction; a rollback undoes it.".into()],
            statements: vec![statement.into()],
            ..Default::default()
        };
        cx.update_window(handle, |_, _, app_cx| app.update(app_cx, |this, cx| this.enqueue_approval(ask, reply, cx))).unwrap();
        // Just opened: Approve is disabled (the delay has not run out).
        let settle_short = |cx: &mut gpui::HeadlessAppContext| {
            for _ in 0..6 {
                cx.run_until_parked();
                cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(15));
            }
        };
        settle_short(&mut cx);
        for (state, wait) in [("disarmed", false), ("armed", true)] {
            if wait {
                std::thread::sleep(super::approval_dialog::ARM_DELAY + std::time::Duration::from_millis(150));
                settle_short(&mut cx);
            }
            let can = cx.update(|app_cx| app.read(app_cx).approvals.can_approve());
            eprintln!("CAN APPROVE {state} {theme_name}: {can}");
            assert_eq!(can, wait, "Approve works only in the armed capture");
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("approval-dialog-{state}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
