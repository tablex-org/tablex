// Offscreen captures of the SQL file wizard: Source with the start of a dump, the Settings, the step before the run, a run in
// progress, and the summary of a run that stopped (headless Metal renderer, real text system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_file_run_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_file_run_screens() {
    use crate::app::file_run::run::{Entry, EntryKind, Outcome, ProgressSnapshot, Report};
    use crate::app::file_run::FileRunInit;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = tempfile::tempdir().unwrap();
    let file = data.path().join("warehouse-dump.sql");
    let mut text = String::from("-- PostgreSQL database dump\n\\connect warehouse\nSET statement_timeout = 0;\nCREATE TABLE public.orders (id bigint PRIMARY KEY, customer text, total numeric(12,2));\n");
    text.push_str("CREATE FUNCTION public.order_total(o bigint) RETURNS numeric LANGUAGE plpgsql AS $$ BEGIN RETURN (SELECT total FROM public.orders WHERE id = o); END; $$;\nCOPY public.orders (id, customer, total) FROM stdin;\n1\tAnn\t10.50\n\\.\n");
    for id in 1..=2000 {
        text.push_str(&format!("INSERT INTO public.orders VALUES ({id}, 'Customer {id}', {}.{:02});\n", id * 3, id % 100));
    }
    std::fs::write(&file, text).unwrap();

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
        let target = crate::app::data_transfer::Namespace { connection: postgres.into(), database: "warehouse".into(), schema: "public".into() };
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);

        cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_file_run(FileRunInit { target: target.clone(), path: Some(file.clone()) }, window, cx))).unwrap();
        settle(&mut cx);
        let wizard = cx.update(|app_cx| app.read(app_cx).file_runs.values().next().cloned().expect("the wizard"));
        shoot(&mut cx, "filerun-source");
        cx.update_window(handle, |_, window, app_cx| wizard.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "filerun-settings");
        cx.update_window(handle, |_, window, app_cx| wizard.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "filerun-ready");
        cx.update_window(handle, |_, _, app_cx| {
            wizard.update(app_cx, |view, cx| {
                view.seed_progress(ProgressSnapshot { number: 1_204_331, ok: 1_204_320, failed: 2, skipped: 9, bytes: 311_000_000 }, 1_073_741_824, std::time::Duration::from_secs(94), cx)
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "filerun-progress");
        let entry = |number: u64, line: u64, excerpt: &str, message: &str, kind| Entry { number, line, excerpt: excerpt.into(), message: message.into(), kind };
        cx.update_window(handle, |_, _, app_cx| {
            wizard.update(app_cx, |view, cx| {
                view.seed_progress(ProgressSnapshot::default(), 0, std::time::Duration::ZERO, cx);
                view.seed_report(
                    Report {
                        outcome: Outcome::Stopped("statement 1204331 was refused: duplicate key value violates unique constraint \"orders_pkey\"".into()),
                        ok: 1_204_320,
                        failed: 1,
                        skipped: 2,
                        passed_over: 0,
                        replayed: 0,
                        not_repeated: 0,
                        result_sets: 3,
                        affected: 1_204_100,
                        rolled_back: 9_999,
                        last: 1_204_331,
                        resume_from: Some(1_200_001),
                        entries: vec![
                            entry(1, 2, "\\connect warehouse", "stays on warehouse", EntryKind::Skipped),
                            entry(7, 5, "COPY public.orders (id, customer, total) FROM stdin (its data lines are left out)", "COPY \u{2026} FROM stdin is not run: its data lines are in the file itself, and the server takes them only from a client. Load that table with Import Data.", EntryKind::Skipped),
                            entry(1_204_331, 1_204_333, "INSERT INTO public.orders VALUES (1204331, 'Customer 1204331', 3612993.31)", "duplicate key value violates unique constraint \"orders_pkey\"", EntryKind::Failed),
                        ],
                        entries_total: 3,
                        uncertain: false,
                        bytes: 311_000_000,
                        elapsed: std::time::Duration::from_secs_f64(94.2),
                    },
                    cx,
                );
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "filerun-summary");
    }
}
