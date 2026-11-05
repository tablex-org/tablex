// Offscreen captures of smart commit and the transaction windows: the header of a tab smart commit
// switched, the tab's transaction log, the pending-transactions list, and the smart commit switches of
// the Connection types page (headless Metal renderer, real text system, fixture data only). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_smart_commit_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_smart_commit_screens() {
    use crate::settings::connection::PresetFlag;
    use crate::transaction::{CommitMode, LogResult, Outcome};

    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();

    // The Connection types page sits far down the Query & Connections page: its own, taller window.
    for (theme_name, mode, height) in [
        ("light", gpui_component::ThemeMode::Light, 1000.0),
        ("dark", gpui_component::ThemeMode::Dark, 1000.0),
        ("light", gpui_component::ThemeMode::Light, 2500.0),
        ("dark", gpui_component::ThemeMode::Dark, 2500.0),
    ] {
        let settings_pass = height > 2000.0;
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
            .open_window(gpui::size(gpui::px(1280.0), gpui::px(height)), |window, cx| {
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

        if !settings_pass {
            // Three tabs on three fixture connections, each with a transaction open as smart commit left it.
            let lex = engine_driver_kit::sql::SqlLexOptions::postgres();
            let transaction = |statements: &[(&str, bool, LogResult, Option<u64>)], smart: bool, age: u64| {
                let mut tx = TabTx::new(CommitMode::Manual);
                tx.smart = smart;
                let started = Instant::now() - Duration::from_secs(age);
                for (sql, writes, result, rows) in statements {
                    tx.sending(started, *writes);
                    tx.log.begin(started, *writes, false, sql, &lex);
                    let error = (*result == LogResult::Failed).then_some("column \"statuss\" of relation \"orders\" does not exist");
                    tx.log.finish(*result, *rows, error, started + Duration::from_millis(7 + 31 * rows.unwrap_or(0)));
                    tx.finished(if *result == LogResult::Failed { Outcome::Failed } else { Outcome::Done }, *writes, false, false, started);
                }
                tx
            };
            let first = transaction(
                &[
                    ("SELECT id, customer, total, status FROM orders WHERE status = 'pending' ORDER BY id", false, LogResult::Ok, Some(14)),
                    ("UPDATE orders SET status = 'shipped' WHERE status = 'pending' AND total > 100", true, LogResult::Ok, Some(9)),
                    ("UPDATE orders SET statuss = 'shipped' WHERE id = 3", true, LogResult::Failed, None),
                    ("INSERT INTO audit_log (action, actor, note) VALUES ('bulk-ship', 'huy', 'pending > 100')", true, LogResult::Ok, Some(1)),
                ],
                true,
                143,
            );
            let tabs = cx
                .update_window(handle, |_, _, app_cx| {
                    app.update(app_cx, |this, cx| {
                        this.seed_connection_tree_scenario();
                        let mut tabs = Vec::new();
                        for (title, connection, sql, tx) in [
                            ("orders report", crate::ui_fixture::POSTGRES_ID, "UPDATE orders SET status = 'shipped' WHERE status = 'pending' AND total > 100;", first.clone()),
                            ("customers cleanup", crate::ui_fixture::POSTGRES_ID, "DELETE FROM customers WHERE last_seen < '2020-01-01';", {
                                let mut tx = TabTx::new(CommitMode::Manual);
                                tx.statements = 2;
                                tx.modifying = 1;
                                tx.since = Some(Instant::now() - Duration::from_secs(48));
                                tx
                            }),
                            ("archive", crate::ui_fixture::SQLITE_ID, "INSERT INTO archive SELECT * FROM events WHERE ts < date('now', '-1 year');", {
                                let mut tx = TabTx::new(CommitMode::Manual);
                                tx.statements = 7;
                                tx.modifying = 6;
                                tx.since = Some(Instant::now() - Duration::from_secs(3725));
                                tx
                            }),
                        ] {
                            let tab = this.workspace.open_query_tab_with_metadata(title, TabMetadata { connection_id: connection.into(), ..Default::default() });
                            this.workspace.tab_mut(tab).unwrap().sql = sql.into();
                            this.transactions.set_for_capture(tab, tx);
                            tabs.push(tab);
                        }
                        this.workspace.activate_tab(tabs[0]);
                        cx.notify();
                        tabs
                    })
                })
                .unwrap();
            settle(&mut cx);
            shoot(&mut cx, "f2-header");

            cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_transaction_log(tabs[0], window, cx))).unwrap();
            settle(&mut cx);
            shoot(&mut cx, "f2-log");
            cx.update_window(handle, |_, window, app_cx| window.close_dialog(app_cx)).unwrap();
            settle(&mut cx);

            cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_pending_transactions(window, cx))).unwrap();
            settle(&mut cx);
            shoot(&mut cx, "f2-pending");
            cx.update_window(handle, |_, window, app_cx| window.close_dialog(app_cx)).unwrap();
            settle(&mut cx);

            // The question asked when the window would close links to the same list.
            cx.update_window(handle, |_, window, app_cx| {
                app.update(app_cx, |this, cx| {
                    this.guard_app_exit("Quitting ends their sessions.", window, cx, |_, _, _| {});
                })
            })
            .unwrap();
            settle(&mut cx);
            shoot(&mut cx, "f2-quit-question");
            cx.update_window(handle, |_, window, app_cx| window.close_dialog(app_cx)).unwrap();
            settle(&mut cx);
        }

        // The switches on the Connection types page (smart commit on, returning to auto-commit on).
        if !settings_pass {
            continue;
        }
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                this.show_connection_types_for_capture(crate::settings::connection::AgentLevel::Off, window, cx);
                this.settings_type_set_flag(crate::settings::connection::DEVELOPMENT, PresetFlag::SmartCommit, true, cx);
                this.settings_type_set_flag(crate::settings::connection::DEVELOPMENT, PresetFlag::SmartCommitRecover, true, cx);
            })
        })
        .unwrap();
        settle(&mut cx);
        settle(&mut cx);
        shoot(&mut cx, "f2-settings");
    }
}
