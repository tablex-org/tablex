// Offscreen captures of Charts from a result: the five chart types in the light and dark theme
// (headless Metal renderer, real text system, the production theme and asset source), with a pixel
// check that each chart really painted its series in the theme's colors. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_chart_screens -- --ignored --nocapture --test-threads=1

fn capture_sales_batch() -> arrow_array::RecordBatch {
    use arrow_array::{ArrayRef, Date32Array, Float64Array, Int64Array, StringArray};
    let regions = ["North", "South", "East", "West", "Central"];
    let teams = ["Retail", "Online", "Wholesale"];
    let mut rows = Vec::new();
    for month in 0..24usize {
        for (t, _) in teams.iter().enumerate() {
            for (r, _) in regions.iter().enumerate() {
                let season = 1.0 + 0.35 * ((month as f64 / 12.0) * std::f64::consts::TAU + t as f64).sin();
                let growth = 1.0 + month as f64 * 0.025 * (t as f64 + 1.0);
                // One sharp spike, so straight segments (no overshoot between points) are visible.
                let spike = if t == 1 && month == 9 { 2.6 } else { 1.0 };
                let revenue = 1_000.0 * (r as f64 + 2.0) * (t as f64 + 1.5) * season * growth * spike;
                let orders = (revenue / (18.0 + 6.0 * t as f64) + (r * 13 % 7) as f64 * 5.0) as i64;
                rows.push((month, t, r, revenue, orders));
            }
        }
    }
    let month: ArrayRef = Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| 19_358 + (r.0 as i32) * 30)));
    let team: ArrayRef = Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(teams[r.1]))));
    let region: ArrayRef = Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(regions[r.2]))));
    let revenue: ArrayRef = Arc::new(Float64Array::from_iter(rows.iter().map(|r| Some((r.3 * 100.0).round() / 100.0))));
    let orders: ArrayRef = Arc::new(Int64Array::from_iter(rows.iter().map(|r| Some(r.4))));
    arrow_array::RecordBatch::try_from_iter(vec![("month", month), ("team", team), ("region", region), ("revenue", revenue), ("orders", orders)])
        .expect("a valid batch")
}

/// Pixels within `tolerance` of `color` in the part of the image where the plot is.
fn pixels_near(image: &image::RgbaImage, region: (u32, u32, u32, u32), color: [u8; 3], tolerance: i32) -> usize {
    let (x0, y0, x1, y1) = region;
    let mut count = 0;
    for y in y0..y1.min(image.height()) {
        for x in x0..x1.min(image.width()) {
            let p = image.get_pixel(x, y).0;
            if (0..3).all(|c| (i32::from(p[c]) - i32::from(color[c])).abs() <= tolerance) {
                count += 1;
            }
        }
    }
    count
}

/// The lengths of the vertical runs of pixels within `tolerance` of `color` down the column `x`, between `y0` and `y1`.
fn vertical_runs(image: &image::RgbaImage, x: u32, (y0, y1): (u32, u32), color: [u8; 3], tolerance: i32) -> Vec<u32> {
    let mut runs = Vec::new();
    let mut length = 0;
    for y in y0..y1.min(image.height()) {
        let p = image.get_pixel(x, y).0;
        if (0..3).all(|c| (i32::from(p[c]) - i32::from(color[c])).abs() <= tolerance) {
            length += 1;
        } else if length > 0 {
            runs.push(length);
            length = 0;
        }
    }
    if length > 0 {
        runs.push(length);
    }
    runs
}

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_chart_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
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
                let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "result-grid", "--ui-fixture", "sample"]).unwrap().unwrap();
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
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.reset_active_view();
                this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
                    crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(capture_sales_batch())]),
                )));
                let tab = this.active_id();
                this.toggle_chart_presentation(tab, cx);
            })
        })
        .unwrap();
        settle(&mut cx);

        let background = results::chart::svg_style().background;
        let palette = results::chart::svg_style().palette;
        // (name, edit): month = 0, team = 1, region = 2, revenue = 3, orders = 4.
        type Edit = fn(&mut crate::chart_data::ChartSpec);
        let shots: [(&str, Edit); 5] = [
            ("bar", |s| {
                s.kind = crate::chart_data::ChartKind::Bar;
                s.x = 2;
                s.values = vec![3];
                s.aggregation = crate::chart_data::Aggregation::Sum;
                s.series_by = Some(1);
            }),
            ("line", |s| {
                s.kind = crate::chart_data::ChartKind::Line;
                s.x = 0;
                s.values = vec![3];
                s.aggregation = crate::chart_data::Aggregation::Sum;
                s.series_by = Some(1);
            }),
            ("area", |s| {
                s.kind = crate::chart_data::ChartKind::Area;
                s.x = 0;
                s.values = vec![3];
                s.aggregation = crate::chart_data::Aggregation::Sum;
                s.series_by = Some(1);
            }),
            ("scatter", |s| {
                s.kind = crate::chart_data::ChartKind::Scatter;
                s.x = 4;
                s.values = vec![3];
                s.aggregation = crate::chart_data::Aggregation::None;
                s.series_by = None;
            }),
            ("pie", |s| {
                s.kind = crate::chart_data::ChartKind::Pie;
                s.x = 2;
                s.values = vec![3];
                s.aggregation = crate::chart_data::Aggregation::Sum;
                s.series_by = None;
            }),
        ];
        for (name, edit) in shots {
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let tab = this.active_id();
                    this.view_mut(tab).chart.edit_spec_for_test(edit);
                    this.notify_tab(tab, cx);
                })
            })
            .unwrap();
            settle(&mut cx);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("chart-{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());

            // The pixels: the plot area (right of the 260px sidebar and the rail, under the settings
            // strip) holds the series colors, not just the background.
            let region = (560, 330, 2560, 1560);
            let first = pixels_near(&image, region, palette[0], 6);
            let second = pixels_near(&image, region, palette[1], 6);
            let other_than_background = {
                let (x0, y0, x1, y1) = region;
                (y0..y1.min(image.height())).flat_map(|y| (x0..x1.min(image.width())).map(move |x| (x, y))).filter(|(x, y)| image.get_pixel(*x, *y).0[..3] != background).count()
            };
            eprintln!("  {name}/{theme_name}: {first} px in chart_1, {second} px in chart_2, {other_than_background} px off the background");
            assert!(first > 150, "{name}/{theme_name}: the first series is painted ({first} px)");
            // An area's fill is translucent: only its outline is the exact series color.
            let least = if name == "area" { 150 } else { 400 };
            if matches!(name, "bar" | "line" | "area" | "pie") {
                assert!(second > least, "{name}/{theme_name}: the second series / slice is painted ({second} px)");
            }
            assert!(other_than_background > 5_000, "{name}/{theme_name}: only {other_than_background} px differ from the background");
        }

        // Lines and areas break where a mean has no row: series "b" has none for x = 4, 5 and 6, so it is two pieces.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                use arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};
                let rows: Vec<(i64, &str, f64)> = (0..12i64)
                    .flat_map(|k| [(k, "a", 20.0 + 15.0 * (k as f64 / 2.0).sin() + k as f64), (k, "b", 40.0 - 12.0 * (k as f64 / 3.0).cos())])
                    .filter(|(k, by, _)| !(*by == "b" && (4..=6).contains(k)))
                    .collect();
                let k: ArrayRef = Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.0)));
                let by: ArrayRef = Arc::new(StringArray::from_iter(rows.iter().map(|r| Some(r.1))));
                let v: ArrayRef = Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.2)));
                let batch = arrow_array::RecordBatch::try_from_iter(vec![("k", k), ("series", by), ("value", v)]).expect("a valid batch");
                this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(batch)]))));
                this.notify_tab(this.active_id(), cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        for (name, kind) in [("line-gaps", crate::chart_data::ChartKind::Line), ("area-gaps", crate::chart_data::ChartKind::Area)] {
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let tab = this.active_id();
                    this.view_mut(tab).chart.edit_spec_for_test(|s| {
                        s.kind = kind;
                        s.x = 0;
                        s.values = vec![2];
                        s.aggregation = crate::chart_data::Aggregation::Avg;
                        s.series_by = Some(1);
                    });
                    this.notify_tab(tab, cx);
                })
            })
            .unwrap();
            settle(&mut cx);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("chart-{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
            assert!(pixels_near(&image, (560, 330, 2560, 1560), palette[1], 6) > 100, "{name}/{theme_name}: the second series is painted");
        }

        // How wide the strokes are: one flat series, so the line is a horizontal bar of pixels. The window is drawn at twice its
        // size. A line takes `LINE_STROKE` logical pixels (what the official `LineChart` draws), the outline of an area
        // `AREA_STROKE`; the exact series color is the core of the stroke, its anti-aliased edge is blended.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                use arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};
                let k: ArrayRef = Arc::new(Int64Array::from_iter_values(0..12i64));
                let by: ArrayRef = Arc::new(StringArray::from_iter((0..12).map(|_| Some("a"))));
                let v: ArrayRef = Arc::new(Float64Array::from_iter_values((0..12).map(|_| 30.0)));
                let batch = arrow_array::RecordBatch::try_from_iter(vec![("k", k), ("series", by), ("value", v)]).expect("a valid batch");
                this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(batch)]))));
                this.notify_tab(this.active_id(), cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        let mut thickness = Vec::new();
        for (name, kind, logical) in [("line", crate::chart_data::ChartKind::Line, crate::chart_data::LINE_STROKE), ("area", crate::chart_data::ChartKind::Area, crate::chart_data::AREA_STROKE)] {
            cx.update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let tab = this.active_id();
                    this.view_mut(tab).chart.edit_spec_for_test(|s| {
                        s.kind = kind;
                        s.x = 0;
                        s.values = vec![2];
                        s.aggregation = crate::chart_data::Aggregation::Sum;
                        s.series_by = None;
                    });
                    this.notify_tab(tab, cx);
                })
            })
            .unwrap();
            settle(&mut cx);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let scale = image.width() as f32 / 1280.0;
            // A column in the middle of the plot, away from the end points and the axes.
            let runs = vertical_runs(&image, image.width() / 2 + 200, (330, 1560), palette[0], 6);
            eprintln!("  {name}/{theme_name}: runs of the series color down a column: {runs:?} (scale {scale})");
            assert_eq!(runs.len(), 1, "{name}/{theme_name}: one flat stroke crosses the column: {runs:?}");
            let device = runs[0] as f32;
            // The solid core is the stroke less what the anti-aliased edges take (up to a device pixel each side).
            assert!(device >= logical * scale - 2.0 && device <= logical * scale + 0.5, "{name}/{theme_name}: a stroke of {logical} px is {device} device px wide at scale {scale}");
            thickness.push(device);
        }
        assert!(thickness[0] > thickness[1], "{theme_name}: a line ({}) is stroked wider than the outline of an area ({})", thickness[0], thickness[1]);
    }
}
