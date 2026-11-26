// Offscreen captures of the Mongo find toolbar with empty FILTER / SORT boxes (their placeholders)
// and of the cell detail panel in its own window, rendered through GPUI's headless Metal renderer
// with the real text system (no desktop session needed). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_cell_detail_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_cell_detail_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (theme_name, mode) in [("dark", gpui_component::ThemeMode::Dark), ("light", gpui_component::ThemeMode::Light)] {
        capture_empty_find_boxes(&dir, theme_name, mode);
        capture_cell_detail_window(&dir, theme_name, mode);
    }
}

fn capture_empty_find_boxes(dir: &std::path::Path, theme_name: &str, mode: gpui_component::ThemeMode) {
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
                UiScenario::from_args(["tablex-gpui", "--ui-scenario", "mongo-grid", "--ui-fixture", "sample"])
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
        for _ in 0..40 {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    };
    settle(&mut cx);
    // Both boxes empty: what shows is their placeholder.
    cx.update_window(handle, |_, window, app_cx| {
        app.update(app_cx, |this, cx| {
            let tab = this.active_id();
            for editor in [this.mongo_filter_editor(tab, window, cx), this.mongo_sort_editor(tab, window, cx)] {
                editor.update(cx, |state, cx| state.set_value("", window, cx));
            }
        })
    })
    .unwrap();
    settle(&mut cx);
    let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
    let path = dir.join(format!("mongo-empty-filter-sort-{theme_name}.png"));
    image.save(&path).expect("write the PNG");
    eprintln!("wrote {}", path.display());
    // The text colour: the pixel farthest from the background (the region's corner).
    let ink = |x0: u32, x1: u32, y0: u32, y1: u32| {
        let bg = image.get_pixel(x0, y0).0;
        let distance = |p: [u8; 4]| (0..3).map(|c| (i32::from(p[c]) - i32::from(bg[c])).abs()).sum::<i32>();
        (y0..y1)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .map(|(x, y)| image.get_pixel(x, y).0)
            .max_by_key(|p| distance(*p))
            .unwrap()
    };
    // The hints over the empty boxes are the faintest text colour (the component's own placeholder, the
    // muted foreground, is not painted: the boxes have none). The sidebar's plain input keeps the muted one.
    let rgb = |color: gpui::Rgba| {
        [(color.r * 255.0).round() as u8, (color.g * 255.0).round() as u8, (color.b * 255.0).round() as u8]
    };
    for (what, region, expected) in [
        ("sort hint", (1632, 1840, 235, 280), rgb(theme::text_faint())),
        ("filter hint", (915, 975, 235, 280), rgb(theme::text_faint())),
        ("sidebar search placeholder", (60, 170, 240, 265), rgb(theme::text_muted())),
    ] {
        let found = ink(region.0, region.1, region.2, region.3);
        eprintln!("PIXELS {theme_name} {what} {found:?} (expected {expected:?})");
        assert_eq!(found[..3], expected, "{theme_name}: the {what} colour");
    }
    // Where the hint's text starts and sits: the same as the typed text of the box (device pixels).
    let extent = |image: &image::RgbaImage| {
        let (x0, x1, y0, y1) = (1575u32, 1900u32, 235u32, 280u32);
        let bg = image.get_pixel(x0, y0).0;
        let far = |p: [u8; 4]| (0..3).map(|c| (i32::from(p[c]) - i32::from(bg[c])).abs()).sum::<i32>() > 90;
        let ink: Vec<(u32, u32)> =
            (y0..y1).flat_map(|y| (x0..x1).map(move |x| (x, y))).filter(|(x, y)| far(image.get_pixel(*x, *y).0)).collect();
        // The SORT label ends left of this window and the clear button starts right of it.
        let left = ink.iter().map(|p| p.0).min().unwrap();
        let top = ink.iter().map(|p| p.1).min().unwrap();
        let bottom = ink.iter().map(|p| p.1).max().unwrap();
        (left, top, bottom)
    };
    let hint_extent = extent(&image);
    cx.update_window(handle, |_, window, app_cx| {
        app.update(app_cx, |this, cx| {
            let tab = this.active_id();
            let editor = this.mongo_sort_editor(tab, window, cx);
            editor.update(cx, |state, cx| state.set_value("{ createdAt: -1 }", window, cx));
        })
    })
    .unwrap();
    settle(&mut cx);
    let typed = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
    typed.save(dir.join(format!("mongo-typed-sort-{theme_name}.png"))).expect("write the PNG");
    let typed_extent = extent(&typed);
    eprintln!("EXTENT {theme_name} hint {hint_extent:?} typed {typed_extent:?}");
    assert!(hint_extent.0.abs_diff(typed_extent.0) <= 3, "{theme_name}: the hint starts where the text does");
    assert!(hint_extent.1.abs_diff(typed_extent.1) <= 3 && hint_extent.2.abs_diff(typed_extent.2) <= 3, "{theme_name}: and sits on its line");
}

/// The cell detail panel in a window of its own, next to the app's window it follows.
fn capture_cell_detail_window(dir: &std::path::Path, theme_name: &str, mode: gpui_component::ThemeMode) {
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
        for _ in 0..40 {
            cx.run_until_parked();
            cx.update_window(handle, |_, window, _| window.refresh()).unwrap();
            if let Some(detail) = cx.update(|app| app.windows().into_iter().find(|window| *window != handle)) {
                cx.update_window(detail, |_, window, _| window.refresh()).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    };
    settle(&mut cx);
    let tab = cx
        .update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                let tab = this.active_id();
                this.select_grid_cell(tab, crate::grid::CellPosition { row: 1, column: 1 });
                this.open_cell_detail_tab(tab, CellDetailTab::Details, cx);
                this.set_cell_detail_placement(tab, CellDetailPlacement::Window, cx);
                tab
            })
        })
        .unwrap();
    settle(&mut cx);
    let detail = cx.update(|app_cx| app.read(app_cx).cell_detail_window(tab)).expect("the panel's window");
    for (name, target) in [("app", handle), ("detail-window", detail)] {
        let image = cx.capture_screenshot(target).expect("the headless renderer draws the window");
        let path = dir.join(format!("cell-detail-{name}-{theme_name}.png"));
        image.save(&path).expect("write the PNG");
        eprintln!("wrote {}", path.display());
    }
}
