// Render benchmark of the table Properties (Columns sub-tab) in view mode and in the structure
// edit mode, hosted like `grid_bench_context.rs` on GPUI's `BenchAppContext`: the real
// `Root` + `AppRoot` window, the headless Metal renderer and the real text system, so layout,
// paint, text shaping and GPU submission are all included. Included next to that file in the
// `bench_context` module (it shares its load-time text system and frame budget).
//
// Run on demand (dev-only `bench` feature; never part of default/test builds):
//
//   cargo test --release -p tablex-gpui --features bench properties_render_bench_context -- --ignored --nocapture --test-threads=1
//
// `TABLEX_PROPS_BENCH_FILTER=<text>` runs only the cases whose label contains <text>.
//
// A case is a table of C columns (`col0`..) in a tab of the Postgres (or SQLite) fixture, shown
// in Properties > Columns, and what happens in it:
//   idle    every frame notifies AppRoot once and nothing else changes: the cost of one re-render
//   scroll  every frame moves the list along a triangle wave over its whole extent
//   commit  an inline cell edit: one frame opens the Name cell of a column, the next types a
//           value and commits it (the draft changes, everything derived from it is rebuilt)
//   quiet   one notify, then 1 s of pumping the app: how many frames and rebuilds follow
// Edit-mode cases start with a draft of K mixed changes (renames, alters, adds, drops). Per frame
// the table reports time, allocations, bytes, and how many AppRoot renders, `rebuild()` calls,
// `plan_against` calls and `load_structure_catalog` calls the frame made (a render count above 1
// per frame, or any in a quiet case, is a notify loop). Tags F1..F13 name the review findings a
// case measures. No perf asserts: the numbers are informational.

use crate::app::results::properties_edit::{Exit, Field};
use crate::app::structure_edit::bench as fixture;
use crate::navigator::{NavigatorNodeId, NavigatorRow, ObjectGroup};
use crate::perf_bench::{self, mark, record};
use crate::perf_counters as counters;

const PROP_FRAMES: u64 = 100;
const QUIET_WINDOW: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropDialect {
    Postgres,
    Sqlite,
}

/// A second tab shown next to the first one in a split pane.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PropOther {
    None,
    /// Another schema's table, also in Properties edit mode.
    EditOtherSchema,
    /// Another schema's table, as a diagram.
    DiagramOtherSchema,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropDrive {
    Idle,
    Scroll,
    Commit,
    Quiet,
}

impl PropDrive {
    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Scroll => "scroll",
            Self::Commit => "commit",
            Self::Quiet => "quiet",
        }
    }
}

#[derive(Clone)]
struct PropCase {
    label: String,
    columns: usize,
    /// `Some(K)`: edit mode with a draft of K changes.
    edit: Option<usize>,
    indexes: usize,
    foreign_keys: usize,
    dialect: PropDialect,
    /// The review dialog is open.
    dialog: bool,
    other: PropOther,
    /// The catalog of the table names stays "loading" (F13).
    catalog_missing: bool,
    drive: PropDrive,
}

impl PropCase {
    fn new(tag: &'static str, columns: usize, edit: Option<usize>, drive: PropDrive) -> Self {
        let mode = match edit {
            Some(changes) => format!("edit K={changes}"),
            None => "view".to_owned(),
        };
        Self {
            label: format!("{tag} C={columns} {mode} {}", drive.name()).trim_start().to_owned(),
            columns,
            edit,
            indexes: 0,
            foreign_keys: 0,
            dialect: PropDialect::Postgres,
            dialog: false,
            other: PropOther::None,
            catalog_missing: false,
            drive,
        }
    }

    fn labelled(mut self, suffix: &str) -> Self {
        self.label = format!("{} {suffix}", self.label);
        self
    }
}

/// One stamp per input callback (consecutive stamps bracket one frame).
#[derive(Clone, Copy)]
struct PropStamp {
    at: Instant,
    allocations: u64,
    bytes: u64,
    renders: u64,
    rebuilds: u64,
    plans: u64,
    loads: u64,
}

fn prop_stamp() -> PropStamp {
    PropStamp {
        at: Instant::now(),
        allocations: crate::grid_bench::allocation_count(),
        bytes: crate::grid_bench::allocation_bytes(),
        renders: frame_count(),
        rebuilds: counters::read(&counters::REBUILD),
        plans: counters::read(&counters::PLAN_AGAINST),
        loads: counters::read(&counters::LOAD_STRUCTURE_CATALOG),
    }
}

/// One inline cell edit: what committing it cost (the call itself, not the frame after).
#[derive(Clone, Copy)]
struct CommitSample {
    /// The input callback it ran in (its stamp and the next one bracket the frame after).
    frame: usize,
    cost: perf_bench::Cost,
    applied: bool,
    rebuilds: u64,
    plans: u64,
}

type SessionData = (Vec<PropStamp>, Vec<CommitSample>);
type PropLog = Rc<RefCell<Vec<SessionData>>>;

/// The window of a case, set up once per Criterion routine call.
#[derive(Clone)]
struct PropWindow {
    handle: gpui::AnyWindowHandle,
    app: gpui::Entity<AppRoot>,
    tab_id: u64,
    /// The column the commit case renames back and forth (no pending change touches it).
    commit_column: usize,
}

struct PropSession {
    window: PropWindow,
    drive: PropDrive,
    max_offset: f32,
    scroll: gpui::ScrollHandle,
    stopped: Arc<AtomicBool>,
    stamps: Vec<PropStamp>,
    commits: Vec<CommitSample>,
    log: PropLog,
}

impl Drop for PropSession {
    fn drop(&mut self) {
        let data = (std::mem::take(&mut self.stamps), std::mem::take(&mut self.commits));
        self.log.borrow_mut().push(data);
    }
}

fn nav_row(cx: &mut gpui::BenchAppContext<'_, '_>, app: &gpui::Entity<AppRoot>, id: &NavigatorNodeId) -> NavigatorRow {
    app.update(cx, |this, _| this.navigator.visible_rows().into_iter().find(|row| row.id == *id))
        .unwrap_or_else(|| panic!("navigator row {id:?} is visible"))
}

fn nav_disclose(cx: &mut gpui::BenchAppContext<'_, '_>, app: &gpui::Entity<AppRoot>, id: &NavigatorNodeId) {
    let row = nav_row(cx, app, id);
    if !row.expanded {
        app.update(cx, |this, cx| this.handle_navigator_disclosure(&row, cx));
        cx.settle();
    }
}

/// Opens the `orders` table of `schema` in a tab and returns its id.
fn open_orders_tab(cx: &mut gpui::BenchAppContext<'_, '_>, app: &gpui::Entity<AppRoot>, dialect: PropDialect, schema: &str) -> u64 {
    let (connection, database, schema) = match dialect {
        PropDialect::Postgres => (crate::ui_fixture::POSTGRES_ID, "warehouse", schema),
        PropDialect::Sqlite => (crate::ui_fixture::SQLITE_ID, "main", "main"),
    };
    let node = |kind: &str| match kind {
        "connection" => NavigatorNodeId::Connection { connection: connection.into() },
        "database" => NavigatorNodeId::Database { connection: connection.into(), database: database.into() },
        _ => NavigatorNodeId::Schema { connection: connection.into(), database: database.into(), schema: schema.into() },
    };
    for kind in ["connection", "database", "schema"] {
        nav_disclose(cx, app, &node(kind));
    }
    nav_disclose(cx, app, &crate::navigator::object_group_id(connection, database, schema, ObjectGroup::Tables));
    let orders = NavigatorNodeId::Object { connection: connection.into(), database: database.into(), schema: schema.into(), object: "orders".into() };
    let row = nav_row(cx, app, &orders);
    app.update(cx, |this, cx| this.handle_navigator_open(&row, cx));
    cx.settle();
    app.read_with(cx, |this, _| this.workspace.active_tab().expect("the table opened a tab").id)
}

/// Makes `properties` what the tab's Properties holds, with the result grid of the same columns
/// (the view mode lists the grid's columns).
fn describe_tab(cx: &mut gpui::BenchAppContext<'_, '_>, app: &gpui::Entity<AppRoot>, tab_id: u64, properties: crate::catalog::TableProperties) {
    let columns = properties.columns.len();
    // A new result makes the Properties fetch the table again (the fixture answers with its own
    // columns): let that answer land first, then put the described table over it.
    app.update(cx, |this, cx| {
        this.set_active_grid(crate::grid::GridState::new(GridContentSource::Arrow(crate::grid_arrow::ArrowGridSource::new(vec![Arc::new(
            wide_grid_batch(20, columns),
        )]))));
        this.show_columns_presentation(tab_id, cx);
        cx.notify();
    });
    cx.settle();
    app.update(cx, |this, cx| {
        let props = &mut this.workspace.runtime_mut(tab_id).expect("a runtime").columns_panel.props;
        props.data = Some(Arc::new(properties));
        props.revision += 1;
        props.tables.clear();
        cx.notify();
    });
    cx.settle();
    let described = app.read_with(cx, |this, _| this.workspace.runtime(tab_id).and_then(|runtime| runtime.columns_panel.props.data.as_ref().map(|data| data.columns.len())));
    assert_eq!(described, Some(columns), "the Properties hold the described table");
}

fn with_window(cx: &mut gpui::BenchAppContext<'_, '_>, handle: gpui::AnyWindowHandle, run: impl FnOnce(&mut Window, &mut gpui::App)) {
    cx.update(|app| app.update_window(handle, |_, window, cx| run(window, cx)).expect("the benchmark window is open"));
}

fn start_edit(cx: &mut gpui::BenchAppContext<'_, '_>, handle: gpui::AnyWindowHandle, app: &gpui::Entity<AppRoot>, tab_id: u64) {
    with_window(cx, handle, |window, cx| app.update(cx, |this, cx| this.toggle_properties_edit(tab_id, window, cx)));
    cx.settle();
    assert!(app.read_with(cx, |this, _| this.properties_edits.contains_key(&tab_id)), "the edit mode started for tab {tab_id}");
}

fn open_properties_window(cx: &mut gpui::BenchAppContext<'_, '_>, case: &PropCase) -> PropWindow {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let scenario = UiScenario {
        kind: crate::ui_scenario::ScenarioKind::ConnectionTree,
        fixture: FixtureKind::Sample,
        delay: Duration::ZERO,
        timeout_delay: Duration::ZERO,
    };
    let handle = cx.update(|app| {
        let bounds = gpui::Bounds { origin: gpui::point(gpui::px(0.0), gpui::px(0.0)), size: gpui::size(gpui::px(1400.0), gpui::px(850.0)) };
        let handle = app
            .open_window(gpui::WindowOptions { window_bounds: Some(gpui::WindowBounds::Windowed(bounds)), ..Default::default() }, |window, cx| {
                let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("the benchmark window should open");
        // As `BenchAppContext::add_empty_window` does: an inactive window is frame-throttled.
        app.update_window(handle.into(), |_, window, _| window.activate_window()).expect("the benchmark window should activate");
        handle
    });
    let root = handle.entity(&*cx).expect("window root should be alive");
    let app = cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().expect("root should contain AppRoot"));
    cx.settle();

    let handle: gpui::AnyWindowHandle = handle.into();
    let tab_id = open_orders_tab(cx, &app, case.dialect, "public");
    describe_tab(cx, &app, tab_id, fixture::described_with(case.columns, case.indexes, case.foreign_keys));
    let changes = case.edit.unwrap_or(0);
    let properties = fixture::described_with(case.columns, case.indexes, case.foreign_keys);
    if case.edit.is_some() {
        start_edit(cx, handle, &app, tab_id);
        if changes > 0 {
            let edit = app.read_with(cx, |this, _| this.properties_edits.get(&tab_id).cloned()).expect("edit mode");
            let list = fixture::mixed_changes(&properties, changes);
            assert!(edit.update(cx, |edit, cx| edit.push_all_for_test(list, cx)), "the draft took the changes");
            cx.settle();
            assert_eq!(edit.read_with(cx, |edit, _| edit.pending_count()), changes, "the draft holds exactly K changes");
        }
    }
    if case.dialog {
        with_window(cx, handle, |window, cx| app.update(cx, |this, cx| this.open_properties_changes(tab_id, window, cx)));
        cx.settle();
        // Measure the open dialog, not its entrance: the dialog animates in on wall-clock time and
        // asks for a frame (a `Root` notify per layer) until it ends, which any frame that lays the
        // dialog out again would otherwise count. Let it run out, then draw its final state.
        std::thread::sleep(*gpui_component::dialog::ANIMATION_DURATION + std::time::Duration::from_millis(50));
        with_window(cx, handle, |window, _| window.refresh());
        cx.settle();
    }
    match case.other {
        PropOther::None => {}
        PropOther::EditOtherSchema | PropOther::DiagramOtherSchema => {
            let second = open_orders_tab(cx, &app, PropDialect::Postgres, "analytics");
            assert_ne!(second, tab_id, "the second table opened its own tab");
            app.update(cx, |this, cx| {
                assert!(this.workspace.split_tab(second, crate::panes::Edge::Right), "the second tab split off");
                cx.notify();
            });
            cx.settle();
            if case.other == PropOther::EditOtherSchema {
                describe_tab(cx, &app, second, fixture::described(case.columns));
                start_edit(cx, handle, &app, second);
            } else {
                app.update(cx, |this, cx| this.show_diagram_presentation(second, cx));
                cx.settle();
            }
        }
    }
    if case.catalog_missing {
        // The tables group of the tab's namespace is "loading" for good: nothing answers it.
        app.update(cx, |this, _| {
            let group = crate::navigator::object_group_id(crate::ui_fixture::POSTGRES_ID, "warehouse", "public", ObjectGroup::Tables);
            this.navigator.reset_node(&group);
            assert!(this.navigator.begin_background_load(&group).is_some(), "the group started loading");
        });
        cx.settle();
    }
    PropWindow { handle, app, tab_id, commit_column: fixture::untouched_column(case.columns, changes) }
}

fn prop_criterion() -> criterion::Criterion<gpui::BenchMeasurement> {
    criterion::Criterion::default()
        .with_measurement(gpui::BenchMeasurement::new(criterion::measurement::WallTime))
        .sample_size(10)
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1))
        .without_plots()
}

fn new_bench_context<'a, 'm>(name: &'static str, bencher: &'a mut criterion::Bencher<'m, gpui::BenchMeasurement>, report: &gpui::BenchReport) -> gpui::BenchAppContext<'a, 'm> {
    gpui::BenchAppContext::new_with_platform_and_report(
        gpui::bench_platform(
            Some(Box::new(gpui_platform::current_headless_renderer)),
            MAIN_THREAD_TEXT_SYSTEM.get().expect("text system is created by the load-time constructor").clone(),
        ),
        Some(name),
        bencher,
        report.clone(),
    )
}

/// `N` frames of `case.drive` per session, as many sessions as Criterion runs.
fn run_frames(name: &'static str, case: &PropCase, report: &gpui::BenchReport) -> Vec<SessionData> {
    let log: PropLog = Rc::new(RefCell::new(Vec::new()));
    prop_criterion().bench_function(name, |bencher| {
        let mut cx = new_bench_context(name, bencher, report);
        let mut window: Option<PropWindow> = None;
        let log = log.clone();
        let case = case.clone();
        cx.bench_renderer_session(
            Duration::from_secs(60),
            |cx| {
                let window = window.get_or_insert_with(|| open_properties_window(cx, &case)).clone();
                let scroll = window.app.read_with(cx, |this, _| this.view(window.tab_id).columns_scroll.0.borrow().base_handle.clone());
                scroll.set_offset(gpui::point(gpui::px(0.0), gpui::px(0.0)));
                // The list learns its extent from the frames drawn after the last change.
                let mut settled = 0;
                while f32::from(scroll.max_offset().y.abs()) == 0.0 && settled < 6 {
                    window.app.update(cx, |_, cx| cx.notify());
                    cx.settle();
                    settled += 1;
                }
                let max_offset = f32::from(scroll.max_offset().y.abs());
                if case.drive == PropDrive::Scroll && max_offset <= 0.0 {
                    let edit = window.app.read_with(cx, |this, _| this.properties_edits.get(&window.tab_id).cloned());
                    let state = edit.map(|edit| edit.read_with(cx, |edit, _| format!("rows {} editable {}", edit.rows().len(), edit.editable())));
                    panic!("the list must overflow to be scrolled: bounds {:?} max {:?} offset {:?}, edit {state:?}", scroll.bounds(), scroll.max_offset(), scroll.offset());
                }
                let stopped = Arc::new(AtomicBool::new(false));
                let handle = window.handle;
                (
                    PropSession {
                        window,
                        drive: case.drive,
                        max_offset,
                        scroll,
                        stopped: stopped.clone(),
                        stamps: Vec::with_capacity(PROP_FRAMES as usize + 2),
                        commits: Vec::with_capacity(PROP_FRAMES as usize),
                        log: log.clone(),
                    },
                    handle,
                    stopped,
                )
            },
            |session, frame, window, cx| {
                session.stamps.push(prop_stamp());
                if frame >= PROP_FRAMES {
                    session.stopped.store(true, Ordering::Release);
                    return;
                }
                match session.drive {
                    PropDrive::Idle | PropDrive::Quiet => {
                        session.window.app.update(cx, |_, cx| cx.notify());
                    }
                    PropDrive::Scroll => {
                        // Triangle wave over the scrollable extent: new rows enter every frame.
                        const SWEEP: u64 = 60;
                        let phase = (frame % (2 * SWEEP)) as f32 / SWEEP as f32;
                        let offset = (if phase <= 1.0 { phase } else { 2.0 - phase }) * session.max_offset;
                        session.scroll.set_offset(gpui::point(gpui::px(0.0), gpui::px(-offset)));
                        session.window.app.update(cx, |_, cx| cx.notify());
                    }
                    PropDrive::Commit => commit_step(session, frame, window, cx),
                }
            },
        );
        cx.teardown();
    });
    let sessions = log.borrow().clone();
    sessions
}

/// Even frames open the Name cell of the commit column, odd frames type a value and commit it
/// (the column is renamed, then renamed back: the draft returns to where it started).
fn commit_step(session: &mut PropSession, frame: u64, window: &mut Window, cx: &mut gpui::App) {
    let PropWindow { app, tab_id, commit_column, .. } = session.window.clone();
    let edit = app.read(cx).properties_edits.get(&tab_id).cloned().expect("the commit case runs in edit mode");
    if frame % 2 == 0 {
        edit.update(cx, |edit, cx| {
            let opened = edit.begin_edit(commit_column, Field::Name, window, cx);
            assert!(opened, "the Name cell opens: column {commit_column}, rows {}, editable {}, selected {:?}, report {:?}", edit.rows().len(), edit.editable(), edit.selected(), edit.review_report());
        });
        return;
    }
    let original = format!("col{commit_column}");
    let text = if (frame / 2) % 2 == 0 { format!("{original}_x") } else { original };
    let pending = edit.read(cx).pending_count();
    let (rebuilds, plans) = (counters::read(&counters::REBUILD), counters::read(&counters::PLAN_AGAINST));
    let started = mark();
    edit.update(cx, |edit, cx| {
        edit.editor().update(cx, |editor, cx| editor.set_value(text, window, cx));
        edit.commit_edit(Exit::Enter, window, cx);
    });
    let cost = started.since();
    session.commits.push(CommitSample {
        frame: session.stamps.len() - 1,
        cost,
        applied: edit.read(cx).pending_count() != pending,
        rebuilds: counters::read(&counters::REBUILD) - rebuilds,
        plans: counters::read(&counters::PLAN_AGAINST) - plans,
    });
}

#[derive(Default)]
struct FrameSummary {
    frames: usize,
    times_ms: Vec<f64>,
    allocations: f64,
    max_allocations: u64,
    kib: f64,
    renders: f64,
    rebuilds: f64,
    plans: f64,
    loads: f64,
}

/// Summarises frames given as (stamp at the start, stamp at the end, cost of work inside the
/// frame that is reported on its own and left out here).
fn summarise_frames(pairs: impl Iterator<Item = (PropStamp, PropStamp, perf_bench::Cost)>) -> FrameSummary {
    let mut summary = FrameSummary::default();
    let (mut allocations, mut bytes, mut renders, mut rebuilds, mut plans, mut loads) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    for (start, end, inner) in pairs {
        let time = end.at.duration_since(start.at).saturating_sub(inner.elapsed);
        summary.times_ms.push(time.as_secs_f64() * 1000.0);
        let frame_allocations = (end.allocations - start.allocations).saturating_sub(inner.allocations);
        summary.max_allocations = summary.max_allocations.max(frame_allocations);
        allocations += frame_allocations;
        bytes += (end.bytes - start.bytes).saturating_sub(inner.bytes);
        renders += end.renders - start.renders;
        rebuilds += end.rebuilds - start.rebuilds;
        plans += end.plans - start.plans;
        loads += end.loads - start.loads;
    }
    summary.frames = summary.times_ms.len();
    assert!(summary.frames > 0, "the benchmark recorded no frames");
    summary.times_ms.sort_by_key(|time| (time * 1e6) as u64);
    let n = summary.frames as f64;
    summary.allocations = allocations as f64 / n;
    summary.kib = bytes as f64 / n / 1024.0;
    summary.renders = renders as f64 / n;
    summary.rebuilds = rebuilds as f64 / n;
    summary.plans = plans as f64 / n;
    summary.loads = loads as f64 / n;
    summary
}

/// The sessions after the first one (which includes warm-up effects).
fn warm_sessions(sessions: &[SessionData]) -> &[SessionData] {
    if sessions.len() > 1 {
        &sessions[1..]
    } else {
        sessions
    }
}

/// Every frame of the warm sessions.
fn all_frames(sessions: &[SessionData]) -> FrameSummary {
    summarise_frames(warm_sessions(sessions).iter().flat_map(|session| session.0.windows(2).map(|pair| (pair[0], pair[1], perf_bench::Cost::default()))))
}

/// The frame after each commit: the interval from the commit's callback to the next one, minus the commit.
fn frames_after_commit(sessions: &[SessionData]) -> FrameSummary {
    summarise_frames(warm_sessions(sessions).iter().flat_map(|session| {
        session.1.iter().filter_map(|commit| Some((*session.0.get(commit.frame)?, *session.0.get(commit.frame + 1)?, commit.cost)))
    }))
}

fn print_frame_row(label: &str, s: &FrameSummary) {
    eprintln!(
        "{:<44} {:>6} {:>8.3} {:>8.3} {:>8.3} {:>10.0} {:>10} {:>9.1} {:>8.2} {:>8.2} {:>7.2} {:>6.2}",
        label,
        s.frames,
        perf_bench::percentile(&s.times_ms, 0.50),
        perf_bench::percentile(&s.times_ms, 0.95),
        s.times_ms.last().copied().unwrap_or_default(),
        s.allocations,
        s.max_allocations,
        s.kib,
        s.renders,
        s.rebuilds,
        s.plans,
        s.loads,
    );
}

fn record_frame_row(case: &PropCase, s: &FrameSummary) {
    record(
        "render",
        &case.label,
        &[
            ("columns", case.columns as f64),
            ("changes", case.edit.unwrap_or(0) as f64),
            ("frames", s.frames as f64),
            ("p50_ms", perf_bench::percentile(&s.times_ms, 0.50)),
            ("p95_ms", perf_bench::percentile(&s.times_ms, 0.95)),
            ("max_ms", s.times_ms.last().copied().unwrap_or_default()),
            ("allocs_per_frame", s.allocations),
            ("max_allocs", s.max_allocations as f64),
            ("kib_per_frame", s.kib),
            ("renders_per_frame", s.renders),
            ("rebuilds_per_frame", s.rebuilds),
            ("plans_per_frame", s.plans),
            ("catalog_loads_per_frame", s.loads),
        ],
    );
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, count) = values.fold((0.0, 0usize), |(sum, count), value| (sum + value, count + 1));
    if count == 0 {
        0.0
    } else {
        sum / count as f64
    }
}

/// The commit case: the commit call, then the frame after it.
fn report_commits(case: &PropCase, sessions: &[SessionData]) {
    let commits: Vec<&CommitSample> = warm_sessions(sessions).iter().flat_map(|session| session.1.iter()).collect();
    assert!(!commits.is_empty(), "no cell was committed");
    let mut times: Vec<f64> = commits.iter().map(|commit| commit.cost.ms()).collect();
    times.sort_by_key(|time| (time * 1e6) as u64);
    let applied = commits.iter().filter(|commit| commit.applied).count();
    let (p50, p95, max) = (perf_bench::percentile(&times, 0.50), perf_bench::percentile(&times, 0.95), times.last().copied().unwrap_or_default());
    let allocations = mean(commits.iter().map(|commit| commit.cost.allocations as f64));
    let kib = mean(commits.iter().map(|commit| commit.cost.bytes as f64)) / 1024.0;
    let rebuilds = mean(commits.iter().map(|commit| commit.rebuilds as f64));
    let plans = mean(commits.iter().map(|commit| commit.plans as f64));
    eprintln!(
        "{:<44} commits {:>3} (applied {:>3})  commit call: p50 {p50:.3} ms  p95 {p95:.3}  max {max:.3}  {allocations:.0} allocs  {kib:.1} KiB  {rebuilds:.2} rebuilds  {plans:.2} plan_against",
        case.label,
        commits.len(),
        applied,
    );
    record(
        "commit",
        &case.label,
        &[
            ("columns", case.columns as f64),
            ("changes", case.edit.unwrap_or(0) as f64),
            ("commits", commits.len() as f64),
            ("applied", applied as f64),
            ("commit_p50_ms", p50),
            ("commit_p95_ms", p95),
            ("commit_max_ms", max),
            ("commit_allocs", allocations),
            ("commit_kib", kib),
            ("commit_rebuilds", rebuilds),
            ("commit_plan_against", plans),
        ],
    );
    let after = frames_after_commit(sessions);
    let mut next = case.clone();
    next.label = format!("{} / frame after commit", case.label);
    print_frame_row(&next.label, &after);
    record_frame_row(&next, &after);
}

/// Pumps the app for one second after a single notify and counts what it did.
#[derive(Clone, Copy)]
struct QuietResult {
    seconds: f64,
    renders: u64,
    rebuilds: u64,
    plans: u64,
    loads: u64,
    allocations: u64,
    kib: f64,
}

fn run_quiet(name: &'static str, case: &PropCase, report: &gpui::BenchReport) -> Vec<QuietResult> {
    let results: Rc<RefCell<Vec<QuietResult>>> = Rc::new(RefCell::new(Vec::new()));
    prop_criterion().bench_function(name, |bencher| {
        let mut cx = new_bench_context(name, bencher, report);
        let results = results.clone();
        let case = case.clone();
        // Set up before the timed iterations: Criterion makes a new context per routine call.
        let window = open_properties_window(&mut cx, &case);
        cx.settle();
        let app = window.app.clone();
        cx.bench_iter(|cx| {
            app.update(cx, |_, cx| cx.notify());
            let before = prop_stamp();
            let started = Instant::now();
            while started.elapsed() < QUIET_WINDOW {
                cx.run_until_idle();
            }
            let after = prop_stamp();
            results.borrow_mut().push(QuietResult {
                seconds: after.at.duration_since(before.at).as_secs_f64(),
                renders: after.renders - before.renders,
                rebuilds: after.rebuilds - before.rebuilds,
                plans: after.plans - before.plans,
                loads: after.loads - before.loads,
                allocations: after.allocations - before.allocations,
                kib: (after.bytes - before.bytes) as f64 / 1024.0,
            });
        });
        cx.teardown();
    });
    let collected = results.borrow().clone();
    collected
}

fn report_quiet(case: &PropCase, results: &[QuietResult]) {
    assert!(!results.is_empty(), "the quiet case measured nothing");
    let median = |value: &dyn Fn(&QuietResult) -> f64| {
        let mut values: Vec<f64> = results.iter().map(value).collect();
        values.sort_by_key(|value| (value * 1e6) as u64);
        values[values.len() / 2]
    };
    let (seconds, renders, rebuilds, plans, loads, allocations, kib) = (
        median(&|r| r.seconds),
        median(&|r| r.renders as f64),
        median(&|r| r.rebuilds as f64),
        median(&|r| r.plans as f64),
        median(&|r| r.loads as f64),
        median(&|r| r.allocations as f64),
        median(&|r| r.kib),
    );
    let worst_renders = results.iter().map(|r| r.renders).max().unwrap_or_default();
    eprintln!(
        "{:<44} after one notify, {seconds:.2} s of pumping ({} runs): {renders:.0} AppRoot renders (max {worst_renders})  {rebuilds:.0} rebuilds  {plans:.0} plan_against  {loads:.0} catalog loads  {allocations:.0} allocs  {kib:.0} KiB",
        case.label,
        results.len()
    );
    record(
        "quiet",
        &case.label,
        &[
            ("runs", results.len() as f64),
            ("seconds", seconds),
            ("renders", renders),
            ("max_renders", worst_renders as f64),
            ("rebuilds", rebuilds),
            ("plan_against", plans),
            ("catalog_loads", loads),
            ("allocs", allocations),
            ("kib", kib),
        ],
    );
}

fn prop_cases() -> Vec<PropCase> {
    let mut cases = Vec::new();
    for columns in [50, 500, 2000] {
        // F6: allocations per idle frame; F2: per scrolled frame (500 columns, view vs edit).
        let tag = |kind: &'static str| if columns == 500 { kind } else { "" };
        cases.push(PropCase::new(tag("F6"), columns, None, PropDrive::Idle));
        cases.push(PropCase::new(tag("F2"), columns, None, PropDrive::Scroll));
        for changes in [0, 20, 200] {
            let first = changes == 0;
            cases.push(PropCase::new(if first { tag("F6") } else { "" }, columns, Some(changes), PropDrive::Idle));
            cases.push(PropCase::new(if first { tag("F2") } else { "" }, columns, Some(changes), PropDrive::Scroll));
            cases.push(PropCase::new("", columns, Some(changes), PropDrive::Commit));
        }
    }
    // F2: the SQLite dialect formats a block reason per cell of an existing column.
    let mut sqlite = PropCase::new("F2", 500, Some(0), PropDrive::Scroll).labelled("sqlite");
    sqlite.dialect = PropDialect::Sqlite;
    cases.push(sqlite);
    let mut sqlite = PropCase::new("F6", 500, Some(0), PropDrive::Idle).labelled("sqlite");
    sqlite.dialect = PropDialect::Sqlite;
    cases.push(sqlite);
    // F3-F5: 500 columns, 50 changes, 50 indexes, 50 foreign keys.
    for drive in [PropDrive::Idle, PropDrive::Commit] {
        let mut lists = PropCase::new("F3-F5", 500, Some(50), drive).labelled("idx=50 fk=50");
        lists.indexes = 50;
        lists.foreign_keys = 50;
        cases.push(lists);
    }
    // F7: the review dialog is open with 100 changes (the dialog rebuilds its model every frame).
    let mut dialog = PropCase::new("F7", 500, Some(100), PropDrive::Idle).labelled("dialog open");
    dialog.dialog = true;
    cases.push(dialog);
    let mut dialog = PropCase::new("F7", 500, Some(100), PropDrive::Commit).labelled("dialog open");
    dialog.dialog = true;
    cases.push(dialog);
    // F7, few changes: below `VIRTUAL_FROM` the dialog lists its cards in a plain scrolling div.
    let mut few = PropCase::new("F7", 500, Some(5), PropDrive::Idle).labelled("dialog open");
    few.dialog = true;
    cases.push(few);
    let mut few = PropCase::new("F7", 500, Some(5), PropDrive::Commit).labelled("dialog open");
    few.dialog = true;
    cases.push(few);
    // F1: two tabs of different schemas side by side (and the control: one tab).
    cases.push(PropCase::new("F1", 50, Some(0), PropDrive::Quiet).labelled("control: one tab"));
    let mut two = PropCase::new("F1", 50, Some(0), PropDrive::Quiet).labelled("two edit-mode tabs, schemas public + analytics");
    two.other = PropOther::EditOtherSchema;
    cases.push(two);
    let mut diagram = PropCase::new("F1", 50, Some(0), PropDrive::Quiet).labelled("edit-mode tab + diagram of analytics");
    diagram.other = PropOther::DiagramOtherSchema;
    cases.push(diagram);
    // F13: the catalog of table names never arrives.
    let mut missing = PropCase::new("F13", 500, Some(0), PropDrive::Quiet).labelled("catalog missing");
    missing.catalog_missing = true;
    cases.push(missing);
    let mut missing = PropCase::new("F13", 500, Some(0), PropDrive::Idle).labelled("catalog missing");
    missing.catalog_missing = true;
    cases.push(missing);
    cases
}

fn run_case(name: &'static str, case: &PropCase) {
    let report = gpui::BenchReport::with_frame_budget_nanos(FRAME_BUDGET_NANOS);
    if case.drive == PropDrive::Quiet {
        let results = run_quiet(name, case, &report);
        report_quiet(case, &results);
        return;
    }
    let sessions = run_frames(name, case, &report);
    if std::env::var_os("TABLEX_BENCH_FULL_REPORT").is_some() {
        report.print(name);
    }
    if let Some(work) = report.foreground_work() {
        eprintln!(
            "  foreground work: {} items, total {:?}, p50 {:?}, p95 {:?}, max {:?}, frame-budget overruns total {} / max {}",
            work.count, work.total, work.p50, work.p95, work.max, work.frame_budget_overruns_total, work.frame_budget_overruns_max
        );
    }
    let summary = all_frames(&sessions);
    let mut every = case.clone();
    if case.drive == PropDrive::Commit {
        every.label = format!("{} / all frames", case.label);
    }
    print_frame_row(&every.label, &summary);
    record_frame_row(&every, &summary);
    if case.drive == PropDrive::Commit {
        report_commits(case, &sessions);
    }
}

#[test]
#[ignore = "on-demand render benchmark; run with --features bench --release --ignored --nocapture"]
fn properties_render_bench_context() {
    let filter = std::env::var("TABLEX_PROPS_BENCH_FILTER").ok();
    eprintln!(
        "properties render: {PROP_FRAMES} frames per session, warm sessions only; renders/rebuilds/plans/loads are per frame"
    );
    eprintln!(
        "{:<44} {:>6} {:>8} {:>8} {:>8} {:>10} {:>10} {:>9} {:>8} {:>8} {:>7} {:>6}",
        "case", "frames", "p50 ms", "p95 ms", "max ms", "allocs/fr", "max allocs", "KiB/fr", "renders", "rebuilds", "plans", "loads"
    );
    let mut failed = Vec::new();
    for (index, case) in prop_cases().into_iter().enumerate() {
        if filter.as_deref().is_some_and(|filter| !case.label.contains(filter)) {
            continue;
        }
        // Criterion wants a 'static name; the label is unique per case.
        let name: &'static str = Box::leak(format!("properties_{index}_{}", case.label.replace(' ', "_")).into_boxed_str());
        // A case that panics is reported and the others still run.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_case(name, &case)));
        if outcome.is_err() {
            eprintln!("CASE FAILED: {}", case.label);
            failed.push(case.label.clone());
        }
    }
    assert!(failed.is_empty(), "cases that failed: {failed:?}");
}
