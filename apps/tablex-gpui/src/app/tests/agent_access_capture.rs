// Offscreen captures of the "Agent access" controls (the connection dialog's Overrides tab and
// Settings › Query & Connections › Connection types), rendered through GPUI's headless Metal
// renderer with the real text system. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_agent_access_screens -- --ignored --nocapture --test-threads=1
//
// One PNG per screen and theme.

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_agent_access_screens() {
    use crate::settings::connection::AgentLevel;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    // (file name, scenario, own level, dropdown open)
    let screens = [
        ("dialog-default", "connection-overrides", None, false),
        ("dialog-read", "connection-overrides", Some(AgentLevel::Read), false),
        ("dialog-open", "connection-overrides", Some(AgentLevel::Schema), true),
        ("type-default", "settings", Some(AgentLevel::Off), false),
        ("type-read", "settings", Some(AgentLevel::Read), false),
        ("type-open", "settings", Some(AgentLevel::Read), true),
    ];
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for (name, scenario, level, open) in screens {
            let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", scenario, "--ui-fixture", "sample"])
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
            let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
            let settle = |cx: &mut gpui::HeadlessAppContext| {
                for _ in 0..12 {
                    cx.run_until_parked();
                    cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
            };
            if scenario_is_dialog(name) {
                cx.update_window(handle, |_, window, app_cx| {
                    app.update(app_cx, |this, cx| this.open_scenario_overrides_dialog(window, cx));
                })
                .unwrap();
                settle(&mut cx);
                let inputs = cx
                    .update_window(handle, |_, _, app_cx| app.read(app_cx).connection_ui.last_inputs.clone())
                    .unwrap()
                    .expect("the dialog opened");
                cx.update_window(handle, |_, window, app_cx| inputs.show_agent_for_capture(level, false, window, app_cx))
                    .unwrap();
                settle(&mut cx);
                if open {
                    cx.update_window(handle, |_, window, app_cx| inputs.show_agent_for_capture(level, true, window, app_cx))
                        .unwrap();
                    settle(&mut cx);
                }
            } else {
                let level = level.unwrap_or_default();
                cx.update_window(handle, |_, window, app_cx| {
                    app.update(app_cx, |this, cx| this.show_connection_types_for_capture(level, window, cx));
                })
                .unwrap();
                settle(&mut cx);
                if open {
                    let select = cx
                        .update_window(handle, |_, _, app_cx| app.read(app_cx).type_agent_select_for_capture())
                        .unwrap()
                        .expect("the type editor rendered");
                    cx.update_window(handle, |_, window, app_cx| {
                        select.update(app_cx, |select, cx| select.focus(window, cx));
                        window.dispatch_keystroke(gpui::Keystroke::parse("down").expect("a key"), app_cx);
                    })
                    .unwrap();
                    settle(&mut cx);
                }
            }
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("agent-{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}

fn scenario_is_dialog(name: &str) -> bool {
    name.starts_with("dialog")
}
