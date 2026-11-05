// Frame-paced scroll benchmark for the results grid, hosted on GPUI's
// `BenchAppContext` (gpui-pre `bench-support`). It opens the real
// `Root` + `AppRoot` window (not a stand-in view), loads a 60 x 100 Arrow
// fixture (`wide_grid_batch`, shared with `grid_render_allocations`) and drives
// `N_FRAMES` frames of vertical scrolling and then `N_FRAMES` frames of
// horizontal scrolling through the headless Metal renderer, so layout, paint,
// text shaping and GPU submission are all included.
//
// Run on demand (dev-only `bench` feature; never part of default/test builds):
//
//   cargo test --release -p tablex-gpui --features bench grid_scroll_bench_context -- --ignored --nocapture --test-threads=1
//
// Output per axis: a frame-time table (p50/p95/max per frame plus allocations
// per frame from the crate's counting test allocator), then GPUI's own
// `BenchReport` (window draw histogram, foreground executor work). Criterion
// repeats the session for warm-up and sampling; the GPUI report aggregates
// every observed session, while the table uses the sessions after the first
// (warm) one. No perf asserts: the numbers are informational.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// macOS creates the real (CoreText) text system only on the process main
/// thread (`MacPlatform::new` asserts it), but libtest runs every test on a
/// spawned thread, even with `--test-threads=1`. A load-time constructor runs
/// on the main thread, so it builds the shared text system once and the test
/// thread borrows it. Bench-only: this module is behind the `bench` feature.
static MAIN_THREAD_TEXT_SYSTEM: std::sync::OnceLock<Arc<dyn gpui::PlatformTextSystem>> =
    std::sync::OnceLock::new();

/// Mach-O module initializer (what the `ctor` crate emits on macOS): dyld runs
/// it on the main thread before `main`.
#[used]
#[link_section = "__DATA,__mod_init_func"]
static INIT_BENCH_TEXT_SYSTEM: extern "C" fn() = {
    extern "C" fn init() {
        let _ = MAIN_THREAD_TEXT_SYSTEM.set(gpui_platform::bench_text_system());
    }
    init
};

const BENCH_ROWS: usize = 60;
const BENCH_COLUMNS: usize = 100;
const N_FRAMES: u64 = 240;
/// 60 Hz budget used for the report's frame-budget-overrun counters.
const FRAME_BUDGET_NANOS: u128 = 16_600_000;

#[derive(Clone, Copy)]
enum ScrollAxis {
    Vertical,
    Horizontal,
    /// Horizontal scroll with two pinned columns (the sticky block repainted
    /// at the viewport's edge on every frame).
    HorizontalPinned,
    /// Vertical scroll of the record view's field list.
    RecordVertical,
}

/// Columns pinned in the `HorizontalPinned` run.
const BENCH_PINNED: usize = 2;

/// One stamp per input callback; consecutive stamps bracket one frame of
/// scroll update + layout + paint + present.
#[derive(Clone, Copy)]
struct FrameStamp {
    at: Instant,
    allocations: u64,
    bytes: u64,
}

type SessionLog = Rc<RefCell<Vec<Vec<FrameStamp>>>>;

/// Session state. Dropping it (after the timed section) hands its frame
/// stamps to the benchmark.
struct ScrollSession {
    app: gpui::Entity<AppRoot>,
    axis: ScrollAxis,
    max_offset: f32,
    stopped: Arc<AtomicBool>,
    stamps: Vec<FrameStamp>,
    log: SessionLog,
}

impl ScrollSession {
    /// Triangle wave over the scrollable extent: the grid scrolls in both
    /// directions and lands on different rows/columns every frame.
    fn offset_for(&self, frame: u64) -> f32 {
        const SWEEP: u64 = 60;
        let phase = (frame % (2 * SWEEP)) as f32 / SWEEP as f32;
        let t = if phase <= 1.0 { phase } else { 2.0 - phase };
        t * self.max_offset
    }

    fn stamp(&mut self) {
        // The vector is pre-sized, so stamping does not allocate.
        self.stamps.push(FrameStamp {
            at: Instant::now(),
            allocations: crate::grid_bench::allocation_count(),
            bytes: crate::grid_bench::allocation_bytes(),
        });
    }
}

impl Drop for ScrollSession {
    fn drop(&mut self) {
        self.log.borrow_mut().push(std::mem::take(&mut self.stamps));
    }
}

fn open_wide_grid_window(
    cx: &mut gpui::BenchAppContext<'_, '_>,
    axis: ScrollAxis,
) -> (gpui::AnyWindowHandle, gpui::Entity<AppRoot>) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let handle = cx.update(|app| {
        let bounds = gpui::Bounds {
            origin: gpui::point(gpui::px(0.0), gpui::px(0.0)),
            size: gpui::size(gpui::px(1200.0), gpui::px(700.0)),
        };
        let handle = app
            .open_window(
                gpui::WindowOptions {
                    window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(AppRoot::new_for_test);
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                },
            )
            .expect("the benchmark window should open");
        // As `BenchAppContext::add_empty_window` does: an inactive window is
        // frame-throttled, which would hide the cost being measured.
        app.update_window(handle.into(), |_, window, _| window.activate_window())
            .expect("the benchmark window should activate");
        handle
    });
    let root = handle.entity(&*cx).expect("window root should be alive");
    let app = cx.read(|cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<AppRoot>()
            .expect("root should contain AppRoot")
    });
    let batch = wide_grid_batch(BENCH_ROWS, BENCH_COLUMNS);
    app.update(cx, |this, cx| {
        // A tab that never ran shows the full-height editor and no grid.
        this.workspace.open_ran_query_tab("bench");
        this.reset_active_view();
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
            crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(batch)]),
        )));
        let tab = this.active_id();
        match axis {
            ScrollAxis::HorizontalPinned => {
                for column in 0..BENCH_PINNED {
                    this.pin_column(tab, column, cx);
                }
            }
            ScrollAxis::RecordVertical => this.toggle_record_view(tab, cx),
            ScrollAxis::Vertical | ScrollAxis::Horizontal => {}
        }
        cx.notify();
    });
    cx.run_until_idle();
    (handle.into(), app)
}

/// Runs one Criterion benchmark of `N_FRAMES` scrolled frames along `axis`
/// and returns the frame stamps of every session Criterion ran.
fn bench_scroll(
    name: &'static str,
    axis: ScrollAxis,
    report: &gpui::BenchReport,
) -> Vec<Vec<FrameStamp>> {
    let log: SessionLog = Rc::new(RefCell::new(Vec::new()));
    let mut criterion = criterion::Criterion::default()
        .with_measurement(gpui::BenchMeasurement::new(
            criterion::measurement::WallTime,
        ))
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2))
        .without_plots();
    criterion.bench_function(name, |bencher| {
        let mut cx = gpui::BenchAppContext::new_with_platform_and_report(
            gpui::bench_platform(
                Some(Box::new(gpui_platform::current_headless_renderer)),
                MAIN_THREAD_TEXT_SYSTEM
                    .get()
                    .expect("text system is created by the load-time constructor")
                    .clone(),
            ),
            Some(name),
            bencher,
            report.clone(),
        );
        // Criterion calls this routine repeatedly; each call owns a fresh app
        // with one window that its sessions reuse.
        let mut window: Option<(gpui::AnyWindowHandle, gpui::Entity<AppRoot>)> = None;
        let log = log.clone();
        cx.bench_renderer_session(
            Duration::from_secs(60),
            |cx| {
                let (handle, app) = window
                    .get_or_insert_with(|| open_wide_grid_window(cx, axis))
                    .clone();
                let max_offset = app.update(cx, |this, _| {
                    let base = this.av().grid_scroll.0.borrow().base_handle.clone();
                    base.set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                    this.av().grid_horizontal_scroll
                        .set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                    match axis {
                        // The fixture height minus roughly one viewport.
                        ScrollAxis::Vertical => {
                            (BENCH_ROWS as f32 * theme::ROW_HEIGHT - 400.0).max(theme::ROW_HEIGHT)
                        }
                        ScrollAxis::Horizontal | ScrollAxis::HorizontalPinned => {
                            f32::from(this.av().grid_horizontal_scroll.max_offset().x)
                        }
                        ScrollAxis::RecordVertical => (BENCH_COLUMNS as f32 * 28.0 - 400.0).max(28.0),
                    }
                });
                assert!(
                    max_offset > 0.0,
                    "the fixture must overflow along the scroll axis"
                );
                let stopped = Arc::new(AtomicBool::new(false));
                (
                    ScrollSession {
                        app,
                        axis,
                        max_offset,
                        stopped: stopped.clone(),
                        stamps: Vec::with_capacity(N_FRAMES as usize + 1),
                        log: log.clone(),
                    },
                    handle,
                    stopped,
                )
            },
            |session, frame, _, cx| {
                session.stamp();
                if frame >= N_FRAMES {
                    session.stopped.store(true, Ordering::Release);
                    return;
                }
                let offset = session.offset_for(frame);
                let axis = session.axis;
                session.app.update(cx, |this, cx| {
                    match axis {
                        ScrollAxis::Vertical => {
                            let base = this.av().grid_scroll.0.borrow().base_handle.clone();
                            base.set_offset(gpui::point(base.offset().x, gpui::px(-offset)));
                        }
                        ScrollAxis::Horizontal | ScrollAxis::HorizontalPinned => {
                            let current = this.av().grid_horizontal_scroll.offset();
                            this.av().grid_horizontal_scroll
                                .set_offset(gpui::point(gpui::px(-offset), current.y));
                        }
                        ScrollAxis::RecordVertical => {
                            let base = this.av().record_scroll.0.borrow().base_handle.clone();
                            base.set_offset(gpui::point(base.offset().x, gpui::px(-offset)));
                        }
                    }
                    cx.notify();
                });
            },
        );
        cx.teardown();
    });
    let sessions = log.borrow().clone();
    sessions
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

struct AxisSummary {
    frames: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    allocations_per_frame: f64,
    max_allocations: u64,
    kib_per_frame: f64,
}

/// Summarises the warm sessions (every session after the first).
fn summarise(sessions: &[Vec<FrameStamp>]) -> AxisSummary {
    let warm = if sessions.len() > 1 {
        &sessions[1..]
    } else {
        sessions
    };
    let mut times = Vec::new();
    let mut allocations = Vec::new();
    let mut bytes = 0u64;
    for stamps in warm {
        for pair in stamps.windows(2) {
            times.push(pair[1].at.duration_since(pair[0].at).as_secs_f64() * 1000.0);
            allocations.push(pair[1].allocations - pair[0].allocations);
            bytes += pair[1].bytes - pair[0].bytes;
        }
    }
    assert!(!times.is_empty(), "the benchmark recorded no frames");
    times.sort_by(f64::total_cmp);
    AxisSummary {
        frames: times.len(),
        p50_ms: percentile(&times, 0.50),
        p95_ms: percentile(&times, 0.95),
        max_ms: *times.last().unwrap(),
        allocations_per_frame: allocations.iter().sum::<u64>() as f64 / allocations.len() as f64,
        max_allocations: allocations.iter().copied().max().unwrap_or_default(),
        kib_per_frame: bytes as f64 / allocations.len() as f64 / 1024.0,
    }
}

#[test]
#[ignore = "on-demand render benchmark; run with --features bench --release --ignored --nocapture"]
fn grid_scroll_bench_context() {
    let mut rows = Vec::new();
    for (name, axis, label) in [
        ("grid_scroll_vertical", ScrollAxis::Vertical, "vertical"),
        (
            "grid_scroll_horizontal",
            ScrollAxis::Horizontal,
            "horizontal",
        ),
        (
            "grid_scroll_horizontal_pinned",
            ScrollAxis::HorizontalPinned,
            "horiz+pinned",
        ),
        (
            "record_scroll_vertical",
            ScrollAxis::RecordVertical,
            "record",
        ),
    ] {
        let report = gpui::BenchReport::with_frame_budget_nanos(FRAME_BUDGET_NANOS);
        let sessions = bench_scroll(name, axis, &report);
        report.print(name);
        if let Some(work) = report.foreground_work() {
            eprintln!(
                "  foreground work ({label}): {} items, total {:?}, p50 {:?}, p95 {:?}, max {:?}, \
                 frame-budget overruns total {} / max {}",
                work.count,
                work.total,
                work.p50,
                work.p95,
                work.max,
                work.frame_budget_overruns_total,
                work.frame_budget_overruns_max,
            );
        }
        rows.push((label, summarise(&sessions), sessions.len()));
    }
    eprintln!(
        "grid scroll frame times: {BENCH_ROWS} rows x {BENCH_COLUMNS} cols, {N_FRAMES} frames per \
         session, warm sessions only"
    );
    eprintln!(
        "{:>11} {:>8} {:>9} {:>9} {:>9} {:>12} {:>11} {:>10}",
        "axis", "frames", "p50 ms", "p95 ms", "max ms", "allocs/frame", "max allocs", "KiB/frame"
    );
    for (label, s, sessions) in &rows {
        eprintln!(
            "{:>11} {:>8} {:>9.3} {:>9.3} {:>9.3} {:>12.0} {:>11} {:>10.1}   ({sessions} sessions)",
            label,
            s.frames,
            s.p50_ms,
            s.p95_ms,
            s.max_ms,
            s.allocations_per_frame,
            s.max_allocations,
            s.kib_per_frame,
        );
    }
}
