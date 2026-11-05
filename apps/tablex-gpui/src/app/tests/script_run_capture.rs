// Offscreen captures of a script run (headless Metal renderer, real text system): the result tabs
// with a result on screen, the Statistics table, and the parameter dialog. The run is fed to the
// app as the model a driver run produces (the same `ScriptRun` the statements land in), so the
// screens do not need a database. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_script_run_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_script_run_screens() {
    use crate::script_run::{PlannedStatement, ResultPage, ScriptRun, ScriptTarget, StatementOutcome};
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
        let ids: Vec<i64> = (1..=14).collect();
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
    let by_status = || {
        let batch = RecordBatch::try_from_iter([
            ("status", Arc::new(StringArray::from(vec!["paid", "shipped", "pending"])) as ArrayRef),
            ("n", Arc::new(Int64Array::from(vec![7, 4, 3])) as ArrayRef),
        ])
        .unwrap();
        ArrowGridSource::new(vec![Arc::new(batch)])
    };
    let statements = [
        "SELECT id, customer, total, status FROM orders WHERE total > 100 ORDER BY total DESC",
        "UPDATE orders SET status = 'shipped' WHERE id IN (7, 9)",
        "CREATE INDEX idx_orders_status ON orders (status)",
        "SELECT status, count(*) AS n\nFROM orders\nGROUP BY status",
        "SELECT * FROM missing_table",
        "DELETE FROM orders WHERE id = 1",
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
            .open_window(gpui::size(gpui::px(1180.0), gpui::px(620.0)), |window, cx| {
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
                        "orders report",
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

        // The run: two result sets, a command, a DDL, then a failure that stops the script.
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
                    StatementOutcome::Rows { source: orders(), page: ResultPage::default() },
                    StatementOutcome::Command { affected: Some(2) },
                    StatementOutcome::Command { affected: Some(0) },
                    StatementOutcome::Rows { source: by_status(), page: ResultPage::default() },
                    StatementOutcome::Failed {
                        message: "relation \"missing_table\" does not exist".into(),
                        cancelled: false,
                        fatal: false,
                    },
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
        shoot(&mut cx, "script-result-1");

        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| this.show_script_tab(tab, crate::script_run::ScriptTab::Statistics, cx))
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "script-statistics");

        // The parameter dialog, with what the last run typed already in it.
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                let sql = "SELECT id, customer, total FROM orders\nWHERE total > :min_total AND status = ${status} AND customer <> :skip";
                this.workspace.runtime_mut(tab).unwrap().param_values.insert(
                    "min_total".into(),
                    crate::sql_params::Value { kind: crate::sql_params::ValueKind::Number, text: "150".into() },
                );
                this.workspace.runtime_mut(tab).unwrap().param_values.insert(
                    "status".into(),
                    crate::sql_params::Value { kind: crate::sql_params::ValueKind::Text, text: "paid".into() },
                );
                assert!(this.prompt_parameters(tab, sql, AfterParams::Run, cx));
            })
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "script-parameters");
    }
}
