// Offscreen captures of the SSH tunnel screens, rendered through GPUI's headless Metal renderer
// with the real text system (no desktop session needed, unlike scripts/capture-gpui.py). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_ssh_tunnel_screens -- --ignored --nocapture --test-threads=1
//
// It drives the same `ui-scenarios` seeds (`TABLEX_SEED_TUNNEL`) the capture runner uses and writes
// one PNG per screen and theme.

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_ssh_tunnel_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    // (file name, TABLEX_SEED_TUNNEL, scenario to open)
    let screens = [
        ("tunnel-tab", "ok", "new-connection"),
        ("test-success", "ok,test", "new-connection"),
        ("test-failure", "refused,test", "new-connection"),
        ("trust-host", "unknown-host,test", "new-connection"),
        ("key-changed", "changed-host,test", "new-connection"),
        ("ki-dialog", "ki,test", "new-connection"),
        ("import-tunnels", "ok", "new-connection"),
        ("via-ssh", "connected", "result-grid"),
        // Two jump rows and a SOCKS5 proxy on the tab, and the chain's status on a connected tab.
        ("tunnel-chain", "ok,chain", "new-connection"),
        ("via-chain", "ok,chain,connected", "result-grid"),
        // The same tab scrolled to its end, where the jump rows and the proxy section are.
        ("tunnel-chain-bottom", "ok,chain,bottom", "new-connection"),
        // And part of the way, where the two jump rows are.
        ("tunnel-chain-jumps", "ok,chain,jumps", "new-connection"),
    ];
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for (name, seed, scenario) in screens {
            std::env::set_var("TABLEX_SEED_TUNNEL", seed);
            let scenario = UiScenario::from_args([
                "tablex-gpui",
                "--ui-scenario",
                scenario,
                "--ui-fixture",
                "sample",
            ])
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
                .open_window(gpui::size(gpui::px(1100.0), gpui::px(850.0)), |window, cx| {
                    let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                })
                .expect("the capture window opens");
            let handle: gpui::AnyWindowHandle = window.into();
            let root = window.entity(&cx).expect("root");
            let app = cx.update(|app| {
                root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot")
            });
            let opens_dialog = !seed.ends_with("connected") && name != "import-tunnels";
            if name == "import-tunnels" {
                let preview = match crate::connection_import::load(
                    crate::connection_import::ImportSource::Navicat,
                    &[std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/connection_import/fixtures/navicat.ncx")],
                ) {
                    Ok(crate::connection_import::Loaded::Ready(preview)) => preview,
                    other => panic!("the fixture loads: {other:?}"),
                };
                cx.update_window(handle, |_, window, app_cx| {
                    app.update(app_cx, |this, cx| this.open_import_selection(preview, window, cx));
                })
                .unwrap();
            }
            if opens_dialog {
                cx.update_window(handle, |_, window, app_cx| {
                    app.update(app_cx, |this, cx| this.open_connection_dialog(None, window, cx));
                })
                .unwrap();
            }
            // Let the seeded Test tunnel run on its worker thread and its dialogs open.
            for _ in 0..60 {
                cx.run_until_parked();
                cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            cx.run_until_parked();
            cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
            cx.run_until_parked();
            let wheel_events = if seed.split(',').any(|token| token == "bottom") {
                8
            } else if seed.split(',').any(|token| token == "jumps") {
                2
            } else {
                0
            };
            if wheel_events > 0 {
                // Wheel the tab's scroll area to its end (the dialog sits in the middle of the window).
                cx.update_window(handle, |_, window, app_cx| {
                    let center = window.bounds().center();
                    for _ in 0..wheel_events {
                        window.dispatch_event(
                            gpui::PlatformInput::ScrollWheel(gpui::ScrollEvent {
                                position: center,
                                delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(-300.0))),
                                touch_phase: gpui::TouchPhase::Moved,
                                modifiers: Default::default(),
                            }),
                            app_cx,
                        );
                    }
                })
                .unwrap();
                for _ in 0..4 {
                    cx.run_until_parked();
                    cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                }
            }
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
