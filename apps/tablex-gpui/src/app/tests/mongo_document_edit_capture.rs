// Offscreen captures of the Mongo Document view: viewing a document, editing it, and a refused Save
// (light and dark theme, headless Metal renderer, the fixture data only). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_mongo_document_edit_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_mongo_document_edit_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (theme_name, mode) in [("dark", gpui_component::ThemeMode::Dark), ("light", gpui_component::ThemeMode::Light)] {
        capture_mongo_document_edit(&dir, theme_name, mode);
    }
}

fn capture_mongo_document_edit(dir: &std::path::Path, theme_name: &str, mode: gpui_component::ThemeMode) {

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
        .open_window(gpui::size(gpui::px(1280.0), gpui::px(820.0)), |window, cx| {
            // The UI fixture, never the user's own connections.
            let scenario =
                UiScenario::from_args(["tablex-gpui", "--ui-scenario", "mongo-grid", "--ui-fixture", "populated"])
                    .unwrap()
                    .unwrap();
            let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        })
        .expect("the capture window opens");
    let handle: gpui::AnyWindowHandle = window.into();
    let root = window.entity(&cx).expect("root");
    let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
    let app_entity = app.clone();
    let settle = |cx: &mut gpui::HeadlessAppContext| {
        for _ in 0..40 {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    };
    let shoot = |cx: &mut gpui::HeadlessAppContext, name: &str| {
        let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
        let path = dir.join(name);
        image.save(&path).expect("write the PNG");
        eprintln!("wrote {}", path.display());
    };
    let tab = cx
        .update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                let tab = this.active_id();
                this.workspace.runtime_mut(tab).unwrap().mongo.view = crate::mongo_result::MongoResultView::Document;
                this.select_mongo_document(tab, 1, cx);
                tab
            })
        })
        .unwrap();
    settle(&mut cx);
    shoot(&mut cx, &format!("mongo-document-view-{theme_name}.png"));

    cx.update_window(handle, |_, window, app_cx| {
        app.update(app_cx, |this, cx| this.start_mongo_document_edit(tab, window, cx))
    })
    .unwrap();
    settle(&mut cx);
    // The user's change: a renamed document, one field gone, one added.
    let editor = cx.update(|app| {
        app_entity.read(app).view(tab).mongo.document_edit.as_ref().expect("the editor opened").editor.clone()
    });
    let set_text = |cx: &mut gpui::HeadlessAppContext, text: String| {
        cx.update_window(handle, |_, window, app_cx| {
            editor.update(app_cx, |state, cx| state.set_value(text, window, cx))
        })
        .unwrap();
        settle(cx);
    };
    let seed = cx.update(|app| editor.read(app).value().to_string());
    let changed = seed
        .replace("\"mongo-customer-001\"", "\"Grace Hopper\"")
        .lines()
        .filter(|line| !line.contains("\"nullable\":"))
        .collect::<Vec<_>>()
        .join("\n")
        .replacen("{\n", "{\n  \"vip\": true,\n", 1);
    set_text(&mut cx, changed.clone());
    shoot(&mut cx, &format!("mongo-document-edit-{theme_name}.png"));

    // A refused Save: the error under the editor, the editor still open.
    set_text(&mut cx, changed.replacen("\"vip\": true,", "\"vip\": true,,", 1));
    cx.update_window(handle, |_, window, app_cx| {
        app.update(app_cx, |this, cx| this.save_mongo_document_edit(tab, window, cx))
    })
        .unwrap();
    settle(&mut cx);
    shoot(&mut cx, &format!("mongo-document-edit-error-{theme_name}.png"));
}
