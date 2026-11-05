// Frame-paced benchmark of the Chart presentation against the Grid, hosted on GPUI's `BenchAppContext`
// (real multi-threaded dispatcher, the real `Root` + `AppRoot` window, the headless Metal renderer).
//
//   cargo test --release -p tablex-gpui --features bench chart_bench_context -- --ignored --nocapture --test-threads=1
//
// Two sessions:
//
// * idle: the same 100,000-row result shown as the Grid and as a Chart (bar, 6 bars), frames drawn
//   without any state change. Per frame: time, allocations, bytes, and how many times the tab's
//   results view was rebuilt (`surface::RENDERS`).
// * one million rows: the frame that switches to Chart starts the computation on the background
//   executor; the frames while it runs are timed, and so is the time until the chart is shown.
//
// No perf asserts: the numbers are informational.

#[derive(Clone, Copy, PartialEq)]
enum ChartBenchMode {
    GridIdle,
    ChartIdle,
    MillionRows,
}

struct ChartFrame {
    at: Instant,
    allocations: u64,
    bytes: u64,
    renders: u64,
    chart_ready: bool,
}

struct ChartSession {
    app: gpui::Entity<AppRoot>,
    mode: ChartBenchMode,
    stopped: Arc<AtomicBool>,
    stamps: Vec<ChartFrame>,
    log: Rc<RefCell<Vec<Vec<ChartFrame>>>>,
}

impl Drop for ChartSession {
    fn drop(&mut self) {
        self.log.borrow_mut().push(std::mem::take(&mut self.stamps));
    }
}

fn big_batch(rows: usize) -> arrow_array::RecordBatch {
    use arrow_array::{ArrayRef, Date32Array, Float64Array, StringArray};
    let names: Vec<String> = (0..1_000).map(|i| format!("customer-{i:04}")).collect();
    let customer: ArrayRef = Arc::new(StringArray::from_iter((0..rows).map(|i| Some(names[(i * 7919) % names.len()].as_str()))));
    let day: ArrayRef = Arc::new(Date32Array::from_iter_values((0..rows).map(|i| 18_000 + ((i * 31) % 3_650) as i32)));
    let amount: ArrayRef = Arc::new(Float64Array::from_iter((0..rows).map(|i| Some(((i * 37) % 9_000) as f64 / 7.0))));
    let seq: ArrayRef = Arc::new(Float64Array::from_iter_values((0..rows).map(|i| ((i * 7919) % rows) as f64)));
    arrow_array::RecordBatch::try_from_iter(vec![("customer", customer), ("day", day), ("amount", amount), ("seq", seq)]).unwrap()
}

fn open_chart_window(cx: &mut gpui::BenchAppContext<'_, '_>, mode: ChartBenchMode) -> (gpui::AnyWindowHandle, gpui::Entity<AppRoot>) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let handle = cx.update(|app| {
        let bounds = gpui::Bounds { origin: gpui::point(gpui::px(0.0), gpui::px(0.0)), size: gpui::size(gpui::px(1200.0), gpui::px(700.0)) };
        let handle = app
            .open_window(
                gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(bounds)), ..Default::default() },
                |window, cx| {
                    // The UI fixture, never the user's own connections.
                    let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "result-grid", "--ui-fixture", "sample"]).unwrap().unwrap();
                    let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                },
            )
            .expect("the benchmark window should open");
        app.update_window(handle.into(), |_, window, _| window.activate_window()).expect("the window activates");
        handle
    });
    let root = handle.entity(&*cx).expect("window root should be alive");
    let app = cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().expect("root should contain AppRoot"));
    let batch = match mode {
        ChartBenchMode::MillionRows => {
            let whole = big_batch(1_000_000);
            (0..10).map(|i| Arc::new(whole.slice(i * 100_000, 100_000))).collect::<Vec<_>>()
        }
        _ => vec![Arc::new(sales_batch(100_000))],
    };
    app.update(cx, |this, cx| {
        this.reset_active_view();
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(batch))));
        if mode == ChartBenchMode::ChartIdle {
            let tab = this.active_id();
            this.toggle_chart_presentation(tab, cx);
        }
        cx.notify();
    });
    cx.run_until_idle();
    (handle.into(), app)
}

fn bench_chart(name: &'static str, mode: ChartBenchMode, frames: u64) -> Vec<Vec<ChartFrame>> {
    let log: Rc<RefCell<Vec<Vec<ChartFrame>>>> = Rc::new(RefCell::new(Vec::new()));
    let mut criterion = criterion::Criterion::default()
        .with_measurement(gpui::BenchMeasurement::new(criterion::measurement::WallTime))
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(if mode == ChartBenchMode::MillionRows { 1 } else { 2 }))
        .without_plots();
    let report = gpui::BenchReport::with_frame_budget_nanos(FRAME_BUDGET_NANOS);
    criterion.bench_function(name, |bencher| {
        let mut cx = gpui::BenchAppContext::new_with_platform_and_report(
            gpui::bench_platform(
                Some(Box::new(gpui_platform::current_headless_renderer)),
                MAIN_THREAD_TEXT_SYSTEM.get().expect("text system is created by the load-time constructor").clone(),
            ),
            Some(name),
            bencher,
            report.clone(),
        );
        let mut window: Option<(gpui::AnyWindowHandle, gpui::Entity<AppRoot>)> = None;
        let log = log.clone();
        cx.bench_renderer_session(
            Duration::from_secs(60),
            |cx| {
                // The million-row session needs a fresh result every time (the chart is switched on in frame 1).
                let (handle, app) = if mode == ChartBenchMode::MillionRows {
                    open_chart_window(cx, mode)
                } else {
                    window.get_or_insert_with(|| open_chart_window(cx, mode)).clone()
                };
                let stopped = Arc::new(AtomicBool::new(false));
                (
                    ChartSession { app, mode, stopped: stopped.clone(), stamps: Vec::with_capacity(frames as usize + 1), log: log.clone() },
                    handle,
                    stopped,
                )
            },
            |session, frame, _, cx| {
                let tab = session.app.read_with(cx, |this, _| this.active_id());
                let chart_ready = session.app.read_with(cx, |this, _| matches!(this.av().chart.outcome_for_test(), Some(Ok(_))));
                session.stamps.push(ChartFrame {
                    at: Instant::now(),
                    allocations: crate::grid_bench::allocation_count(),
                    bytes: crate::grid_bench::allocation_bytes(),
                    renders: tab_renders(tab),
                    chart_ready,
                });
                // The million-row session runs until the chart has been on screen for a few frames.
                let ready_frames = session.stamps.iter().filter(|f| f.chart_ready).count();
                let done = if session.mode == ChartBenchMode::MillionRows { ready_frames >= 5 || frame >= 2_000_000 } else { frame >= frames };
                if done {
                    session.stopped.store(true, Ordering::Release);
                    return;
                }
                // Frame 1 of the million-row session switches to Chart, as a click on the rail button does.
                if session.mode == ChartBenchMode::MillionRows && frame == 1 {
                    session.app.update(cx, |this, cx| {
                        let tab = this.active_id();
                        this.toggle_chart_presentation(tab, cx);
                    });
                }
            },
        );
        cx.teardown();
    });
    let sessions = log.borrow_mut().drain(..).collect();
    sessions
}

#[test]
#[ignore = "on-demand render benchmark; run with --features bench --release --ignored --nocapture"]
fn chart_bench_context() {
    const IDLE_FRAMES: u64 = 240;
    eprintln!("idle frames: the same 100,000-row result, {IDLE_FRAMES} frames per session, warm sessions only");
    eprintln!("{:>10} {:>8} {:>9} {:>9} {:>9} {:>12} {:>10} {:>16}", "mode", "frames", "p50 ms", "p95 ms", "max ms", "allocs/frame", "KiB/frame", "renders/frame");
    for (label, mode) in [("Grid", ChartBenchMode::GridIdle), ("Chart", ChartBenchMode::ChartIdle)] {
        let sessions = bench_chart(if mode == ChartBenchMode::GridIdle { "chart_bench_grid_idle" } else { "chart_bench_chart_idle" }, mode, IDLE_FRAMES);
        let warm = if sessions.len() > 1 { &sessions[1..] } else { &sessions[..] };
        let (mut times, mut allocations, mut bytes, mut renders, mut count) = (Vec::new(), 0u64, 0u64, 0u64, 0u64);
        for stamps in warm {
            for pair in stamps.windows(2) {
                times.push(pair[1].at.duration_since(pair[0].at).as_secs_f64() * 1000.0);
                allocations += pair[1].allocations - pair[0].allocations;
                bytes += pair[1].bytes - pair[0].bytes;
                renders += pair[1].renders - pair[0].renders;
                count += 1;
            }
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "{:>10} {:>8} {:>9.3} {:>9.3} {:>9.3} {:>12.1} {:>10.2} {:>16.3}",
            label,
            times.len(),
            percentile(&times, 0.50),
            percentile(&times, 0.95),
            times.last().copied().unwrap_or_default(),
            allocations as f64 / count as f64,
            bytes as f64 / count as f64 / 1024.0,
            renders as f64 / count as f64,
        );
    }

    eprintln!("one million rows (1,000 customers, 3,650 days): switching to Chart (bar, SUM(amount) by customer)");
    let sessions = bench_chart("chart_bench_million_rows", ChartBenchMode::MillionRows, 400);
    let mut report = Vec::new();
    for stamps in &sessions {
        let Some(ready) = stamps.iter().position(|f| f.chart_ready) else { continue };
        let times: Vec<f64> = stamps.windows(2).map(|p| p[1].at.duration_since(p[0].at).as_secs_f64() * 1000.0).collect();
        // Frame 1 starts the computation; frames 2..ready are drawn while it runs.
        let while_preparing = &times[1.min(times.len())..ready.saturating_sub(1).max(1).min(times.len())];
        let before = &times[..1.min(times.len())];
        let waited = stamps[ready].at.duration_since(stamps[1].at).as_secs_f64() * 1000.0;
        report.push((before.iter().copied().fold(0.0, f64::max), while_preparing.iter().copied().fold(0.0, f64::max), while_preparing.len(), waited));
    }
    assert!(!report.is_empty(), "the chart never appeared");
    for (i, (first, max_while, frames, waited)) in report.iter().enumerate() {
        eprintln!(
            "  session {i}: the switch frame took {first:.2} ms; {frames} frames drawn while the rows were aggregated, the slowest {max_while:.2} ms; chart shown {waited:.0} ms after the click"
        );
    }
}
