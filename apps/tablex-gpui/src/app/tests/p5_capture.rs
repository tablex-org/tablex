// Offscreen captures of the result grid's record view, binary value panel (hex, image) and pinned
// columns (headless Metal renderer, real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_result_grid_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_result_grid_screens() {
    use arrow_array::{ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray};

    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let png: &[u8] = include_bytes!("fixtures/gradient.png");
    let jpeg: &[u8] = include_bytes!("fixtures/gradient.jpg");
    let customers = ["Acme Corp", "Globex", "Initech", "Umbrella", "Hooli", "Stark Industries"];
    let statuses = ["paid", "shipped", "pending", "paid"];
    let orders = || {
        let ids: Vec<i64> = (1..=18).collect();
        let text = |f: &dyn Fn(i64) -> String| {
            Arc::new(StringArray::from_iter_values(ids.iter().map(|i| f(*i)))) as ArrayRef
        };
        let mut columns: Vec<(String, ArrayRef)> = vec![
            ("id".into(), Arc::new(Int64Array::from(ids.clone())) as ArrayRef),
            ("customer".into(), text(&|i| customers[i as usize % 6].to_string())),
            (
                "total".into(),
                Arc::new(Float64Array::from_iter_values(ids.iter().map(|i| 120.0 + (*i as f64) * 37.25))) as ArrayRef,
            ),
            ("status".into(), text(&|i| statuses[i as usize % 4].to_string())),
            ("created_at".into(), text(&|i| format!("2026-10-{:02} 09:{:02}:00", i, (i * 7) % 60))),
            ("notes".into(), text(&|i| format!("Order {i}: handle with care, deliver before noon"))),
            (
                "avatar".into(),
                Arc::new(BinaryArray::from_iter(ids.iter().map(|i| {
                    Some(if i % 2 == 1 { png } else { jpeg })
                }))) as ArrayRef,
            ),
            (
                "attachment".into(),
                Arc::new(BinaryArray::from_iter(ids.iter().map(|i| {
                    Some((0..300u32).map(|n| (n as i64 * 7 + i * 13) as u8).collect::<Vec<u8>>())
                }))) as ArrayRef,
            ),
        ];
        for extra in 0..10 {
            columns.push((
                format!("metric_{extra}"),
                text(&|i| format!("{:.2}", (i * (extra + 3)) as f64 / 7.0)),
            ));
        }
        let batch = RecordBatch::try_from_iter(columns).unwrap();
        ArrowGridSource::new(vec![Arc::new(batch)])
    };

    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "result-grid", "--ui-fixture", "sample"])
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
        let tab = cx
            .update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let tab = this.active_id();
                    this.workspace.runtime_mut(tab).unwrap().grid =
                        GridState::new(GridContentSource::Arrow(orders()));
                    this.reset_active_view();
                    cx.notify();
                    tab
                })
            })
            .unwrap();
        settle(&mut cx);

        // Pinned columns: id and customer stay while the grid is scrolled to the right.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.pin_column(tab, 0, cx);
                this.pin_column(tab, 1, cx);
                this.select_grid_cell(tab, CellPosition { row: 3, column: 5 });
                this.av_mut().grid_horizontal_scroll.set_offset(gpui::point(gpui::px(-640.0), gpui::px(0.0)));
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "pinned-columns");

        // Image preview: the avatar of an odd row is a PNG, in the Details panel.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.unpin_all_columns(tab, cx);
                this.av_mut().grid_horizontal_scroll.set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                let position = CellPosition { row: 2, column: 6 };
                this.select_grid_cell(tab, position);
                this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
                this.workspace.runtime_mut(tab).unwrap().cell_detail.right_width = 400.0;
                this.av_mut().grid_horizontal_scroll.set_offset(gpui::point(gpui::px(-330.0), gpui::px(0.0)));
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "image-preview");

        // Hex view of the attachment.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                let position = CellPosition { row: 2, column: 7 };
                this.select_grid_cell(tab, position);
                this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "hex-view");

        // The record view of row 3 with the avatar field selected, the panel following it.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.workspace.runtime_mut(tab).unwrap().cell_detail.close();
                this.select_grid_cell(tab, CellPosition { row: 2, column: 5 });
                this.toggle_record_view(tab, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "record-view");
    }
}
