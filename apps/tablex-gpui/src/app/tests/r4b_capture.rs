// Offscreen captures of the data transfer wizard: the tables to pick, the target (tables and files), the mapping of a table
// that exists (with a flagged column) and of one to create, the run settings, a transfer in progress, its summary with a table
// that failed and rows that were skipped, a file export, and a query as the source (headless Metal renderer, real text system).
// Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_transfer_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_transfer_screens() {
    use crate::app::data_import::run::{Outcome, Report, RowError};
    use crate::app::data_transfer::job::{JobResult, TransferOutcome, TransferProgress, TransferReport};
    use crate::app::data_transfer::{Namespace, TransferInit};
    use std::sync::atomic::Ordering;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "connection-tree", "--ui-fixture", "sample"]).unwrap().unwrap();
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
            for _ in 0..40 {
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
        let postgres = crate::ui_fixture::POSTGRES_ID;
        let namespace = Namespace { connection: postgres.into(), database: "warehouse".into(), schema: "public".into() };
        let open = |cx: &mut gpui::HeadlessAppContext, init: TransferInit| {
            cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_data_transfer(init, window, cx))).unwrap();
        };
        let wizard = |cx: &mut gpui::HeadlessAppContext| cx.update(|app_cx| app.read(app_cx).transfers.iter().max_by_key(|(tab, _)| **tab).map(|(_, view)| view.clone()).expect("the wizard"));
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);

        // 1. Tables to a table: pick, target, mapping of a table that exists and of one to create, run settings.
        open(&mut cx, TransferInit { source: namespace.clone(), tables: vec!["orders".into()], all: false, to_file: false, query: None });
        settle(&mut cx);
        let view = wizard(&mut cx);
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.select_all_for_test(true, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-source");
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.pick_first_for_test(cx))).unwrap();
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-target");
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        // orders exists beside itself: another name, which exists too (a table with a different set of columns), so the mapping has gaps.
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.rename_job_for_test(0, "customers", cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-mapping-append");
        cx.update_window(handle, |_, _, app_cx| {
            view.update(app_cx, |view, cx| {
                view.set_action_for_test(0, true, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-mapping-create");
        cx.update_window(handle, |_, window, app_cx| view.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-run-settings");

        // 2. A transfer in flight, and its summary.
        let labels: Vec<gpui::SharedString> = ["orders", "customers", "invoices", "payments", "shipments"].into_iter().map(Into::into).collect();
        let progress = Arc::new(TransferProgress::new(labels.len()));
        progress.current.store(2, Ordering::Relaxed);
        let line = |read: u64, written: u64, skipped: u64, outcome: Outcome, errors: Vec<RowError>| JobResult {
            report: Report { outcome, inserted: written, rolled_back: 0, skipped, rows_read: read, errors_total: skipped, errors, uncertain: 0, bytes: read * 120, elapsed: std::time::Duration::from_secs(3) },
            file: None,
            note: None,
            snapshot: crate::app::data_transfer::reader::SnapshotState::Held,
        };
        for (index, result) in [line(1_204_331, 1_204_331, 0, Outcome::Completed, Vec::new()), line(88_120, 88_120, 0, Outcome::Completed, Vec::new())].into_iter().enumerate() {
            progress.finished.lock().unwrap()[index] = Some(result);
        }
        progress.jobs[0].rows_read.store(1_204_331, Ordering::Relaxed);
        progress.jobs[0].inserted.store(1_204_331, Ordering::Relaxed);
        progress.jobs[0].bytes.store(144_519_720, Ordering::Relaxed);
        progress.jobs[1].rows_read.store(88_120, Ordering::Relaxed);
        progress.jobs[1].inserted.store(88_120, Ordering::Relaxed);
        progress.jobs[1].bytes.store(10_574_400, Ordering::Relaxed);
        progress.jobs[2].rows_read.store(412_000, Ordering::Relaxed);
        progress.jobs[2].inserted.store(410_000, Ordering::Relaxed);
        progress.jobs[2].skipped.store(3, Ordering::Relaxed);
        progress.jobs[2].bytes.store(49_440_000, Ordering::Relaxed);
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.seed_progress(progress.clone(), std::time::Duration::from_secs(21), labels.clone(), cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-progress");
        let report = TransferReport {
            outcome: TransferOutcome::Completed,
            jobs: vec![
                Some(line(1_204_331, 1_204_331, 0, Outcome::Completed, Vec::new())),
                Some(line(88_120, 88_120, 0, Outcome::Completed, Vec::new())),
                Some(line(
                    412_003,
                    412_000,
                    3,
                    Outcome::Completed,
                    vec![
                        RowError { row: 7, line: 0, message: "\"total\" is not a number: \"n/a\"".into() },
                        RowError { row: 5_021, line: 0, message: "duplicate key value violates unique constraint \"invoices_pkey\"".into() },
                        RowError { row: 77_310, line: 0, message: "null value in column \"customer_id\" violates not-null constraint".into() },
                    ],
                )),
                Some(line(0, 0, 0, Outcome::Failed("relation \"public.payments\" does not exist".into()), Vec::new())),
                None,
            ],
            elapsed: std::time::Duration::from_secs_f64(41.7),
        };
        cx.update_window(handle, |_, _, app_cx| view.update(app_cx, |view, cx| view.seed_report(labels.clone(), report, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-summary");

        // 3. Files: the target, the files of a schema.
        open(&mut cx, TransferInit { source: namespace.clone(), tables: Vec::new(), all: true, to_file: true, query: None });
        settle(&mut cx);
        let files = wizard(&mut cx);
        cx.update_window(handle, |_, window, app_cx| files.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-files");
        cx.update_window(handle, |_, window, app_cx| files.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "transfer-files-mapping");

        // 4. A query as the source.
        open(
            &mut cx,
            TransferInit {
                source: namespace,
                tables: Vec::new(),
                all: false,
                to_file: true,
                query: Some("SELECT o.id, c.name, o.total, o.created\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.created >= DATE '2024-01-01'\nORDER BY o.id".into()),
            },
        );
        settle(&mut cx);
        shoot(&mut cx, "transfer-query");
    }
}
