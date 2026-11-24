// Offscreen captures of the Welcome tab (see `shell_capture.rs` for the way they are made). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_welcome_screens -- --ignored --nocapture --test-threads=1
//
// `welcome-recents`  connections listed, a driver installed (no note)
// `welcome-empty`    no saved connections and no driver: the empty Recent list and the note
// `welcome-small`    the same with connections, in a 560 x 420 window: the sections wrap and scroll

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_welcome_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for (name, width, height) in [("welcome-recents", 1100.0, 760.0), ("welcome-empty", 1100.0, 760.0), ("welcome-small", 560.0, 420.0)] {
            let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "connection-tree", "--ui-fixture", "sample"])
                .unwrap()
                .unwrap();
            let (mut cx, handle, app) =
                shell_capture_window_sized(mode, scenario, gpui::size(gpui::px(width), gpui::px(height)));
            shell_capture_frames(&mut cx, handle);
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    this.driver_ui.installed_loaded = true;
                    if name == "welcome-empty" {
                        this.saved_connections.clear();
                        this.driver_ui.installed.clear();
                    } else if name == "welcome-recents" {
                        this.driver_ui.installed.insert("sqlite".into());
                    }
                    cx.notify();
                });
            })
            .unwrap();
            shell_capture_frames(&mut cx, handle);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            image.save(dir.join(format!("{name}-{theme_name}.png"))).expect("write the PNG");
        }
    }
}
