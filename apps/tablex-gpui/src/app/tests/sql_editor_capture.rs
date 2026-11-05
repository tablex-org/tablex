// Offscreen captures of the SQL editor's text commands (headless Metal renderer, real text system):
// a query before and after Format SQL, the editor's search with its replace row, and a tab bound to
// a file with unsaved changes. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_sql_editor_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_sql_editor_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    const MESSY: &str = "select o.id,o.total,c.name as customer, action from orders o join customers c on c.id=o.customer_id left join order_items i on i.order_id=o.id where o.total>100 and c.region in ('eu','us') -- big orders\norder by o.total desc limit 50;\nupdate orders set status='shipped' where id=7;";
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
                        "orders report",
                        TabMetadata { connection_id: key, ..Default::default() },
                    );
                    this.workspace.tab_mut(tab).unwrap().sql = MESSY.into();
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
        shoot(&mut cx, "sql-format-before");

        // Format SQL with the selection on the update's statement only, then the whole editor.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                let editor = this.query_editors.by_tab[&tab].clone();
                editor.update(cx, |state, cx| {
                    state.focus(window, cx);
                    state.set_selected_range(0..0, cx);
                });
                this.format_sql(window, cx);
            });
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "sql-format-after");

        // The editor's search with its replace row, matches highlighted.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                let editor = this.query_editors.by_tab[&tab].clone();
                editor.update(cx, |state, cx| {
                    state.focus(window, cx);
                    state.open_search(true, cx);
                    state.set_search_query("orders", true, cx);
                });
            });
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "sql-find-replace");

        // A tab bound to a file, edited since it was saved.
        cx.update_window(handle, |_, window, app_cx| {
            app.update(app_cx, |this, cx| {
                let editor = this.query_editors.by_tab[&tab].clone();
                editor.update(cx, |state, cx| state.close_search(cx));
                let saved: Arc<str> = Arc::from("select 1;\n");
                if let Some(tab) = this.workspace.tab_mut(tab) {
                    tab.title = "orders.sql".into();
                    tab.file = Some(crate::workspace::SqlFile { path: "/tmp/orders.sql".into(), saved });
                }
                cx.notify();
                let _ = window;
            });
        })
        .unwrap();
        settle(&mut cx);
        shoot(&mut cx, "sql-file-unsaved");
    }
}
