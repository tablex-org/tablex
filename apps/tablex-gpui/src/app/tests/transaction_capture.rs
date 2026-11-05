// Offscreen captures of manual commit mode (headless Metal renderer, real text system): the
// toolbar of a tab with a transaction open, with the PostgreSQL aborted hint, and the question
// asked when its tab closes. The state is set on the app directly (no database). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_transaction_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_transaction_screens() {
    use crate::script_run::{PlannedStatement, ResultPage, ScriptRun, ScriptTarget, StatementOutcome};
    use crate::transaction::TabTx;
    use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};

    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let orders = || {
        let customers = ["Acme", "Globex", "Initech", "Umbrella", "Hooli", "Stark"];
        let statuses = ["paid", "shipped", "pending", "paid"];
        let ids: Vec<i64> = (1..=8).collect();
        let batch = RecordBatch::try_from_iter([
            ("id", Arc::new(Int64Array::from(ids.clone())) as ArrayRef),
            (
                "customer",
                Arc::new(StringArray::from_iter_values(ids.iter().map(|i| customers[*i as usize % 6]))) as ArrayRef,
            ),
            (
                "total",
                Arc::new(Float64Array::from_iter_values(ids.iter().map(|i| 120.0 + (*i as f64) * 37.25))) as ArrayRef,
            ),
            (
                "status",
                Arc::new(StringArray::from_iter_values(ids.iter().map(|i| statuses[*i as usize % 4]))) as ArrayRef,
            ),
        ])
        .unwrap();
        ArrowGridSource::new(vec![Arc::new(batch)])
    };
    let statements = [
        "UPDATE orders SET status = 'shipped' WHERE id IN (7, 9)",
        "DELETE FROM orders WHERE status = 'cancelled'",
        "SELECT id, customer, total, status FROM orders ORDER BY id",
    ];
    let script = statements.join(";\n");

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
            .open_window(gpui::size(gpui::px(1180.0), gpui::px(560.0)), |window, cx| {
                let view = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
                cx.new(|cx| gpui_component::Root::new(view, window, cx))
            })
            .expect("the capture window opens");
        let handle: gpui::AnyWindowHandle = window.into();
        let root = window.entity(&cx).expect("root");
        let app = cx.update(|app| root.read(app).view().clone().downcast::<AppRoot>().expect("AppRoot"));
        let tab = cx
            .update_window(handle, |_, _, app_cx| {
                app.update(app_cx, |this, cx| {
                    let saved = explain_connection(ConnectionKind::Postgres);
                    let key = saved.id.to_string();
                    this.saved_connections.push(saved);
                    let epoch = this.sessions.begin_connect(&key).unwrap();
                    this.sessions.finish_connect(&key, epoch, Ok(()));
                    let tab = this.workspace.open_query_tab_with_metadata(
                        "orders fixes",
                        TabMetadata { connection_id: key, ..Default::default() },
                    );
                    this.workspace.tab_mut(tab).unwrap().sql = script.clone();
                    this.workspace.activate_tab(tab);
                    cx.notify();
                    tab
                })
            })
            .unwrap();
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
        settle(&mut cx);
        // A result on screen, as the last statement of the script left it.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                let mut at = 0;
                let planned = statements
                    .iter()
                    .map(|sql| {
                        let statement = PlannedStatement { range: at..at + sql.len(), sql: (*sql).into(), confirmed: false };
                        at += sql.len() + 2;
                        statement
                    })
                    .collect();
                let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
                let mut script_run = ScriptRun::new(run, ScriptTarget::default(), planned, crate::settings::OnScriptError::Stop, 20);
                for outcome in [
                    StatementOutcome::Command { affected: Some(2) },
                    StatementOutcome::Command { affected: Some(0) },
                    StatementOutcome::Rows { source: orders(), page: ResultPage::default() },
                ] {
                    script_run.begin_statement();
                    std::thread::sleep(std::time::Duration::from_millis(3));
                    script_run.record(outcome);
                }
                let mut view = script_run.finish();
                view.shown = crate::script_run::ScriptTab::Result(0);
                this.land_script(tab, Some(run), view, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "tx-auto-commit");

        // Manual mode, a transaction open for a couple of minutes with three statements.
        let open_tx = |aborted_hint: bool| {
            let mut tx = TabTx::new(crate::transaction::CommitMode::Manual);
            tx.statements = 3;
            tx.modifying = 2;
            tx.since = Some(Instant::now() - Duration::from_secs(125));
            tx.aborted_hint = aborted_hint;
            tx
        };
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.transactions.set_for_capture(tab, open_tx(false));
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "tx-manual-open");

        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.transactions.set_for_capture(tab, open_tx(true));
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "tx-manual-aborted");

        // Closing the tab asks first.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.transactions.set_for_capture(tab, open_tx(false));
                cx.notify();
            })
        })
        .unwrap();
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| this.confirm_close_tab(tab, window, cx))
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "tx-close-guard");
    }
}
