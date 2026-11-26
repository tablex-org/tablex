// Real-interaction proofs for the Error Log: the status-bar icon and badge,
// the resizable pane, filters, details dialog, toolbar actions, persistence.
// Pure model and file-rotation behaviour is covered in `src/error_log.rs`.

fn error_log_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    (app, visual)
}

fn log_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tablex-errlog-ui-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn report(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    f: impl FnOnce(&mut AppRoot),
) {
    app.update(visual, |this, cx| {
        f(this);
        cx.notify();
    });
    settle(visual);
}

fn log_len(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> usize {
    app.read_with(visual, |this, _| this.error_log.log.len())
}

fn visible_rows(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    app.update(visual, |this, _| this.error_log.visible_len())
}

fn open_log_pane(visual: &mut gpui::VisualTestContext) {
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_some(), "pane opens");
}

/// Clicks `selector` until the clipboard holds `needle`. Parallel tests share
/// the platform clipboard, so a single write/read pair can be overwritten by
/// an unrelated test; a retry makes the check about our click, not the race.
fn click_copies(visual: &mut gpui::VisualTestContext, selector: &'static str, needle: &str) -> String {
    let mut copied = String::new();
    for _ in 0..10 {
        click_debug_button(visual, selector);
        copied = visual.cx.read_from_clipboard().and_then(|item| item.text()).unwrap_or_default();
        if copied.contains(needle) {
            break;
        }
    }
    copied
}

fn double_click(visual: &mut gpui::VisualTestContext, position: gpui::Point<gpui::Pixels>) {
    visual.simulate_click(position, Default::default());
    visual.simulate_event(gpui::MouseDownEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
    });
    settle(visual);
}

#[gpui::test]
fn failing_query_lights_the_icon_and_opening_the_pane_clears_it(cx: &mut TestAppContext) {
    let db = SqliteDb::new(ITEMS_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let visual = rig.visual;
    assert!(visual.debug_bounds("error-log-toggle").is_some(), "icon is in the status bar");
    assert!(visual.debug_bounds("error-log-badge").is_none(), "nothing new: no badge");
    assert!(visual.debug_bounds("error-log-pane").is_none(), "pane starts closed");

    // A real failing query on the real SQLite driver.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().sql = "SELECT * FROM no_such_table".into();
        this.run_active_tab_query(cx);
    });
    wait_until(visual, "the failing query", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.active_tab().is_some_and(|tab| {
                matches!(
                    this.workspace.runtime(tab.id).unwrap().query.status(),
                    crate::query::QueryStatus::Failed(_)
                )
            })
        })
    });
    settle(visual);
    app.read_with(visual, |this, _| {
        let log = &this.error_log.log;
        assert_eq!(log.len(), 1);
        let entry = log.last().unwrap();
        assert_eq!(entry.severity, crate::error_log::Severity::Error);
        assert_eq!(entry.area, Area::Query);
        assert!(entry.message.to_lowercase().contains("no_such_table"), "{}", entry.message);
        assert_eq!(log.unseen_count(), 1);
    });
    assert!(visual.debug_bounds("error-log-badge").is_some(), "unseen error shows the badge");
    assert!(
        visual.debug_bounds("status-latest-error").is_some(),
        "status text shows the latest error"
    );
    assert!(visual.debug_bounds("error-log-pane").is_none(), "a failure does not pop the pane open");

    // Clicking the icon opens the pane and marks everything seen.
    open_log_pane(visual);
    app.read_with(visual, |this, _| {
        assert!(this.error_log.open);
        assert_eq!(this.error_log.log.unseen_count(), 0);
    });
    assert!(visual.debug_bounds("error-log-badge").is_none(), "opening clears the badge");
    assert!(visual.debug_bounds("status-latest-error").is_none());
    assert!(visual.debug_bounds("error-row-0").is_some(), "the failure is listed");
    let pane = visual.debug_bounds("error-log-pane").unwrap();
    let status = visual.debug_bounds("status-bar").unwrap();
    assert!(
        pane.bottom() <= status.top() + gpui::px(1.0),
        "pane sits above the status bar: {pane:?} {status:?}"
    );

    // The icon toggles the pane closed again.
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_none());
}

#[gpui::test]
fn icon_color_follows_the_highest_unseen_severity_and_status_text_opens_the_entry(
    cx: &mut TestAppContext,
) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| this.report_warning(Area::Driver, None, "slow driver"));
    let warning_badge = visual.debug_bounds("error-log-badge").expect("warning badge");
    report(&app, visual, |this| this.report_error(Area::Connect, None, "connection refused"));
    app.read_with(visual, |this, _| {
        assert_eq!(this.error_log.log.unseen_severity(), Some(crate::error_log::Severity::Error));
        assert_eq!(this.error_log.log.unseen_count(), 2);
    });
    assert!(visual.debug_bounds("error-log-badge").is_some());
    let _ = warning_badge;

    // The status text names the newest error; clicking it opens the pane on that row.
    click_debug_button(visual, "status-latest-error");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_some());
    app.read_with(visual, |this, _| {
        let selected = this.error_log.selected_entry().expect("entry selected");
        assert_eq!(selected.first_line.as_ref(), "connection refused");
        assert_eq!(this.error_log.log.unseen_count(), 0);
    });
}

#[gpui::test]
fn severity_pills_and_search_filter_the_table(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "relation orders does not exist");
        this.report_warning(Area::Driver, None, "driver is slow");
        this.report_error(Area::Navigator, None, "listing tables failed");
        this.report_event(None, Report::info(Area::Export, "export finished"));
    });
    open_log_pane(visual);
    assert_eq!(visible_rows(&app, visual), 4);

    click_debug_button(visual, "error-filter-errors");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 2);
    click_debug_button(visual, "error-filter-warnings");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 1);
    click_debug_button(visual, "error-filter-info");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 1);
    click_debug_button(visual, "error-filter-all");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 4);

    // Typing in the search input filters case-insensitively on message / source.
    let search = visual.debug_bounds("error-log-search").expect("search input");
    visual.simulate_click(search.center(), Default::default());
    settle(visual);
    visual.simulate_input("ORDERS");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 1, "message match");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("navigator");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 1, "source match");
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("zzz-nothing");
    settle(visual);
    assert_eq!(visible_rows(&app, visual), 0);
    assert!(visual.debug_bounds("error-log-empty").is_some(), "empty-filter state");
    assert_eq!(log_len(&app, visual), 4, "filtering never drops entries");
}

#[gpui::test]
fn header_click_sorts_by_source_and_date(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "first");
        this.report_error(Area::Admin, None, "second");
        this.report_error(Area::Connect, None, "third");
    });
    open_log_pane(visual);
    let first_line = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, pos: usize| {
        app.update(visual, |this, _| {
            this.error_log.visible_len();
            let ui = &this.error_log;
            ui.log.get(ui.visible_index(pos)).unwrap().first_line.to_string()
        })
    };
    assert_eq!(first_line(&app, visual, 0), "third", "newest first by default");
    click_debug_button(visual, "error-sort-source");
    settle(visual);
    assert_eq!(first_line(&app, visual, 0), "second", "Admin sorts before Connect and Query");
    click_debug_button(visual, "error-sort-source");
    settle(visual);
    assert_eq!(first_line(&app, visual, 0), "first", "second click reverses");
    click_debug_button(visual, "error-sort-date");
    settle(visual);
    assert_eq!(first_line(&app, visual, 0), "third");
    click_debug_button(visual, "error-sort-date");
    settle(visual);
    assert_eq!(first_line(&app, visual, 0), "first", "oldest first");
}

#[gpui::test]
fn double_click_and_enter_open_event_details_with_code_and_retryable(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| {
        this.report_event(
            None,
            Report::warning(Area::Query, "operation was interrupted\nsecond line")
                .code("query.interrupted")
                .retryable(true),
        );
        this.report_error(Area::Query, None, "plain failure without a code");
    });
    open_log_pane(visual);

    // Row 1 (older) is the interrupted warning; rows are newest first.
    let row = visual.debug_bounds("error-row-1").expect("second row");
    double_click(visual, row.center());
    assert!(visual.debug_bounds("error-details").is_some(), "double-click opens the dialog");
    assert!(visual.debug_bounds("error-details-code").is_some(), "code row");
    assert!(visual.debug_bounds("error-details-retryable").is_some());
    assert!(visual.debug_bounds("error-details-severity").is_some());
    assert!(visual.debug_bounds("error-details-message").is_some());
    app.read_with(visual, |this, _| {
        let entry = this.error_log.selected_entry().unwrap();
        assert_eq!(entry.code.as_deref(), Some("query.interrupted"));
        assert!(entry.retryable);
    });
    let copied = click_copies(visual, "error-details-copy", "query.interrupted");
    assert!(copied.contains("query.interrupted"), "Copy puts the event on the clipboard: {copied}");
    click_debug_button(visual, "error-details-close");
    settle(visual);
    assert!(visual.debug_bounds("error-details").is_none(), "Close dismisses it");

    // An entry without a code has no code row; Enter opens details for the selection.
    let row = visual.debug_bounds("error-row-0").unwrap();
    visual.simulate_click(row.center(), Default::default());
    settle(visual);
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert!(visual.debug_bounds("error-details").is_some(), "Enter opens the dialog");
    assert!(visual.debug_bounds("error-details-code").is_none(), "no code, no code row");
}

#[gpui::test]
fn arrow_keys_move_the_selection_and_escape_closes_the_pane(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "a");
        this.report_error(Area::Query, None, "b");
        this.report_error(Area::Query, None, "c");
    });
    open_log_pane(visual);
    visual.simulate_keystrokes("down");
    let selected = |app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext| {
        app.read_with(visual, |this, _| {
            this.error_log.selected_entry().map(|entry| entry.first_line.to_string())
        })
    };
    assert_eq!(selected(&app, visual).as_deref(), Some("c"), "first Down selects the top row");
    visual.simulate_keystrokes("down");
    assert_eq!(selected(&app, visual).as_deref(), Some("b"));
    visual.simulate_keystrokes("up");
    assert_eq!(selected(&app, visual).as_deref(), Some("c"));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_none(), "Escape closes the pane");
}

#[gpui::test]
fn clear_viewer_empties_the_table_but_keeps_the_file(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let dir = log_temp_dir("clear");
    app.update(visual, |this, cx| {
        this.init_error_log(Some(crate::error_log::LogStore::new(&dir)), cx);
    });
    settle(visual);
    report(&app, visual, |this| this.report_error(Area::Query, None, "will be cleared"));
    wait_until(visual, "the log file", |_| dir.join("errors.log").exists());
    open_log_pane(visual);
    assert!(visual.debug_bounds("error-row-0").is_some());
    click_debug_button(visual, "error-log-clear");
    settle(visual);
    assert_eq!(log_len(&app, visual), 0);
    assert!(visual.debug_bounds("error-row-0").is_none());
    assert!(visual.debug_bounds("error-log-empty").is_some());
    assert!(dir.join("errors.log").exists(), "Clear viewer leaves the log file alone");
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn export_writes_the_log_to_the_chosen_file(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "export me");
        this.report_event(
            None,
            Report::warning(Area::Driver, "retry later").code("query.interrupted").retryable(true),
        );
    });
    open_log_pane(visual);
    click_debug_button(visual, "error-log-export");
    assert!(visual.did_prompt_for_new_path(), "Export asks for a destination");
    let dir = log_temp_dir("export");
    let target = dir.join("exported.txt");
    // Cancelling writes nothing.
    visual.simulate_new_path_selection(|_| None);
    settle(visual);
    assert!(!target.exists());

    click_debug_button(visual, "error-log-export");
    let chosen = target.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    wait_until(visual, "the exported file", |_| target.exists());
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.contains("export me") && text.contains("retry later"), "{text}");
    assert!(text.contains("(query.interrupted) retryable"), "{text}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn copy_selected_puts_the_entry_on_the_clipboard(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    report(&app, visual, |this| this.report_error(Area::Query, None, "copy this failure"));
    open_log_pane(visual);
    let button = visual.debug_bounds("error-log-copy").unwrap();
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
    assert!(
        visual.cx.read_from_clipboard().and_then(|item| item.text()).is_none_or(|t| !t.contains("copy this failure")),
        "Copy is disabled until a row is selected"
    );
    let row = visual.debug_bounds("error-row-0").unwrap();
    visual.simulate_click(row.center(), Default::default());
    settle(visual);
    let copied = click_copies(visual, "error-log-copy", "copy this failure");
    assert!(copied.contains("copy this failure") && copied.contains("[ERROR] Query"), "{copied}");
}

#[gpui::test]
fn delete_log_confirms_then_removes_the_file_and_the_viewer(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let dir = log_temp_dir("delete");
    app.update(visual, |this, cx| {
        this.init_error_log(Some(crate::error_log::LogStore::new(&dir)), cx);
    });
    settle(visual);
    report(&app, visual, |this| this.report_error(Area::Query, None, "to be deleted"));
    wait_until(visual, "the log file", |_| dir.join("errors.log").exists());
    open_log_pane(visual);
    click_debug_button(visual, "error-log-delete");
    settle(visual);
    assert_eq!(log_len(&app, visual), 1, "nothing is deleted before the confirmation");
    assert!(dir.join("errors.log").exists());
    visual.simulate_keystrokes("enter");
    wait_until(visual, "the log file removal", |_| !dir.join("errors.log").exists());
    assert_eq!(log_len(&app, visual), 0);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn reports_persist_off_the_ui_thread(cx: &mut TestAppContext) {
    let dir = log_temp_dir("persist");
    let (app, visual) = error_log_app(cx);
    app.update(visual, |this, cx| {
        this.init_error_log(Some(crate::error_log::LogStore::new(&dir)), cx);
    });
    settle(visual);
    report(&app, visual, |this| {
        this.report_event(
            Some("11111111-1111-1111-1111-111111111111"),
            Report::error(Area::Navigator, "persist me").code("session.not_connected"),
        );
        // Queued, not written: the UI thread never touches the file.
        assert!(!this.error_log.store.as_ref().unwrap().path().exists());
    });
    wait_until(visual, "the log file", |_| dir.join("errors.log").exists());
    let written = std::fs::read_to_string(dir.join("errors.log")).unwrap();
    assert!(written.contains("persist me") && written.contains("session.not_connected"), "{written}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn previous_session_entries_are_restored_already_seen(cx: &mut TestAppContext) {
    let dir = log_temp_dir("restore");
    let store = crate::error_log::LogStore::new(&dir);
    let mut earlier = crate::error_log::ErrorLog::default();
    earlier.record(
        Report::error(Area::Navigator, "persist me").code("session.not_connected"),
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
    store.append(&earlier.last().unwrap().to_line()).unwrap();
    let (app, visual) = error_log_app(cx);
    app.update(visual, |this, cx| this.init_error_log(Some(store.clone()), cx));
    wait_until(visual, "restored entries", |visual| log_len(&app, visual) == 1);
    app.read_with(visual, |this, _| {
        let entry = this.error_log.log.last().unwrap();
        assert_eq!(entry.first_line.as_ref(), "persist me");
        assert_eq!(entry.code.as_deref(), Some("session.not_connected"));
        assert_eq!(this.error_log.log.unseen_count(), 0, "restored entries are seen");
    });
    assert!(visual.debug_bounds("error-log-badge").is_none(), "no badge for old entries");
    // New reports append after the restored ones (and are not re-read as duplicates).
    report(&app, visual, |this| this.report_error(Area::Query, None, "fresh"));
    open_log_pane(visual);
    assert_eq!(visible_rows(&app, visual), 2);
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn repeated_auto_refresh_failures_collapse_into_one_row_with_a_count(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let tab = app.update(visual, |this, _| {
        let tab = this.workspace.open_query_tab("Refresh");
        this.workspace.active_tab_mut().unwrap().connection_id =
            "11111111-1111-1111-1111-111111111111".into();
        tab
    });
    for _ in 0..4 {
        app.update(visual, |this, cx| {
            let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
            this.apply_query_result(tab, run, Err("connection reset by peer".into()));
            cx.notify();
        });
        settle(visual);
    }
    assert_eq!(log_len(&app, visual), 1, "identical consecutive failures collapse");
    app.read_with(visual, |this, _| {
        let entry = this.error_log.log.last().unwrap();
        assert_eq!(entry.repeat, 4);
        assert_eq!(this.error_log.log.unseen_count(), 1);
    });
    // A different failure starts a new row.
    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        this.apply_query_result(tab, run, Err("syntax error".into()));
        cx.notify();
    });
    assert_eq!(log_len(&app, visual), 2);
    open_log_pane(visual);
    assert!(visual.debug_bounds("error-row-1").is_some());
}

/// A server that ran out of memory is said so in plain words first, in the result area and in the
/// log entry; the driver's own text stays in both.
#[gpui::test]
fn a_server_out_of_memory_failure_leads_with_a_plain_line_and_keeps_the_driver_text(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let tab = app.update(visual, |this, _| this.workspace.open_query_tab("Memory"));
    let raw = "driver rejected the request: query.failed: ERROR: out of memory DETAIL: Failed on request of size 8192 in memory context \"MessageContext\".";
    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        this.apply_query_result(tab, run, Err(raw.into()));
        cx.notify();
    });
    settle(visual);
    assert!(visual.debug_bounds("result-empty-state").is_some(), "the result area shows the failure");
    app.read_with(visual, |this, _| {
        let crate::query::QueryStatus::Failed(error) = this.workspace.runtime(tab).unwrap().query.status() else {
            panic!("the run failed");
        };
        let shown = super::results::grid::failed_query_text(error);
        assert!(shown.starts_with("The database server ran out of memory."), "{shown}");
        assert!(shown.contains("not in tablex") && shown.contains("Failed on request of size 8192"), "{shown}");
        let entry = this.error_log.log.last().unwrap();
        assert!(entry.first_line.starts_with("The database server ran out of memory."), "{}", entry.first_line);
        assert_eq!(entry.code.as_deref(), Some("query.failed"));
        assert!(entry.details.as_deref().unwrap().contains("Failed on request of size 8192"));
    });
}

#[gpui::test]
fn cancelled_runs_are_not_errors_and_interrupts_are_warnings(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let tab = app.update(visual, |this, _| this.workspace.open_query_tab("Cancel"));
    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        // The user cancels; the late driver answer is ignored entirely.
        this.workspace.runtime_mut(tab).unwrap().query.cancel();
        this.apply_query_result(tab, run, Err("driver rejected the request: query.cancelled: stopped".into()));
        cx.notify();
    });
    assert_eq!(log_len(&app, visual), 0, "a cancelled run's late failure is dropped");
    app.update(visual, |this, cx| {
        let run = this.workspace.runtime_mut(tab).unwrap().query.begin().unwrap();
        this.apply_query_result(
            tab,
            run,
            Err("driver rejected the request: query.interrupted: interrupted by the server".into()),
        );
        cx.notify();
    });
    app.read_with(visual, |this, _| {
        let entry = this.error_log.log.last().unwrap();
        assert_eq!(entry.severity, crate::error_log::Severity::Warning);
        assert_eq!(entry.code.as_deref(), Some("query.interrupted"));
        assert!(entry.retryable);
    });
}

#[gpui::test]
fn secrets_never_reach_the_viewer_or_the_file(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let dir = log_temp_dir("secrets");
    app.update(visual, |this, cx| {
        this.init_error_log(Some(crate::error_log::LogStore::new(&dir)), cx);
    });
    settle(visual);
    report(&app, visual, |this| {
        this.report_error(
            Area::Connect,
            None,
            "could not connect: postgres://alice:hunter2@db.internal:5432/app refused",
        );
    });
    app.read_with(visual, |this, _| {
        let entry = this.error_log.log.last().unwrap();
        assert!(!entry.message.contains("hunter2") && !entry.message.contains("alice"));
    });
    wait_until(visual, "the log file", |_| dir.join("errors.log").exists());
    let written = std::fs::read_to_string(dir.join("errors.log")).unwrap();
    assert!(!written.contains("hunter2") && !written.contains("alice"), "{written}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[gpui::test]
fn pane_is_resizable_and_keeps_its_height_when_reopened(cx: &mut TestAppContext) {
    let (_app, visual) = error_log_app(cx);
    open_log_pane(visual);
    let pane = visual.debug_bounds("error-log-pane").unwrap();
    assert!(
        (pane.size.height - gpui::px(200.0)).abs() < gpui::px(6.0),
        "default height is about 200px, got {:?}",
        pane.size.height
    );
    // Drag the divider (the pane's top edge) 60px up.
    let grab = gpui::point(pane.center().x, pane.top());
    visual.simulate_mouse_down(grab, gpui::MouseButton::Left, Default::default());
    // The first move starts the drag; the group re-reads it on the next paint.
    for dy in [4.0, 30.0, 60.0] {
        visual.simulate_mouse_move(
            gpui::point(grab.x, grab.y - gpui::px(dy)),
            gpui::MouseButton::Left,
            Default::default(),
        );
        settle(visual);
    }
    visual.simulate_mouse_up(
        gpui::point(grab.x, grab.y - gpui::px(60.0)),
        gpui::MouseButton::Left,
        Default::default(),
    );
    settle(visual);
    let resized = visual.debug_bounds("error-log-pane").unwrap();
    assert!(
        resized.size.height > pane.size.height + gpui::px(40.0),
        "dragging the divider up grows the pane: {:?} -> {:?}",
        pane.size.height,
        resized.size.height
    );
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    open_log_pane(visual);
    let reopened = visual.debug_bounds("error-log-pane").unwrap();
    assert!(
        (reopened.size.height - resized.size.height).abs() < gpui::px(6.0),
        "height is remembered across close/open: {:?} vs {:?}",
        reopened.size.height,
        resized.size.height
    );
}

#[gpui::test]
fn scenario_and_test_runs_never_write_the_real_log_dir(cx: &mut TestAppContext) {
    let _ = cx;
    assert!(crate::error_log::LogStore::resolve(None).is_none(), "plain test roots persist nothing");
    let root = std::env::temp_dir().join("tablex-scenario-x");
    let store = crate::error_log::LogStore::resolve(Some(&root)).unwrap();
    assert!(store.dir().starts_with(&root), "scenario logs live under the scenario temp root");
}

#[gpui::test]
fn row_cells_start_at_the_same_x_as_their_header_cells(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    let connection = "11111111-1111-1111-1111-111111111111";
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "short");
        // Long single line: must be clipped into its cell, not push the others.
        this.report_error(
            Area::Navigator,
            Some(connection),
            format!("a very long failure message {}", "that keeps going ".repeat(60)),
        );
        // A repeated entry carries the extra x-count chip inside the Message cell.
        for _ in 0..3 {
            this.report_error(Area::Export, None, "repeated");
        }
    });
    open_log_pane(visual);
    let probe = |name: &str| -> &'static str { Box::leak(name.to_string().into_boxed_str()) };
    let bounds = |visual: &mut gpui::VisualTestContext, selector: &str| {
        visual
            .debug_bounds(probe(selector))
            .unwrap_or_else(|| panic!("{selector} is rendered"))
    };
    let pane = visual.debug_bounds("error-log-pane").unwrap();
    for column in ["message", "source", "date"] {
        let header = bounds(visual, &format!("error-col-{column}"));
        for row in 0..3 {
            let cell = bounds(visual, &format!("error-cell-{row}-{column}"));
            assert_eq!(cell.left(), header.left(), "row {row} {column} starts under its header");
            assert_eq!(cell.size.width, header.size.width, "row {row} {column} has the header's width");
        }
    }
    // Rows span the pane and the columns keep their order and fixed widths.
    let (message, source, date) = (
        bounds(visual, "error-col-message"),
        bounds(visual, "error-col-source"),
        bounds(visual, "error-col-date"),
    );
    assert!(message.right() <= source.left() + gpui::px(0.5) && source.right() <= date.left() + gpui::px(0.5));
    assert_eq!(source.size.width, gpui::px(200.0));
    assert_eq!(date.size.width, gpui::px(170.0));
    assert!(date.right() <= pane.right(), "the table fits the pane");
    let row = bounds(visual, "error-row-0");
    assert!(row.size.width >= pane.size.width - gpui::px(1.0), "rows fill the pane width");
}

#[gpui::test]
fn cmd_shift_l_toggles_the_pane(cx: &mut TestAppContext) {
    let (app, visual) = error_log_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    // Keyboard focus must be somewhere inside the app for an action to reach it.
    tb_focus_root(&app, visual);
    visual.simulate_keystrokes("secondary-shift-l");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_some(), "shortcut opens the pane");
    visual.simulate_keystrokes("secondary-shift-l");
    settle(visual);
    assert!(visual.debug_bounds("error-log-pane").is_none(), "and closes it again");
}

// ---- placement: bottom, right or a tab of its own ----------------------------------

/// Picks `slug` ("below", "right", "tab") in the Error Log toolbar's position menu.
fn pick_error_log_place(visual: &mut gpui::VisualTestContext, slug: &str) {
    click_debug_button(visual, "error-log-place");
    settle(visual);
    let item = Box::leak(format!("error-log-place-{slug}").into_boxed_str());
    click_debug_button(visual, item);
    settle(visual);
}

fn error_log_place_of(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> crate::settings::ErrorLogPlace {
    app.read_with(visual, |this, _| this.error_log_place())
}

/// A new SQL tab (the editor area is on screen) with the Error Log opened from the status bar.
fn query_tab_with_log(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    let (app, visual) = layout_app(cx);
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let query = active_tab(&app, visual);
    open_log_pane(visual);
    (app, visual, query)
}

#[gpui::test]
fn the_position_menu_moves_the_pane_bottom_right_and_to_a_tab(cx: &mut TestAppContext) {
    use crate::settings::ErrorLogPlace;
    let (app, visual, query) = query_tab_with_log(cx);
    // Bottom (the default): under the editor, full width.
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let pane = visual.debug_bounds("error-log-pane").unwrap();
    assert!(pane.top() >= area.bottom() - gpui::px(1.0), "below the editor: {area:?} {pane:?}");
    assert!(pane.size.width > gpui::px(900.0), "full width: {pane:?}");
    assert!(visual.debug_bounds("error-log-place").is_some(), "the position button is in the toolbar");
    let close = visual.debug_bounds("error-log-close").unwrap();
    let place = visual.debug_bounds("error-log-place").unwrap();
    assert!(place.right() <= close.left(), "left of the close button: {place:?} {close:?}");

    // Right: beside the editor and tab strip area, full height of the main area.
    pick_error_log_place(visual, "right");
    assert_eq!(error_log_place_of(&app, visual), ErrorLogPlace::Right);
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let pane = visual.debug_bounds("error-log-pane").expect("pane");
    assert!(pane.left() >= area.right() - gpui::px(1.0), "right of the editor: {area:?} {pane:?}");
    assert!(pane.size.height > gpui::px(500.0), "full height: {pane:?}");
    assert!(pane.size.width < gpui::px(600.0), "a side pane, not the whole width: {pane:?}");
    let status = visual.debug_bounds("status-bar").unwrap();
    assert!(pane.bottom() <= status.top() + gpui::px(1.0), "above the status bar");
    assert!(visual.debug_bounds("error-log-place").is_some());

    // A tab of its own: the tab shows the pane, the query tab's editor is not on screen.
    pick_error_log_place(visual, "tab");
    assert_eq!(error_log_place_of(&app, visual), ErrorLogPlace::Tab);
    let tab = active_tab(&app, visual);
    assert_ne!(tab, query);
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tab(tab).unwrap().kind, TabKind::ErrorLog);
        assert_eq!(this.error_log.tab, Some(tab));
    });
    let pane = visual.debug_bounds("error-log-pane").expect("pane in the tab");
    assert!(visual.debug_bounds("query-editor-area").is_none(), "the tab shows the log only");
    assert!(pane.size.width > gpui::px(500.0) && pane.size.height > gpui::px(400.0), "fills the tab: {pane:?}");

    // Back to the bottom: the tab closes, the pane is under the editor again.
    pick_error_log_place(visual, "below");
    assert_eq!(error_log_place_of(&app, visual), ErrorLogPlace::Bottom);
    app.read_with(visual, |this, _| {
        assert!(this.workspace.tab(tab).is_none(), "the Error Log tab closed");
        assert!(this.error_log.tab.is_none() && this.error_log.open, "still open, now docked");
    });
    assert_eq!(active_tab(&app, visual), query);
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let pane = visual.debug_bounds("error-log-pane").expect("pane");
    assert!(pane.top() >= area.bottom() - gpui::px(1.0), "below again: {area:?} {pane:?}");
}

#[test]
fn narrow_panes_drop_source_then_date_and_keep_the_message_readable() {
    use super::error_log_view::ColumnLayout;
    assert_eq!(ColumnLayout::for_width(1000.0), ColumnLayout::ALL);
    assert_eq!(ColumnLayout::for_width(480.0), ColumnLayout { source: false, date: true });
    assert_eq!(ColumnLayout::for_width(360.0), ColumnLayout { source: false, date: true });
    assert_eq!(ColumnLayout::for_width(320.0), ColumnLayout { source: false, date: false });
}

/// The Right place at the default 1100pt window: the table keeps a readable Message column, the
/// toolbar rows are one line each and never overlap, and rows line up under their headers.
#[gpui::test]
fn right_pane_at_the_default_window_size_has_a_readable_table_and_no_overlap(cx: &mut TestAppContext) {
    let (app, visual, _query) = query_tab_with_log(cx);
    report(&app, visual, |this| {
        this.report_error(Area::Query, None, "short");
        this.report_error(
            Area::Navigator,
            None,
            format!("a very long failure message {}", "that keeps going ".repeat(40)),
        );
        this.report_warning(Area::App, None, "a warning");
    });
    pick_error_log_place(visual, "right");
    let pane = visual.debug_bounds("error-log-pane").expect("pane");
    assert!(
        pane.size.width >= gpui::px(359.0) && pane.size.width <= gpui::px(460.0),
        "about 40% of the content area, at least 360pt: {pane:?}"
    );

    let header = visual.debug_bounds("error-log-header").unwrap();
    let filters = visual.debug_bounds("error-log-filters").unwrap();
    let columns = visual.debug_bounds("error-col-message").unwrap();
    assert!(header.bottom() <= filters.top() + gpui::px(0.5), "title row above the filters: {header:?} {filters:?}");
    assert!(filters.bottom() <= columns.top() + gpui::px(0.5), "filters above the table header: {filters:?} {columns:?}");
    assert!(header.size.height < gpui::px(40.0) && filters.size.height <= gpui::px(32.5), "one line each");
    // Title, pills and search sit left to right inside their row; the close button ends the title row.
    let title_count = visual.debug_bounds("error-log-count").unwrap();
    let place = visual.debug_bounds("error-log-place").unwrap();
    let close = visual.debug_bounds("error-log-close").unwrap();
    assert!(title_count.right() <= place.left() && close.right() <= pane.right() + gpui::px(0.5));
    let last_pill = visual.debug_bounds("error-filter-info").unwrap();
    let search = visual.debug_bounds("error-log-search").unwrap();
    assert!(last_pill.right() <= search.left() + gpui::px(0.5), "pills then search: {last_pill:?} {search:?}");
    assert!(search.right() <= pane.right() + gpui::px(0.5), "search inside the pane: {search:?}");
    assert!(search.size.width >= gpui::px(72.0));

    // The Message column keeps the room; Source is dropped, Date stays.
    assert!(columns.size.width >= gpui::px(160.0), "Message is readable: {columns:?}");
    assert!(visual.debug_bounds("error-col-source").is_none(), "Source is hidden in a narrow pane");
    let date = visual.debug_bounds("error-col-date").expect("Date stays");
    assert!(columns.right() <= date.left() + gpui::px(0.5) && date.right() <= pane.right());
    for row in 0..3 {
        let message = visual.debug_bounds(Box::leak(format!("error-cell-{row}-message").into_boxed_str())).unwrap();
        let cell = visual.debug_bounds(Box::leak(format!("error-cell-{row}-date").into_boxed_str())).unwrap();
        assert_eq!(message.left(), columns.left(), "row {row} starts under the header");
        assert_eq!(message.size.width, columns.size.width, "row {row} has the header's width");
        assert_eq!(cell.size.width, date.size.width);
        assert!(message.size.height < gpui::px(30.0), "row {row} is one line, not one letter per line");
    }

    // Widening the pane brings Source back; the pane stays resizable.
    let grab = gpui::point(pane.left(), pane.center().y);
    visual.simulate_mouse_down(grab, gpui::MouseButton::Left, Default::default());
    for dx in [4.0, 60.0, 200.0] {
        visual.simulate_mouse_move(
            gpui::point(grab.x - gpui::px(dx), grab.y),
            gpui::MouseButton::Left,
            Default::default(),
        );
        settle(visual);
    }
    visual.simulate_mouse_up(
        gpui::point(grab.x - gpui::px(200.0), grab.y),
        gpui::MouseButton::Left,
        Default::default(),
    );
    settle(visual);
    settle(visual);
    let wide = visual.debug_bounds("error-log-pane").unwrap();
    assert!(wide.size.width > pane.size.width + gpui::px(100.0), "dragging the divider left widens it: {pane:?} -> {wide:?}");
    if wide.size.width >= gpui::px(574.0) {
        assert!(visual.debug_bounds("error-col-source").is_some(), "Source returns in a wide pane");
    }
    assert!(visual.debug_bounds("error-col-message").unwrap().size.width >= gpui::px(160.0));
}

#[gpui::test]
fn moving_the_pane_keeps_filter_search_selection_sort_and_scroll(cx: &mut TestAppContext) {
    let (app, visual, _query) = query_tab_with_log(cx);
    report(&app, visual, |this| {
        for index in 0..60 {
            this.report_error(Area::Query, None, format!("failure number {index}"));
        }
        this.report_warning(Area::App, None, "a warning that the Errors pill hides");
    });
    click_debug_button(visual, "error-filter-errors");
    click_debug_button(visual, "error-sort-source");
    settle(visual);
    let search = app.read_with(visual, |this, _| this.error_log.search.clone().unwrap());
    visual.update(|window, cx| search.update(cx, |input, cx| input.set_value("failure", window, cx)));
    settle(visual);
    app.update(visual, |this, cx| {
        let id = this.error_log.log.get(20).unwrap().id;
        this.error_log.selected = Some(id);
        this.error_log.scroll.scroll_to_item(20, gpui::ScrollStrategy::Top);
        cx.notify();
    });
    settle(visual);
    let snapshot = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        app.read_with(visual, |this, cx| {
            (
                this.error_log.spec.clone(),
                this.error_log.selected,
                this.error_log.search.as_ref().unwrap().read(cx).value().to_string(),
                f32::from(this.error_log.scroll.0.borrow().base_handle.offset().y),
            )
        })
    };
    let before = snapshot(&app, visual);
    assert_eq!(before.0.severity, crate::error_log::SeverityFilter::Errors);
    assert_eq!(before.0.sort, crate::error_log::SortKey::Source);
    assert_eq!(before.2, "failure");
    assert!(before.3 < -100.0, "scrolled down: {}", before.3);
    for place in ["right", "tab", "below", "right"] {
        pick_error_log_place(visual, place);
        let after = snapshot(&app, visual);
        assert_eq!(after.0, before.0, "filter, search and sort spec after {place}");
        assert_eq!(after.1, before.1, "selection after {place}");
        assert_eq!(after.2, before.2, "search text after {place}");
        assert!((after.3 - before.3).abs() < 1.0, "scroll after {place}: {} vs {}", after.3, before.3);
        let pill = visual.debug_bounds("error-filter-errors");
        assert!(pill.is_some(), "the pane is on screen after {place}");
    }
}

#[gpui::test]
fn the_toggle_works_in_every_place(cx: &mut TestAppContext) {
    use crate::settings::ErrorLogPlace;
    let (app, visual) = layout_app(cx);
    visual.update(|_, cx| actions::bind_keys(cx));
    click_debug_button(visual, "global-new-query");
    settle(visual);
    let query = active_tab(&app, visual);
    for place in [ErrorLogPlace::Bottom, ErrorLogPlace::Right, ErrorLogPlace::Tab] {
        app.update(visual, |this, cx| {
            let mut next = (**this.settings).clone();
            next.appearance.error_log_place = place;
            this.apply_settings(next, None, cx);
        });
        settle(visual);
        assert!(visual.debug_bounds("error-log-pane").is_none(), "{place:?}: closed to start with");
        click_debug_button(visual, "error-log-toggle");
        settle(visual);
        assert!(visual.debug_bounds("error-log-pane").is_some(), "{place:?}: the icon opens it");
        assert_eq!(active_tab(&app, visual) != query, place == ErrorLogPlace::Tab, "{place:?}: only a tab place has a tab");
        click_debug_button(visual, "error-log-toggle");
        settle(visual);
        assert!(visual.debug_bounds("error-log-pane").is_none(), "{place:?}: the icon closes it");
        app.read_with(visual, |this, _| {
            assert!(!this.error_log.open && this.error_log.tab.is_none(), "{place:?}: closed, no tab left");
            assert_eq!(this.workspace.tabs().iter().filter(|tab| tab.kind == TabKind::ErrorLog).count(), 0);
        });
        assert_eq!(active_tab(&app, visual), query);
        // The shortcut too.
        tb_focus_root(&app, visual);
        visual.simulate_keystrokes("secondary-shift-l");
        settle(visual);
        assert!(visual.debug_bounds("error-log-pane").is_some(), "{place:?}: the shortcut opens it");
        visual.simulate_keystrokes("secondary-shift-l");
        settle(visual);
        assert!(visual.debug_bounds("error-log-pane").is_none(), "{place:?}: and closes it");
    }
}

#[gpui::test]
fn in_a_tab_the_toggle_brings_the_tab_forward_and_closing_the_tab_closes_the_log(cx: &mut TestAppContext) {
    let (app, visual, query) = query_tab_with_log(cx);
    pick_error_log_place(visual, "tab");
    let tab = active_tab(&app, visual);
    activate_tab_by_click(visual, query);
    assert!(visual.debug_bounds("error-log-pane").is_none(), "the query tab does not show the log");
    // From another tab the status-bar icon shows the Error Log tab instead of closing the log.
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    assert_eq!(active_tab(&app, visual), tab, "the Error Log tab came forward");
    assert!(visual.debug_bounds("error-log-pane").is_some());
    app.read_with(visual, |this, _| assert!(this.error_log.open));
    // Closing the tab closes the Error Log (the place stays).
    app.update(visual, |this, cx| this.close_tab(tab, cx));
    settle(visual);
    app.read_with(visual, |this, _| {
        assert!(!this.error_log.open && this.error_log.tab.is_none());
        assert_eq!(this.error_log_place(), crate::settings::ErrorLogPlace::Tab);
    });
    assert!(visual.debug_bounds("error-log-pane").is_none());
    // Opening it again shows it in a tab.
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!(tab.kind, TabKind::ErrorLog);
    });
}

#[gpui::test]
fn the_chosen_place_survives_a_restart(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, _query) = query_tab_with_log(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    pick_error_log_place(visual, "right");
    visual.executor().advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let on_disk = crate::settings::store::load(&path, &crate::settings::store::Legacy::default());
    assert_eq!(on_disk.settings.appearance.error_log_place, crate::settings::ErrorLogPlace::Right);
    assert!(std::fs::read_to_string(&path).unwrap().contains("\"error_log_place\": \"right\""));

    // "Relaunch": defaults in memory, the file read back.
    app.update(visual, |this, cx| {
        *this.settings = std::sync::Arc::new(Settings::default());
        this.settings_ui.path = Some(path.clone());
        this.init_settings(cx);
    });
    settle(visual);
    assert_eq!(error_log_place_of(&app, visual), crate::settings::ErrorLogPlace::Right);
    click_debug_button(visual, "error-log-toggle");
    click_debug_button(visual, "error-log-toggle");
    settle(visual);
    let area = visual.debug_bounds("query-editor-area").expect("editor area");
    let pane = visual.debug_bounds("error-log-pane").expect("pane");
    assert!(pane.left() >= area.right() - gpui::px(1.0), "opens on the right after a restart");
    let _ = std::fs::remove_file(path);
}
