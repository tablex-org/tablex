// Frame-paced benchmark of the left pane (the connection / object tree), hosted like
// `grid_bench_context.rs` on GPUI's `BenchAppContext`: the real `Root` + `AppRoot` window, headless
// Metal, one input callback per frame, allocations from the crate's counting allocator. Run on demand:
//
//   RUST_MIN_STACK=16777216 cargo test --release -j 4 -p tablex-gpui --features bench sidebar_bench_context -- --ignored --nocapture --test-threads=1
//
// Cases (each one a Criterion session, warm sessions only in the table):
//
// * scroll: the sidebar list scrolled `SB_FRAMES` frames over a tiny / 3 x 1,000 / 1 x 10,000 table tree.
// * idle: `AppRoot` notified every frame, nothing else changes (the cost of an unrelated notify),
//   also with the sidebar collapsed (the shell without the tree).
// * frequency: AppRoot renders (`FRAME_COUNT`) and navigator list passes / rows built per frame for typing 30
//   characters into a SQL editor, hovering 20 sidebar rows, a Connecting dot for 2 s, scrolling the results grid,
//   (a scroll notifies the tab only, as the wheel does) and how many times the cached sidebar view itself was rendered per frame (`sb/fr`).
//
// No perf asserts: the numbers are informational.

const SB_FRAMES: u64 = 240;
const SB_TYPED: &str = "select * from orders where id ";
const SB_HOVER_ROWS: u64 = 20;
const SB_PULSE_SECONDS: u64 = 2;

#[derive(Clone, Copy, PartialEq)]
enum SbTree {
    /// 3 schemas x 3 tables, every Tables folder open.
    Tiny,
    /// `schemas` schemas x `tables` tables, every Tables folder open.
    Big { schemas: usize, tables: usize },
}

#[derive(Clone, Copy, PartialEq)]
enum SbAction {
    /// The sidebar list scrolled along a triangle wave.
    Scroll,
    /// `cx.notify()` on AppRoot every frame, nothing else.
    IdleNotify,
    /// One character typed into the focused SQL editor per frame.
    Typing,
    /// The mouse moves one sidebar row further per frame.
    Hover,
    /// The mouse moves a pixel at a time inside one sidebar row.
    HoverJitter,
    /// A connection in Connecting state; frames run (with sleeps) for `SB_PULSE_SECONDS`.
    Pulse,
    /// The same wait without a Connecting row: what else redraws on its own.
    Quiet,
    /// The results grid scrolled vertically (a 60 x 100 grid, as `grid_bench_context`).
    GridScroll,
}

#[derive(Clone, Copy)]
struct SbCase {
    label: &'static str,
    tree: SbTree,
    action: SbAction,
    /// The sidebar collapsed to its 36px rail (no navigator render).
    hidden: bool,
    /// A table row is selected (the usual state once the user clicked anything).
    selected: bool,
}

#[derive(Clone, Copy)]
struct SbStamp {
    at: Instant,
    allocations: u64,
    bytes: u64,
    renders: u64,
    list_passes: u64,
    rows_built: u64,
    sidebar_renders: u64,
}

fn sb_now_stamp() -> SbStamp {
    SbStamp {
        at: Instant::now(),
        allocations: crate::grid_bench::allocation_count(),
        bytes: crate::grid_bench::allocation_bytes(),
        renders: crate::app::frame_count(),
        list_passes: crate::app::navigator_ui::LIST_PASSES.load(Ordering::Relaxed),
        rows_built: crate::app::navigator_ui::ROWS_BUILT.load(Ordering::Relaxed),
        sidebar_renders: crate::app::navigator_ui::sidebar_renders(),
    }
}

type SbLog = Rc<RefCell<Vec<Vec<SbStamp>>>>;

struct SbSession {
    app: gpui::Entity<AppRoot>,
    case: SbCase,
    max_offset: f32,
    started: Option<Instant>,
    stopped: Arc<AtomicBool>,
    stamps: Vec<SbStamp>,
    log: SbLog,
}

impl Drop for SbSession {
    fn drop(&mut self) {
        self.log.borrow_mut().push(std::mem::take(&mut self.stamps));
    }
}

/// Expands `connection` (Postgres of the sample fixture) down to its Tables folders and fills each with
/// `tables` tables, through the same model calls a completed load makes.
fn sb_seed_tree(nav: &mut crate::navigator::NavigatorState, schemas: usize, tables: usize) {
    use crate::navigator::{
        database_child, object_child, object_group_id, schema_child, KindOfObject, NavigatorNodeId, ObjectGroup,
    };
    let conn = crate::ui_fixture::POSTGRES_ID;
    let root = NavigatorNodeId::Connection { connection: conn.into() };
    let request = nav.toggle_expansion(&root).request.expect("databases request");
    nav.apply_load(&request, nav.load_generation(&request).unwrap(), Ok(vec![database_child(conn, "app")]));
    let database = NavigatorNodeId::Database { connection: conn.into(), database: "app".into() };
    let request = nav.toggle_expansion(&database).request.expect("schemas request");
    let children = (0..schemas).map(|s| schema_child(conn, "app", &format!("schema{s}"))).collect();
    nav.apply_load(&request, nav.load_generation(&request).unwrap(), Ok(children));
    for s in 0..schemas {
        let schema_name = format!("schema{s}");
        let schema = NavigatorNodeId::Schema { connection: conn.into(), database: "app".into(), schema: schema_name.clone() };
        nav.toggle_expansion(&schema);
        let group = object_group_id(conn, "app", &schema_name, ObjectGroup::Tables);
        let request = nav.toggle_expansion(&group).request.expect("objects request");
        let children = (0..tables)
            .map(|t| object_child(conn, "app", &schema_name, &format!("table_{t:05}"), KindOfObject::Table))
            .collect();
        nav.apply_load(&request, nav.load_generation(&request).unwrap(), Ok(children));
    }
}

fn open_sidebar_window(cx: &mut gpui::BenchAppContext<'_, '_>, case: SbCase) -> (gpui::AnyWindowHandle, gpui::Entity<AppRoot>) {
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
                gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(bounds)), ..Default::default() },
                |window, cx| {
                    let view = cx.new(AppRoot::new_for_test);
                    cx.new(|cx| gpui_component::Root::new(view, window, cx))
                },
            )
            .expect("the benchmark window should open");
        app.update_window(handle.into(), |_, window, _| window.activate_window()).expect("the window activates");
        handle
    });
    let root = handle.entity(&*cx).expect("window root should be alive");
    let app = cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().expect("root should contain AppRoot"));
    app.update(cx, |this, cx| {
        let (schemas, tables) = match case.tree {
            SbTree::Tiny => (3, 3),
            SbTree::Big { schemas, tables } => (schemas, tables),
        };
        sb_seed_tree(&mut this.navigator, schemas, tables);
        // The editor tab (typing) or a grid tab (grid scroll); the sidebar cases keep a fresh SQL tab too.
        if case.action == SbAction::GridScroll {
            this.workspace.open_ran_query_tab("bench");
            this.reset_active_view();
            let batch = wide_grid_batch(60, 100);
            this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(
                crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(batch)]),
            )));
        } else {
            this.workspace.open_query_tab("bench");
        }
        this.shell.sidebar_collapsed = case.hidden;
        if case.selected {
            let id = crate::navigator::NavigatorNodeId::Object {
                connection: crate::ui_fixture::POSTGRES_ID.into(),
                database: "app".into(),
                schema: "schema0".into(),
                object: "table_00002".into(),
            };
            this.navigator.select(id);
        }
        cx.notify();
    });
    cx.run_until_idle();
    (handle.into(), app)
}

fn run_sidebar_case(case: SbCase, report: &gpui::BenchReport) -> (Vec<Vec<SbStamp>>, usize) {
    let log: SbLog = Rc::new(RefCell::new(Vec::new()));
    let rows_total = Rc::new(std::cell::Cell::new(0usize));
    let mut criterion = criterion::Criterion::default()
        .with_measurement(gpui::BenchMeasurement::new(criterion::measurement::WallTime))
        .sample_size(10)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_secs(2))
        .without_plots();
    let name: &'static str = case.label;
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
        let rows_total = rows_total.clone();
        let micro_done = std::cell::Cell::new(false);
        cx.bench_renderer_session(
            Duration::from_secs(120),
            |cx| {
                let (handle, app) = window.get_or_insert_with(|| open_sidebar_window(cx, case)).clone();
                let (max_offset, rows) = app.update(cx, |this, _| {
                    let rows = this.navigator.visible_row_count();
                    let base = this.sidebar.scroll.0.borrow().base_handle.clone();
                    base.set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                    if case.action == SbAction::GridScroll {
                        let base = this.av().grid_scroll.0.borrow().base_handle.clone();
                        base.set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                    }
                    // The list's viewport is what is left of the 700px window under the toolbars (about 460px).
                    let max = (rows as f32 * theme::SIDEBAR_ROW_HEIGHT - 460.0).max(theme::SIDEBAR_ROW_HEIGHT);
                    (max, rows)
                });
                rows_total.set(rows);
                if !micro_done.replace(true) && case.selected {
                    sb_micro(&app, cx);
                }
                let stopped = Arc::new(AtomicBool::new(false));
                (
                    SbSession {
                        app,
                        case,
                        max_offset,
                        started: None,
                        stopped: stopped.clone(),
                        stamps: Vec::with_capacity(SB_FRAMES as usize + 1),
                        log: log.clone(),
                    },
                    handle,
                    stopped,
                )
            },
            |session, frame, window, cx| {
                session.stamps.push(sb_now_stamp());
                let started = *session.started.get_or_insert_with(Instant::now);
                let done = match session.case.action {
                    SbAction::Typing => frame > SB_TYPED.len() as u64,
                    SbAction::Hover => frame > SB_HOVER_ROWS,
                    SbAction::HoverJitter => frame > 100,
                    SbAction::Pulse | SbAction::Quiet => started.elapsed() >= Duration::from_secs(SB_PULSE_SECONDS),
                    _ => frame >= SB_FRAMES,
                };
                if done {
                    session.stopped.store(true, Ordering::Release);
                    return;
                }
                let max_offset = session.max_offset;
                let action = session.case.action;
                let app = session.app.clone();
                match action {
                    SbAction::Scroll => {
                        const SWEEP: u64 = 60;
                        let phase = (frame % (2 * SWEEP)) as f32 / SWEEP as f32;
                        let t = if phase <= 1.0 { phase } else { 2.0 - phase };
                        app.update(cx, |this, cx| {
                            let base = this.sidebar.scroll.0.borrow().base_handle.clone();
                            base.set_offset(gpui::point(base.offset().x, gpui::px(-(t * max_offset))));
                            cx.notify();
                        });
                    }
                    SbAction::GridScroll => {
                        const SWEEP: u64 = 60;
                        let phase = (frame % (2 * SWEEP)) as f32 / SWEEP as f32;
                        let t = if phase <= 1.0 { phase } else { 2.0 - phase };
                        let max = (60.0 * theme::ROW_HEIGHT - 400.0).max(theme::ROW_HEIGHT);
                        app.update(cx, |this, cx| {
                            let base = this.av().grid_scroll.0.borrow().base_handle.clone();
                            base.set_offset(gpui::point(base.offset().x, gpui::px(-(t * max))));
                            // As a wheel scroll over the grid does: the tab's own view, not the root.
                            let tab = this.active_id();
                            this.notify_tab(tab, cx);
                        });
                    }
                    SbAction::IdleNotify => app.update(cx, |_, cx| cx.notify()),
                    SbAction::Typing => {
                        if frame == 0 {
                            // Focus the (empty) editor; this frame is not counted (see `WARMUP`).
                            let editor = app.read_with(cx, |this, _| {
                                let tab = this.active_id();
                                this.query_editors.by_tab[&tab].clone()
                            });
                            editor.update(cx, |state, cx| state.set_value("", window, cx));
                            let handle = editor.read(cx).focus_handle(cx);
                            window.focus(&handle, cx);
                        } else {
                            let ch = SB_TYPED.chars().nth(frame as usize - 1).unwrap().to_string();
                            let keystroke = gpui::Keystroke {
                                modifiers: Default::default(),
                                key: ch.clone(),
                                key_char: Some(ch),
                            };
                            window.dispatch_keystroke(keystroke, cx);
                        }
                    }
                    SbAction::HoverJitter => {
                        let bounds = app.read_with(cx, |this, _| this.sidebar.scroll.0.borrow().base_handle.bounds());
                        let y = bounds.origin.y + gpui::px(theme::SIDEBAR_ROW_HEIGHT * 3.5);
                        let x = bounds.origin.x + gpui::px(60.0 + (frame % 100) as f32);
                        window.simulate_mouse_move(gpui::point(x, y), cx);
                    }
                    SbAction::Hover => {
                        // Frame 0 parks the mouse on the first row; each later frame moves one row down.
                        let bounds = app.read_with(cx, |this, _| this.sidebar.scroll.0.borrow().base_handle.bounds());
                        let y = bounds.origin.y
                            + gpui::px(theme::SIDEBAR_ROW_HEIGHT * (frame as f32 + 0.5));
                        let x = bounds.origin.x + gpui::px(120.0);
                        window.simulate_mouse_move(gpui::point(x, y), cx);
                    }
                    SbAction::Pulse => {
                        if frame == 0 {
                            app.update(cx, |this, cx| {
                                this.sessions.begin_connect(crate::ui_fixture::POSTGRES_ID);
                                cx.notify();
                            });
                        }
                        std::thread::sleep(Duration::from_millis(8));
                    }
                    SbAction::Quiet => std::thread::sleep(Duration::from_millis(8)),
                }
            },
        );
        cx.teardown();
    });
    let sessions = log.borrow().clone();
    (sessions, rows_total.get())
}

struct SbSummary {
    frames: usize,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
    allocs: f64,
    kib: f64,
    renders: f64,
    passes: f64,
    rows: f64,
    sidebar: f64,
    per_second: f64,
    allocs_per_second: f64,
}

/// Warm sessions (every one after the first); the first frame of the sessions that set up state
/// (typing: focus; hover: park; pulse: begin connect) is left out.
fn sb_summarise(sessions: &[Vec<SbStamp>], skip: usize) -> SbSummary {
    let warm = if sessions.len() > 1 { &sessions[1..] } else { sessions };
    let (mut times, mut allocs) = (Vec::new(), Vec::new());
    let (mut bytes, mut renders, mut passes, mut rows, mut sidebar) = (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut seconds = 0.0;
    for stamps in warm {
        for pair in stamps.windows(2).skip(skip) {
            let dt = pair[1].at.duration_since(pair[0].at).as_secs_f64();
            seconds += dt;
            times.push(dt * 1000.0);
            allocs.push(pair[1].allocations - pair[0].allocations);
            bytes += pair[1].bytes - pair[0].bytes;
            renders += pair[1].renders - pair[0].renders;
            passes += pair[1].list_passes - pair[0].list_passes;
            rows += pair[1].rows_built - pair[0].rows_built;
            sidebar += pair[1].sidebar_renders - pair[0].sidebar_renders;
        }
    }
    assert!(!times.is_empty(), "the benchmark recorded no frames");
    times.sort_by(f64::total_cmp);
    let n = times.len() as f64;
    let total_allocs = allocs.iter().sum::<u64>() as f64;
    SbSummary {
        frames: times.len(),
        p50_ms: percentile(&times, 0.50),
        p95_ms: percentile(&times, 0.95),
        max_ms: *times.last().unwrap(),
        allocs: total_allocs / n,
        kib: bytes as f64 / n / 1024.0,
        renders: renders as f64 / n,
        passes: passes as f64 / n,
        rows: rows as f64 / n,
        sidebar: sidebar as f64 / n,
        per_second: renders as f64 / seconds,
        allocs_per_second: total_allocs / seconds,
    }
}

#[test]
#[ignore = "on-demand render benchmark; run with --features bench --release --ignored --nocapture"]
fn sidebar_bench_context() {
    let big3 = SbTree::Big { schemas: 3, tables: 1000 };
    let big10 = SbTree::Big { schemas: 1, tables: 10_000 };
    let cases = [
        SbCase { label: "scroll-tiny", tree: SbTree::Tiny, action: SbAction::Scroll, hidden: false, selected: false },
        SbCase { label: "scroll-3x1000", tree: big3, action: SbAction::Scroll, hidden: false, selected: false },
        SbCase { label: "scroll-1x10000", tree: big10, action: SbAction::Scroll, hidden: false, selected: false },
        SbCase { label: "idle-tiny", tree: SbTree::Tiny, action: SbAction::IdleNotify, hidden: false, selected: false },
        SbCase { label: "idle-3x1000", tree: big3, action: SbAction::IdleNotify, hidden: false, selected: false },
        SbCase { label: "idle-1x10000", tree: big10, action: SbAction::IdleNotify, hidden: false, selected: false },
        SbCase { label: "idle-3x1000-hidden", tree: big3, action: SbAction::IdleNotify, hidden: true, selected: false },
        SbCase { label: "idle-tiny-selected", tree: SbTree::Tiny, action: SbAction::IdleNotify, hidden: false, selected: true },
        SbCase { label: "idle-3x1000-selected", tree: big3, action: SbAction::IdleNotify, hidden: false, selected: true },
        SbCase { label: "idle-1x10000-selected", tree: big10, action: SbAction::IdleNotify, hidden: false, selected: true },
        SbCase { label: "scroll-3x1000-selected", tree: big3, action: SbAction::Scroll, hidden: false, selected: true },
        SbCase { label: "jitter-3x1000", tree: big3, action: SbAction::HoverJitter, hidden: false, selected: false },
        SbCase { label: "typing-tiny", tree: SbTree::Tiny, action: SbAction::Typing, hidden: false, selected: false },
        SbCase { label: "typing-3x1000", tree: big3, action: SbAction::Typing, hidden: false, selected: false },
        SbCase { label: "typing-3x1000-hidden", tree: big3, action: SbAction::Typing, hidden: true, selected: false },
        SbCase { label: "hover-tiny", tree: SbTree::Tiny, action: SbAction::Hover, hidden: false, selected: false },
        SbCase { label: "hover-3x1000", tree: big3, action: SbAction::Hover, hidden: false, selected: false },
        SbCase { label: "pulse-tiny", tree: SbTree::Tiny, action: SbAction::Pulse, hidden: false, selected: false },
        SbCase { label: "pulse-3x1000", tree: big3, action: SbAction::Pulse, hidden: false, selected: false },
        SbCase { label: "quiet-tiny", tree: SbTree::Tiny, action: SbAction::Quiet, hidden: false, selected: false },
        SbCase { label: "gridscroll-tiny", tree: SbTree::Tiny, action: SbAction::GridScroll, hidden: false, selected: false },
        SbCase { label: "gridscroll-3x1000", tree: big3, action: SbAction::GridScroll, hidden: false, selected: false },
        SbCase { label: "gridscroll-3x1000-hidden", tree: big3, action: SbAction::GridScroll, hidden: true, selected: false },
    ];
    let only = std::env::var("SIDEBAR_BENCH_ONLY").ok();
    let mut rows = Vec::new();
    for case in cases {
        if only.as_deref().is_some_and(|only| !only.split(',').any(|part| case.label.contains(part))) {
            continue;
        }
        let report = gpui::BenchReport::with_frame_budget_nanos(16_600_000);
        let (sessions, tree_rows) = run_sidebar_case(case, &report);
        let skip = usize::from(matches!(case.action, SbAction::Typing | SbAction::Hover | SbAction::Pulse));
        rows.push((case.label, tree_rows, sb_summarise(&sessions, skip), sessions.len()));
    }
    eprintln!("sidebar frame times, warm sessions only ({SB_FRAMES} frames per scroll/idle session)");
    eprintln!(
        "{:>25} {:>7} {:>7} {:>8} {:>8} {:>8} {:>10} {:>9} {:>9} {:>8} {:>8} {:>7} {:>9} {:>10}",
        "case", "rows", "frames", "p50 ms", "p95 ms", "max ms", "allocs/fr", "KiB/fr", "renders/fr", "passes/fr", "rows/fr", "sb/fr", "renders/s", "allocs/s"
    );
    for (label, tree_rows, s, sessions) in &rows {
        eprintln!(
            "{:>25} {:>7} {:>7} {:>8.3} {:>8.3} {:>8.3} {:>10.0} {:>9.1} {:>9.2} {:>9.2} {:>8.1} {:>7.2} {:>9.1} {:>10.0}   ({sessions} sessions)",
            label, tree_rows, s.frames, s.p50_ms, s.p95_ms, s.max_ms, s.allocs, s.kib, s.renders, s.passes, s.rows, s.sidebar, s.per_second, s.allocs_per_second,
        );
    }
}

/// Direct timings of single calls the navigator render makes (mean of `N` calls), on the seeded tree.
fn sb_micro(app: &gpui::Entity<AppRoot>, cx: &mut gpui::BenchAppContext<'_, '_>) {
    const N: u32 = 200;
    app.update(cx, |this, _| {
        let rows = this.navigator.visible_row_count();
        let time = |f: &mut dyn FnMut()| {
            f();
            let start = Instant::now();
            for _ in 0..N {
                f();
            }
            start.elapsed().as_secs_f64() * 1e6 / f64::from(N)
        };
        let selection = time(&mut || {
            std::hint::black_box(this.navigator.selection_in_visible_order());
        });
        let cloned = time(&mut || {
            for index in 0..22 {
                std::hint::black_box(this.navigator.visible_row_at(index + rows / 2));
            }
        });
        let object = this.navigator.visible_row_at(rows / 2).expect("a row");
        let stats = time(&mut || {
            std::hint::black_box((this.table_stats_supported(&object.id), this.table_stats_of(&object.id)));
        });
        let allocations = crate::grid_bench::allocation_count();
        this.table_stats_of(&object.id);
        this.table_stats_supported(&object.id);
        let stats_allocs = crate::grid_bench::allocation_count() - allocations;
        eprintln!(
            "MICRO rows={rows} selection_in_visible_order={selection:.1}us visible_row_at x22 (clone)={cloned:.2}us \
             table_stats_supported+table_stats_of(one Object row)={stats:.3}us ({stats_allocs} allocs) row={:?}",
            object.id
        );
    });
}
