// R6: Backup and Restore with the server's native tools, through the real navigator menus, the wizard tab and the real
// process runner, against FAKE tool scripts (a shell script per tool: it answers `--version`, records what it was given, writes the
// output and prints verbose lines). The fixture Postgres connection stands in for the server, so no database is needed.
//
// The live tests at the end (`#[ignore]`d) run the real `pg_dump` / `pg_restore` of /opt/homebrew/opt/postgresql@17 against the
// dedicated `tablex-test-pg` container.

use crate::app::backup::view::ViewSnapshot as BrSnapshot;
use crate::app::backup::Mode as BrMode;
use crate::backup::job::{Operation as BrOperation, Outcome as BrOutcome};

const BR_PASSWORD: &str = "hunter2-pass";

/// A folder of fake tools: `bin/<name>` answers `--version`, records its arguments and environment in `record`, its process id in `pid`,
/// and then runs the body of the test.
struct BrTools {
    dir: tempfile::TempDir,
}

impl BrTools {
    fn new() -> Self {
        let tools = Self { dir: tempfile::tempdir().unwrap() };
        std::fs::create_dir(tools.dir.path().join("bin")).unwrap();
        tools
    }

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join("bin").join(name)
    }

    /// The tool `name` answering `version` for `--version`; `body` runs otherwise, with `$out` the file it is asked to write.
    fn install(&self, name: &str, version: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = "$(stat -c %a \"$1\" 2>/dev/null || stat -f %Lp \"$1\")";
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in --version) echo '{version}'; exit 0;; esac\n\
             root=\"$(dirname \"$0\")/..\"\n\
             out=''; for a in \"$@\"; do case \"$a\" in --file=*) out=\"${{a#--file=}}\";; --result-file=*) out=\"${{a#--result-file=}}\";; esac; done\n\
             mode() {{ echo \"{mode}\"; }}\n\
             {{ echo \"ARGS=$*\"; echo \"PGPASSFILE=$PGPASSFILE PGSSLMODE=$PGSSLMODE\"; \
                if [ -n \"$PGPASSFILE\" ]; then echo \"PGPASS_CONTENT=$(cat \"$PGPASSFILE\")\"; echo \"PGPASS_MODE=$(mode \"$PGPASSFILE\")\"; fi; \
                case \"$1\" in --defaults-file=*) f=\"${{1#--defaults-file=}}\"; echo \"DEFAULTS=$f\"; echo \"DEFAULTS_CONTENT=$(cat \"$f\")\"; echo \"DEFAULTS_MODE=$(mode \"$f\")\";; esac; }} > \"$root/record\"\n\
             echo $$ > \"$root/pid\"\n\
             wait_for_go() {{ while [ ! -f \"$root/go\" ]; do sleep 0.05; done; }}\n\
             {body}\n"
        );
        let path = self.path(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn record(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("record")).unwrap_or_default()
    }

    /// Lets a tool that waits for it go on.
    fn go(&self) {
        std::fs::write(self.dir.path().join("go"), "").unwrap();
    }

    fn pid(&self) -> Option<i32> {
        std::fs::read_to_string(self.dir.path().join("pid")).ok()?.trim().parse().ok()
    }
}

fn br_alive(pid: i32) -> bool {
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// pg_dump that prints a verbose line, waits to be let go, writes the output and ends well.
const BR_DUMP: &str = "echo 'pg_dump: dumping contents of table \"public.orders\"' >&2\nwait_for_go\nprintf 'dump of orders' > \"$out\"\nexit 0";

fn br_snapshot(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> BrSnapshot {
    app.read_with(visual, |this, cx| this.backups.values().next().expect("a backup tab is open").read(cx).snapshot())
}

fn br_wait(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, label: &'static str, ready: impl Fn(&BrSnapshot) -> bool) {
    let app = app.clone();
    wait_until(visual, label, |visual| ready(&br_snapshot(&app, visual)));
    settle(visual);
}

fn br_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> u64 {
    app.read_with(visual, |this, _| this.workspace.tabs().iter().find(|tab| matches!(tab.kind, TabKind::Backup | TabKind::Restore)).expect("a backup or restore tab").id)
}

/// The names of the first rows' selectors, drawn by the checklist: `backup-item-<position>`.
const BR_ITEM_SELECTORS: [&str; 8] = ["backup-item-0", "backup-item-1", "backup-item-2", "backup-item-3", "backup-item-4", "backup-item-5", "backup-item-6", "backup-item-7"];

fn br_click_item(visual: &mut gpui::VisualTestContext, position: usize) {
    br_click(visual, BR_ITEM_SELECTORS[position]);
}

/// The unfinished file a run left beside its output (`<name>.<id>.tablex-part`), if any.
fn br_partial_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).find(|path| path.to_string_lossy().ends_with(".tablex-part"))
}

fn br_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    click_debug_button(visual, selector);
    settle(visual);
}

/// Opens the select drawn at `selector` and moves `steps` rows down before choosing.
fn br_choose(visual: &mut gpui::VisualTestContext, selector: &'static str, steps: usize) {
    br_click(visual, selector);
    for _ in 0..steps {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
}

/// The Postgres fixture app with the fake tools installed (`pg_dump` 17.4, `pg_restore` 17.4) and set in Settings, the session's own
/// address carrying a password, and a server of version 17.
fn br_pg(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, BrTools) {
    let (app, visual, _, _) = oa_pg(cx);
    let tools = BrTools::new();
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", BR_DUMP);
    tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "echo 'pg_restore: processing data for table \"public.orders\"' >&2\nexit 0");
    br_configure(&app, visual, &tools);
    (app, visual, tools)
}

fn br_configure(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tools: &BrTools) {
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.query.pg_dump_path = tools.path("pg_dump").display().to_string();
        settings.query.pg_restore_path = tools.path("pg_restore").display().to_string();
        settings.query.mysqldump_path = tools.path("mysqldump").display().to_string();
        this.settings = std::sync::Arc::new(settings);
        this.driver_runtime.session_uris.insert(OA_PG.into(), engine_connect::records::RuntimeUri::new(format!("postgres://ann:{BR_PASSWORD}@127.0.0.1:5999/warehouse?sslmode=require")));
        this.driver_runtime.server_versions.insert(OA_PG.into(), 170_400);
        cx.notify();
    });
    settle(visual);
}

fn br_schema() -> NavigatorNodeId {
    NavigatorNodeId::Schema { connection: OA_PG.into(), database: "warehouse".into(), schema: "public".into() }
}

fn br_group() -> NavigatorNodeId {
    fixture_group(ObjectGroup::Tables)
}

/// The Source step left for Settings, and Settings for Run.
fn br_next(visual: &mut gpui::VisualTestContext) {
    br_click(visual, "backup-next");
}

/// Runs the wizard to the end: Run, then (the fake tool waits for it) let it go and wait for the report.
fn br_finish(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tools: &BrTools) -> crate::backup::job::Report {
    br_wait(app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump") || snapshot.tool.starts_with("pg_restore") || snapshot.tool.starts_with("mysqldump"));
    br_click(visual, "backup-start");
    br_wait(app, visual, "the tool to run", |snapshot| snapshot.running || snapshot.report.is_some());
    tools.go();
    br_wait(app, visual, "the run to end", |snapshot| snapshot.report.is_some());
    br_snapshot(app, visual).report.unwrap()
}

// ---- backup ---------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn a_table_row_opens_a_backup_with_that_table_and_the_wizard_runs_the_tool_to_a_summary(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("warehouse.dump");
    oa_menu_on(&app, visual, &oa_pg_object("orders"), "object-menu-backup");

    // The tab, the connection and the table row that was clicked.
    let (kind, title) = app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        (tab.kind, tab.title.clone())
    });
    assert_eq!((kind, title.as_str()), (TabKind::Backup, "Backup warehouse"));
    for selector in ["backup-view", "backup-source-panel", "backup-connection", "backup-namespace", "backup-scope", "backup-next"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    br_wait(&app, visual, "the tables to be listed", |snapshot| !snapshot.listed.is_empty());
    let snapshot = br_snapshot(&app, visual);
    assert_eq!((snapshot.mode, snapshot.step), (BrMode::Backup, "source"));
    assert_eq!(snapshot.listed.iter().filter(|(_, selected)| *selected).map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["orders"], "the clicked table is selected");
    assert!(visual.debug_bounds("backup-list").is_some() && visual.debug_bounds("backup-filter").is_some() && visual.debug_bounds("backup-count").is_some());

    // One more table from the list, by its real checkbox.
    let other = snapshot.listed.iter().position(|(name, _)| name != "orders").expect("another table");
    // The checkbox of that row, by the position it is drawn at (a table list has no row selector worth leaking a string for).
    br_click_item(visual, other);
    assert_eq!(br_snapshot(&app, visual).listed.iter().filter(|(_, selected)| *selected).count(), 2);

    // Settings: the options through the real controls.
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).step, "settings");
    for selector in ["backup-format", "backup-content", "backup-flag-no-owner", "backup-flag-no-privileges", "backup-flag-clean", "backup-choose-output", "backup-output"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("backup-compression").is_none(), "plain SQL has no compression");
    br_choose(visual, "backup-format", 1);
    assert!(visual.debug_bounds("backup-compression").is_some() && visual.debug_bounds("backup-jobs").is_none(), "a custom archive is compressed, not parallel");
    oa_type(visual, "backup-compression", "5");
    br_click(visual, "backup-flag-no-owner");
    // The next step needs a place to write.
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).message.as_deref(), Some("Choose the file to write the backup to."));
    br_click(visual, "backup-choose-output");
    assert!(visual.did_prompt_for_new_path(), "the output file is asked of the platform's save dialog");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).step, "run");
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump 17.4"));
    assert!(visual.debug_bounds("backup-ready").is_some() && visual.debug_bounds("backup-tool").is_some() && visual.debug_bounds("backup-start").is_some());

    // Run: progress, the log, Cancel; then the tool is let go.
    br_click(visual, "backup-start");
    br_wait(&app, visual, "the tool to print its first line", |snapshot| snapshot.running && snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 2));
    for selector in ["backup-progress", "backup-progress-bar", "backup-cancel", "backup-log", "backup-log-0", "backup-log-1"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let log = br_snapshot(&app, visual).log;
    assert!(log[0].starts_with("$ pg_dump ") && log[1].contains("dumping contents of table"), "{log:?}");
    tools.go();
    br_wait(&app, visual, "the run to end", |snapshot| snapshot.report.is_some());
    for selector in ["backup-summary", "backup-headline", "backup-stats", "backup-kept", "backup-command", "backup-again", "backup-close"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let report = br_snapshot(&app, visual).report.unwrap();
    assert_eq!((report.operation, report.outcome.clone()), (BrOperation::Backup, BrOutcome::Completed), "{report:?}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "dump of orders");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1, "no unfinished file beside the backup");

    // What the tool was given: the options the controls hold, the connection, and no password.
    let record = tools.record();
    let args = record.lines().next().unwrap();
    for part in ["--host=127.0.0.1", "--port=5999", "--username=ann", "--no-password", "--dbname=dbname='warehouse'", "--format=c", "--no-owner", "--compress=5", "--verbose", "--table=\"public\".\"orders\""] {
        assert!(args.contains(part), "{part} in {args}");
    }
    assert!(!args.contains(BR_PASSWORD), "{args}");
    assert!(record.contains("PGSSLMODE=require"), "the connection's TLS mode reaches the tool: {record}");
    assert!(record.contains(&format!("PGPASS_CONTENT=127.0.0.1:5999:*:ann:{BR_PASSWORD}")) && record.contains("PGPASS_MODE=600"), "{record}");
    let passfile = record.lines().find_map(|line| line.strip_prefix("PGPASSFILE=")).and_then(|line| line.split(' ').next()).unwrap();
    assert!(!std::path::Path::new(passfile).exists(), "the passfile is gone after the run");
    let log = br_snapshot(&app, visual).log;
    assert!(!log.join("\n").contains(BR_PASSWORD));
    let navigator_notice = app.read_with(visual, |this, _| this.error_log.log.len());
    assert_eq!(navigator_notice, 0, "a good run reports nothing to the Error Log");

    // Close ends the tab; a finished run asks nothing.
    br_click(visual, "backup-close");
    assert!(app.read_with(visual, |this, _| this.backups.is_empty() && this.backup_controls.is_empty()));
}

#[gpui::test]
fn the_tables_group_and_the_schema_open_a_whole_namespace_backup_and_scope_and_filter_work(cx: &mut TestAppContext) {
    let (app, visual, _tools) = br_pg(cx);
    for (node, selector) in [(br_group(), "object-menu-backup"), (br_schema(), "object-menu-backup")] {
        oa_menu_on(&app, visual, &node, selector);
        let snapshot = br_snapshot(&app, visual);
        assert_eq!((snapshot.target.database.as_str(), snapshot.target.schema.as_str()), ("warehouse", "public"));
        assert!(snapshot.listed.is_empty(), "the whole database is the default scope: nothing to choose");
        assert!(visual.debug_bounds("backup-scope-note").is_some());
        let tab = br_tab(&app, visual);
        app.update(visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
        settle(visual);
    }
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    // Schemas: this fixture lists one.
    br_choose(visual, "backup-scope", 1);
    let snapshot = br_snapshot(&app, visual);
    assert_eq!(snapshot.listed, [("public".to_owned(), false), ("analytics".to_owned(), false)], "the schemas of the database the navigator lists");
    // Tables: the filter narrows what is shown, Select all selects what is shown only.
    br_choose(visual, "backup-scope", 1);
    br_wait(&app, visual, "the tables", |snapshot| snapshot.listed.len() > 5);
    let all = br_snapshot(&app, visual).listed.len();
    oa_type(visual, "backup-filter", "order");
    br_click(visual, "backup-select-all");
    let selected: Vec<String> = br_snapshot(&app, visual).listed.into_iter().filter(|(_, selected)| *selected).map(|(name, _)| name).collect();
    assert!(!selected.is_empty() && selected.len() < all && selected.iter().all(|name| name.contains("order")), "{selected:?} of {all}");
    br_click(visual, "backup-select-none");
    assert!(br_snapshot(&app, visual).listed.iter().all(|(_, selected)| !selected));
    // Nothing selected: Next says so instead of backing up nothing.
    oa_type(visual, "backup-filter", "");
    br_next(visual);
    let snapshot = br_snapshot(&app, visual);
    assert_eq!(snapshot.step, "source");
    assert_eq!(snapshot.message.as_deref(), Some("Select at least one schema or table, or back up the whole database."));
}

#[gpui::test]
fn cancelling_a_backup_stops_the_tool_and_removes_the_unfinished_file(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho started >&2\nsleep 60");
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("warehouse.sql");
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_click(visual, "backup-choose-output");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump"));
    br_click(visual, "backup-start");
    br_wait(&app, visual, "the tool to start", |snapshot| snapshot.running && snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 2));
    wait_until(visual, "the unfinished file", |_| br_partial_in(out.path()).is_some());
    let pid = tools.pid().expect("the tool recorded its process id");
    assert!(br_alive(pid));
    br_click(visual, "backup-cancel");
    br_wait(&app, visual, "the cancelled run to end", |snapshot| snapshot.report.is_some());
    let report = br_snapshot(&app, visual).report.unwrap();
    assert_eq!(report.outcome, BrOutcome::Cancelled, "{report:?}");
    assert!(br_partial_in(out.path()).is_none() && !output.exists(), "nothing of the backup is left");
    wait_until(visual, "the tool to be gone", |_| !br_alive(pid));
    assert!(visual.debug_bounds("backup-headline").is_some());
    let notice = app.read_with(visual, |this, _| this.error_log.log.len());
    assert_eq!(notice, 1, "a cancelled run is a warning in the Error Log");
}

#[gpui::test]
fn closing_a_running_backup_asks_first_and_then_cancels_it(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho started >&2\nsleep 60");
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("warehouse.sql");
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_click(visual, "backup-choose-output");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump"));
    br_click(visual, "backup-start");
    br_wait(&app, visual, "the tool to start", |snapshot| snapshot.running && snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 2));
    let pid = tools.pid().unwrap();
    let tab = br_tab(&app, visual);
    assert!(app.read_with(visual, |this, cx| this.backup_running(tab, cx)));

    visual.update(|window, cx| app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(visual);
    assert!(visual.debug_bounds("discard-pending-body").is_some(), "closing a running backup asks");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).is_some()) && br_alive(pid), "kept, and still running");

    visual.update(|window, cx| app.update(cx, |this, cx| this.confirm_close_tab(tab, window, cx)));
    settle(visual);
    br_click(visual, "discard-pending-confirm");
    assert!(app.read_with(visual, |this, _| this.workspace.tab(tab).is_none() && this.backups.is_empty() && this.backup_controls.is_empty()));
    wait_until(visual, "the tool to be stopped by the close", |_| !br_alive(pid));
    wait_until(visual, "the unfinished file to be removed", |_| br_partial_in(out.path()).is_none());
}

#[gpui::test]
fn a_tool_that_fails_shows_its_error_and_leaves_the_older_backup(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho 'pg_dump: error: connection to server failed: password authentication failed for user \"ann\"' >&2\nexit 1");
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("warehouse.sql");
    std::fs::write(&output, "older backup").unwrap();
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_click(visual, "backup-choose-output");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    let report = br_finish(&app, visual, &tools);
    let BrOutcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
    assert!(message.contains("exit code 1") && message.contains("password authentication failed"), "{message}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "older backup");
    assert_eq!(std::fs::read_dir(out.path()).unwrap().count(), 1);
    let logged = app.read_with(visual, |this, _| this.error_log.log.len());
    assert_eq!(logged, 1, "a failure is in the Error Log");
}

#[gpui::test]
fn a_pg_dump_older_than_the_server_or_missing_is_told_before_anything_runs(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_dump", "pg_dump (PostgreSQL) 15.6", BR_DUMP);
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("w.sql");
    br_click(visual, "backup-choose-output");
    visual.simulate_new_path_selection(move |_| Some(output.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    br_wait(&app, visual, "the tool check", |snapshot| snapshot.tool.starts_with("failed"));
    let snapshot = br_snapshot(&app, visual);
    assert!(snapshot.tool.contains("15.6") && snapshot.tool.contains("older than the server") && snapshot.tool.contains("PostgreSQL 17"), "{}", snapshot.tool);
    // Run is not offered, and pressing Enter-style Next does not start it either.
    assert!(!snapshot.running);
    let bounds = visual.debug_bounds("backup-start").expect("Run is drawn, disabled");
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
    assert!(!br_snapshot(&app, visual).running && tools.record().is_empty(), "the tool never ran");

    // A path in Settings that leads nowhere is told, not skipped.
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.query.pg_dump_path = "/nonexistent/pg_dump".into();
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
    br_click(visual, "backup-back");
    br_next(visual);
    br_wait(&app, visual, "the tool check", |snapshot| snapshot.tool.starts_with("failed"));
    assert!(br_snapshot(&app, visual).tool.contains("set in Settings"), "{}", br_snapshot(&app, visual).tool);
}

#[gpui::test]
fn a_directory_backup_with_jobs_and_a_mysql_backup_pass_their_options(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "mkdir \"$out\"\nprintf 'toc' > \"$out/toc.dat\"\nwait_for_go\nexit 0");
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("warehouse-dir");
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_choose(visual, "backup-format", 2);
    assert!(visual.debug_bounds("backup-jobs").is_some() && visual.debug_bounds("backup-compression").is_some());
    oa_type(visual, "backup-jobs", "4");
    br_click(visual, "backup-choose-output");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    let report = br_finish(&app, visual, &tools);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(output.join("toc.dat").is_file() && br_partial_in(out.path()).is_none());
    let args = tools.record();
    assert!(args.lines().next().unwrap().contains("--format=d") && args.contains("--jobs=4"), "{args}");

    // The options that make no sense together are refused with the reason.
    br_click(visual, "backup-close");
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_choose(visual, "backup-format", 1);
    oa_type(visual, "backup-compression", "12");
    br_click(visual, "backup-choose-output");
    let chosen = out.path().join("second.dump");
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).message.as_deref(), Some("The compression level is 0 to 9, not 12."));
    oa_type(visual, "backup-compression", "x");
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).message.as_deref(), Some("The compression level must be a whole number"));
}

/// "Backup…" / "Restore…" of the connection's own menu.
fn br_open_connection(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, mode: BrMode, key: &str) {
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_backup_for_connection(mode, key, window, cx)));
    settle(visual);
}

fn br_open_namespace(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, mode: BrMode, key: &str, database: &str) {
    let target = crate::app::data_transfer::Namespace { connection: key.to_owned(), database: database.to_owned(), schema: String::new() };
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_backup(crate::app::backup::BackupInit { mode, target, tables: Vec::new(), archive: None }, window, cx)));
    settle(visual);
}

fn br_add_mysql(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, kind: ConnectionKind, tools: &BrTools) -> String {
    let id = uuid::Uuid::new_v4();
    let key = id.to_string();
    let (scheme, name) = if kind == ConnectionKind::Mariadb { ("mariadb", "MariaDB fixture") } else { ("mysql", "MySQL fixture") };
    let key_for_app = key.clone();
    app.update(visual, |this, cx| {
        this.saved_connections.push(SavedConnection { id, name: name.into(), uri: format!("{scheme}://root@127.0.0.1:3306/shop"), kind, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false });
        this.driver_runtime.session_uris.insert(key_for_app, engine_connect::records::RuntimeUri::new(format!("{scheme}://root:{BR_PASSWORD}@127.0.0.1:3306/shop?ssl-mode=REQUIRED&ssl-ca=/etc/ca.pem")));
        let mut settings = (*this.settings).clone();
        settings.query.mysqldump_path = tools.path("mysqldump").display().to_string();
        settings.query.mariadb_dump_path = tools.path("mariadb-dump").display().to_string();
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
    settle(visual);
    key
}

#[gpui::test]
fn a_connection_whose_databases_are_not_listed_yet_backs_up_the_one_its_address_names(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let key = br_add_mysql(&app, visual, ConnectionKind::Mariadb, &tools);
    br_open_connection(&app, visual, BrMode::Backup, &key);
    let snapshot = br_snapshot(&app, visual);
    assert_eq!((snapshot.target.connection.as_str(), snapshot.target.database.as_str()), (key.as_str(), "shop"), "the database of the saved address");
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().title.clone()), "Backup shop");
    // MariaDB: no schemas to choose, tables or everything.
    br_choose(visual, "backup-scope", 1);
    assert!(visual.debug_bounds("backup-scope").is_some());
}

#[gpui::test]
fn a_mysql_backup_runs_mysqldump_with_the_option_file_first_and_the_password_only_in_it(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("mysqldump", "mysqldump  Ver 8.0.36 for Linux on x86_64", "echo '-- Retrieving table structure for table orders...' >&2\nwait_for_go\nprintf -- '-- dump' > \"$out\"\nexit 0");
    let key = br_add_mysql(&app, visual, ConnectionKind::Mysql, &tools);
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("shop.sql");
    br_open_namespace(&app, visual, BrMode::Backup, &key, "shop");
    let snapshot = br_snapshot(&app, visual);
    assert_eq!((snapshot.target.connection.as_str(), snapshot.target.database.as_str()), (key.as_str(), "shop"));
    assert!(snapshot.connections.iter().any(|name| name == "MySQL fixture"), "{:?}", snapshot.connections);
    // MySQL: the tables or the whole database; its options.
    br_next(visual);
    assert!(visual.debug_bounds("backup-flag-single-transaction").is_some());
    for selector in ["backup-flag-routines", "backup-flag-triggers", "backup-flag-events", "backup-flag-no-data", "backup-flag-add-drop-table", "backup-flag-extended-insert", "backup-flag-create-database", "backup-flag-add-drop-database"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("backup-format").is_none(), "a MySQL dump is an SQL script");
    br_click(visual, "backup-flag-no-data");
    br_click(visual, "backup-flag-extended-insert");
    br_click(visual, "backup-choose-output");
    let chosen = output.clone();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    let report = br_finish(&app, visual, &tools);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert_eq!(std::fs::read_to_string(&output).unwrap(), "-- dump");
    let record = tools.record();
    let args = record.lines().next().unwrap();
    assert!(args.starts_with("ARGS=--defaults-file="), "the option file is the first argument: {args}");
    for part in ["--single-transaction", "--routines", "--events", "--no-data", "--skip-extended-insert", "--hex-blob", "--databases -- shop"] {
        assert!(args.contains(part), "{part} in {args}");
    }
    assert!(!args.contains(BR_PASSWORD) && !args.contains("root"), "{args}");
    for line in ["host=\"127.0.0.1\"", "user=\"root\"", &format!("password=\"{BR_PASSWORD}\""), "ssl-mode=REQUIRED", "ssl-ca=\"/etc/ca.pem\"", "DEFAULTS_MODE=600"] {
        assert!(record.contains(line), "{line} in {record}");
    }
    let defaults = record.lines().find_map(|line| line.strip_prefix("DEFAULTS=")).unwrap();
    assert!(!std::path::Path::new(defaults).exists(), "the option file is gone after the run");
}

// ---- gates ----------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn a_read_only_connection_backs_up_but_refuses_a_restore_and_a_production_one_asks_before_restoring(cx: &mut TestAppContext) {
    use crate::settings::connection::PRODUCTION;
    let (app, visual, tools) = br_pg(cx);
    let archive_dir = tempfile::tempdir().unwrap();
    let archive = archive_dir.path().join("a.dump");
    std::fs::write(&archive, b"PGDMP\x01\x0e").unwrap();
    tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "case \"$*\" in *--list*) echo '218; 1259 16385 TABLE public orders tablex'; exit 0;; esac\nwait_for_go\nexit 0");

    // Read-only: a backup is a read and opens; a restore is refused with the reason, no tab.
    publish_types(&app, visual, true);
    give_profile(&app, visual, OA_PG, typed(PRODUCTION));
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    assert!(app.read_with(visual, |this, _| this.backups.len() == 1), "a read-only connection can be backed up");
    let tab = br_tab(&app, visual);
    app.update(visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(visual);
    oa_menu_on(&app, visual, &br_group(), "object-menu-restore");
    assert!(!visual.did_prompt_for_paths(), "nothing is asked of a read-only connection");
    let notice = app.read_with(visual, |this, _| this.sidebar.notice.clone()).expect("the refusal is shown");
    assert!(notice.contains("Restoring a backup is") && notice.contains("read-only"), "{notice}");
    assert!(app.read_with(visual, |this, _| this.backups.is_empty()));

    // Production, writable: the restore opens, Restore asks first.
    publish_types(&app, visual, false);
    oa_menu_on(&app, visual, &br_group(), "object-menu-restore");
    assert!(visual.did_prompt_for_paths(), "Restore asks the platform for the backup");
    let chosen = archive.clone();
    visual.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
    settle(visual);
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "custom archive"));
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries == 1);
    br_next(visual);
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_restore"));
    br_click(visual, "backup-start");
    assert!(visual.debug_bounds("backup-confirm").is_some(), "Production asks for its confirmation");
    assert!(!br_snapshot(&app, visual).running);
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(br_snapshot(&app, visual).report.is_none() && !tools.record().contains("--dbname"), "declined: nothing ran");

    // The connection turns read-only while the dialog was open: the answer is checked again at the moment of the run.
    br_click(visual, "backup-start");
    publish_types(&app, visual, true);
    visual.simulate_keystrokes("enter");
    settle(visual);
    let message = br_snapshot(&app, visual).message.expect("the refusal is shown in the wizard");
    assert!(message.contains("read-only"), "{message}");
    assert!(!br_snapshot(&app, visual).running);

    // Writable again: confirmed, it runs.
    publish_types(&app, visual, false);
    br_click(visual, "backup-start");
    visual.simulate_keystrokes("enter");
    settle(visual);
    br_wait(&app, visual, "the restore to run", |snapshot| snapshot.running);
    tools.go();
    br_wait(&app, visual, "the restore to end", |snapshot| snapshot.report.is_some());
    let report = br_snapshot(&app, visual).report.unwrap();
    assert_eq!((report.operation, report.outcome.clone()), (BrOperation::Restore, BrOutcome::Completed), "{report:?}");
}

// ---- restore --------------------------------------------------------------------------------------------------------------

#[gpui::test]
fn a_restore_lists_the_contents_of_the_archive_and_runs_pg_restore_with_the_selection(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install(
        "pg_restore",
        "pg_restore (PostgreSQL) 17.4",
        "case \"$*\" in *--list*) printf '; header\\n10; 2615 2200 SCHEMA - public tablex\\n218; 1259 16385 TABLE public orders tablex\\n3352; 0 16385 TABLE DATA public orders tablex\\n99; 1259 16399 TABLE public items tablex\\n'; exit 0;; esac\n\
         for a in \"$@\"; do case \"$a\" in --use-list=*) cat \"${a#--use-list=}\" > \"$root/list\";; esac; done\n\
         echo 'pg_restore: creating TABLE \"public.orders\"' >&2\nwait_for_go\nexit 0",
    );
    let archive_dir = tempfile::tempdir().unwrap();
    let archive = archive_dir.path().join("warehouse.dump");
    std::fs::write(&archive, b"PGDMP\x01\x0e").unwrap();
    oa_menu_on(&app, visual, &br_group(), "object-menu-restore");
    let chosen = archive.clone();
    visual.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.directories && !options.multiple);
        Some(vec![chosen.clone()])
    });
    settle(visual);
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "custom archive"));
    let tab_kind = app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().kind);
    assert_eq!(tab_kind, TabKind::Restore);
    assert!(visual.debug_bounds("backup-choose-file").is_some() && visual.debug_bounds("backup-choose-folder").is_some() && visual.debug_bounds("backup-archive").is_some());

    // Settings: the contents of the archive, every object selected; clear one.
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries == 4);
    for selector in ["backup-toc", "backup-list", "backup-flag-restore-clean", "backup-flag-restore-no-owner", "backup-flag-restore-single-transaction", "backup-jobs"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    let listed = br_snapshot(&app, visual).listed;
    assert_eq!(listed.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["SCHEMA public", "TABLE public.orders", "TABLE DATA public.orders", "TABLE public.items"]);
    assert!(listed.iter().all(|(_, selected)| *selected));
    oa_type(visual, "backup-filter", "items");
    br_click(visual, "backup-select-none");
    oa_type(visual, "backup-filter", "");
    assert_eq!(br_snapshot(&app, visual).listed.iter().filter(|(_, selected)| *selected).count(), 3);
    br_click(visual, "backup-flag-restore-clean");
    br_click(visual, "backup-flag-restore-no-owner");
    oa_type(visual, "backup-jobs", "3");
    // Jobs and a single transaction exclude each other; the box is disabled while jobs > 1.
    br_click(visual, "backup-flag-restore-single-transaction");
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).step, "run");
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_restore 17.4"));
    br_click(visual, "backup-start");
    br_wait(&app, visual, "the restore to run", |snapshot| snapshot.running && snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 2));
    assert!(visual.debug_bounds("backup-cancel").is_some() && visual.debug_bounds("backup-progress").is_some());
    tools.go();
    br_wait(&app, visual, "the restore to end", |snapshot| snapshot.report.is_some());
    let report = br_snapshot(&app, visual).report.unwrap();
    assert_eq!((report.operation, report.outcome.clone()), (BrOperation::Restore, BrOutcome::Completed), "{report:?}");
    let record = tools.record();
    let args = record.lines().next().unwrap();
    for part in ["--dbname=dbname='warehouse'", "--clean", "--if-exists", "--no-owner", "--jobs=3", "--use-list=", "--verbose", &format!("-- {}", archive.display())] {
        assert!(args.contains(part), "{part} in {args}");
    }
    assert!(!args.contains("--single-transaction"), "{args}");
    assert!(!args.contains(BR_PASSWORD));
    let list = std::fs::read_to_string(tools.dir.path().join("list")).unwrap();
    assert!(list.contains("\n218; 1259") && list.contains("\n3352; 0") && list.contains(";99; 1259"), "{list}");
    assert!(app.read_with(visual, |this, _| this.backup_controls.is_empty()));
    assert!(visual.debug_bounds("backup-summary").is_some() && visual.debug_bounds("backup-again").is_some());
}

#[gpui::test]
fn an_sql_script_is_restored_through_run_sql_file_and_a_mysql_restore_explains_why(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("dump.sql");
    std::fs::write(&script, "CREATE TABLE t(a int);\nINSERT INTO t VALUES (1);\n").unwrap();
    oa_menu_on(&app, visual, &br_group(), "object-menu-restore");
    let chosen = script.clone();
    visual.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
    settle(visual);
    br_wait(&app, visual, "the file to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "SQL script"));
    assert!(visual.debug_bounds("backup-script-note").is_some() && visual.debug_bounds("backup-open-script").is_some(), "a plain script is for Run SQL File");
    let next = visual.debug_bounds("backup-next").is_some();
    assert!(next);
    assert!(!br_snapshot(&app, visual).running);
    // Next is off for a script: pg_restore cannot read it.
    br_click(visual, "backup-next");
    assert_eq!(br_snapshot(&app, visual).step, "source");

    br_click(visual, "backup-open-script");
    settle(visual);
    let (kind, backups) = app.read_with(visual, |this, _| (this.workspace.active_tab().unwrap().kind, this.backups.len()));
    assert_eq!((kind, backups), (TabKind::SqlFile, 0), "the SQL file tab opened and the restore tab is done");
    let file_run = app.read_with(visual, |this, cx| this.file_runs.values().next().expect("the SQL file wizard").read(cx).snapshot());
    assert_eq!(file_run.target.database, "warehouse");
    assert!(file_run.connections.iter().any(|name| name.contains("Postgres")));
    let tab = fr_tab(&app, visual);
    app.update(visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(visual);

    // MySQL: every backup is a script, and the tab says why before a file is even chosen.
    let key = br_add_mysql(&app, visual, ConnectionKind::Mysql, &tools);
    br_open_namespace(&app, visual, BrMode::Restore, &key, "shop");
    assert!(visual.debug_bounds("backup-script-note").is_some(), "the reason is shown");
    assert!(visual.debug_bounds("backup-choose-folder").is_none(), "only PostgreSQL has directory archives");
    assert!(visual.debug_bounds("backup-open-script").is_none(), "no file yet");
}

#[gpui::test]
fn a_tool_missing_or_not_a_backup_is_told_on_the_restore_source(cx: &mut TestAppContext) {
    let (app, visual, _tools) = br_pg(cx);
    let dir = tempfile::tempdir().unwrap();
    let empty_folder = dir.path().join("not-an-archive");
    std::fs::create_dir(&empty_folder).unwrap();
    oa_menu_on(&app, visual, &br_group(), "object-menu-restore");
    let chosen = empty_folder.clone();
    visual.simulate_path_prompt_response(move |_| Some(vec![chosen.clone()]));
    settle(visual);
    br_wait(&app, visual, "the folder to be looked at", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state.starts_with("failed")));
    let (_, state) = br_snapshot(&app, visual).archive.unwrap();
    assert!(state.contains("no toc.dat"), "{state}");
    br_click(visual, "backup-next");
    assert_eq!(br_snapshot(&app, visual).step, "source", "Next stays off");
}

// ---- entry points ---------------------------------------------------------------------------------------------------------

#[gpui::test]
fn the_palette_and_the_file_menu_open_a_backup_and_a_restore_for_the_selected_row(cx: &mut TestAppContext) {
    use crate::app::command_palette::StaticAction;
    let (app, visual, _tools) = br_pg(cx);
    app.update(visual, |this, _| this.navigator.select(br_schema()));
    visual.update(|window, cx| app.update(cx, |this, cx| StaticAction::BackupDatabase.run(this, window, cx)));
    settle(visual);
    let snapshot = br_snapshot(&app, visual);
    assert_eq!((snapshot.mode, snapshot.target.database.as_str(), snapshot.target.schema.as_str()), (BrMode::Backup, "warehouse", "public"));
    let tab = br_tab(&app, visual);
    app.update(visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(visual);
    visual.update(|window, cx| app.update(cx, |this, cx| StaticAction::RestoreDatabase.run(this, window, cx)));
    settle(visual);
    assert!(visual.did_prompt_for_paths());
    assert_eq!(app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().kind), TabKind::Restore);
    // The File menu carries both.
    let menus = crate::app::menus::build();
    let file: Vec<String> = menus[1].items.iter().filter_map(|item| match item {
        gpui::MenuItem::Action { name, .. } => Some(name.to_string()),
        _ => None,
    }).collect();
    assert!(file.contains(&"Backup…".to_owned()) && file.contains(&"Restore…".to_owned()), "{file:?}");
}

#[gpui::test]
fn backup_and_restore_are_offered_on_sql_servers_with_native_tools_only(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let entries = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, id: &NavigatorNodeId| -> Vec<crate::app::object_actions::ObjectMenuItem> { app.read_with(visual, |this, _| this.object_menu_items(id)) };
    use crate::app::object_actions::ObjectMenuItem as Item;
    let group = entries(&app, visual, &br_group());
    assert!(group.contains(&Item::Backup) && group.contains(&Item::Restore));
    let table = entries(&app, visual, &oa_pg_object("orders"));
    assert!(table.contains(&Item::Backup) && !table.contains(&Item::Restore), "a table row backs up that table; a restore is of a database");
    // SQLite has no native tool: its rows do not offer them (and the gate says why when asked another way).
    let sqlite = NavigatorNodeId::Schema { connection: crate::ui_fixture::SQLITE_ID.into(), database: "main".into(), schema: "main".into() };
    assert!(!entries(&app, visual, &sqlite).contains(&Item::Backup));
    let refusal = app.read_with(visual, |this, _| this.backup_gate(crate::ui_fixture::SQLITE_ID, BrMode::Backup).err().map(|error| error.to_string()));
    assert!(refusal.is_some_and(|text| text.contains("SQLite")));
    let _ = tools;
}

// ---- the log and the lists stay bounded and virtualized ----------------------------------------------------------------

/// A backup whose tool printed 6000 lines and waits to be let go (then prints 300 more), the log followed to its end.
fn br_many_lines(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, BrTools, tempfile::TempDir) {
    let (app, visual, tools) = br_pg(cx);
    tools.install(
        "pg_dump",
        "pg_dump (PostgreSQL) 17.4",
        "i=0; while [ $i -lt 6000 ]; do echo \"pg_dump: dumping contents of table t$i\" >&2; i=$((i+1)); done\nprintf 'dump' > \"$out\"\nwait_for_go\ni=0; while [ $i -lt 300 ]; do echo \"pg_dump: late line $i\" >&2; i=$((i+1)); done\nexit 0",
    );
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("big.sql");
    oa_menu_on(&app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_click(visual, "backup-choose-output");
    visual.simulate_new_path_selection(move |_| Some(output.clone()));
    settle(visual);
    br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    br_wait(&app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump"));
    br_click(visual, "backup-start");
    br_wait(&app, visual, "the lines", |snapshot| snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 6000));
    br_wait(&app, visual, "the log to follow its end", |snapshot| snapshot.drawn_log.1 >= crate::backup::process::LOG_CAP - 2);
    (app, visual, tools, out)
}

#[gpui::test]
fn a_tool_that_prints_thousands_of_lines_keeps_a_capped_log_drawn_by_the_rows_on_screen(cx: &mut TestAppContext) {
    let (app, visual, tools, _out) = br_many_lines(cx);
    let progress = br_snapshot(&app, visual).progress.unwrap();
    assert_eq!(progress.log_len, crate::backup::process::LOG_CAP, "the log keeps at most {} lines", crate::backup::process::LOG_CAP);
    assert!(progress.dropped >= 4000, "{progress:?}");
    // Only the rows on screen are drawn, and they are the last ones: the log follows its end.
    assert!(visual.debug_bounds("backup-log").is_some());
    let (start, end) = br_snapshot(&app, visual).drawn_log;
    assert!(end > start && end - start < 80, "{} log rows are drawn of {}", end - start, crate::backup::process::LOG_CAP);
    assert!(end >= crate::backup::process::LOG_CAP - 2 && start > crate::backup::process::LOG_CAP - 100, "rows {start}..{end} are drawn: the end of the log");
    assert!(br_snapshot(&app, visual).log_pinned, "the log is at its end");

    // The user scrolls up to read, with the wheel: the lines that come now do not pull the log away.
    let log = visual.debug_bounds("backup-log").unwrap();
    visual.simulate_event(gpui::ScrollEvent { position: log.center(), delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(4000.0))), ..Default::default() });
    settle(visual);
    assert!(!br_snapshot(&app, visual).log_pinned, "the wheel moved the log off its end");
    let top = br_snapshot(&app, visual).log_top;
    tools.go();
    br_wait(&app, visual, "the run to end", |snapshot| snapshot.report.is_some());
    let snapshot = br_snapshot(&app, visual);
    assert!(snapshot.log.last().is_some_and(|line| line.contains("late line 299")), "the late lines arrived");
    assert!(snapshot.drawn_log.1 < crate::backup::process::LOG_CAP - 100 && snapshot.log_top.abs_diff(top) <= 2, "the log stayed where it was read: top {top}, now {} and rows {:?}", snapshot.log_top, snapshot.drawn_log);
    assert!(visual.debug_bounds("backup-log-title").is_some());
}

#[gpui::test]
fn a_log_moved_off_its_end_by_anything_but_the_wheel_is_left_where_it_is_too(cx: &mut TestAppContext) {
    let (app, visual, tools, _out) = br_many_lines(cx);
    // What a scrollbar drag or a key does: the scroll position is set on the handle itself.
    app.read_with(visual, |this, cx| {
        let view = this.backups.values().next().unwrap().read(cx);
        view.log_scroll.0.borrow().base_handle.set_offset(gpui::point(gpui::px(0.0), gpui::px(-18.0 * 700.0)));
    });
    settle(visual);
    assert!(!br_snapshot(&app, visual).log_pinned);
    let start = br_snapshot(&app, visual).drawn_log.0;
    assert!((690..710).contains(&start), "rows {start}.. are drawn");
    tools.go();
    br_wait(&app, visual, "the run to end", |snapshot| snapshot.report.is_some());
    let snapshot = br_snapshot(&app, visual);
    assert!(snapshot.log.last().is_some_and(|line| line.contains("late line 299")));
    assert!(snapshot.drawn_log.0.abs_diff(start) <= 2 && snapshot.drawn_log.1 < crate::backup::process::LOG_CAP - 100, "the log stayed where it was set: from row {start}, now {:?}", snapshot.drawn_log);
}

#[gpui::test]
fn the_contents_of_a_large_archive_are_a_virtualized_list_with_a_filter(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install(
        "pg_restore",
        "pg_restore (PostgreSQL) 17.4",
        "case \"$*\" in *--list*) i=1; while [ $i -le 5000 ]; do echo \"$i; 1259 $i TABLE public table_$i tablex\"; i=$((i+1)); done; exit 0;; esac\nexit 0",
    );
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("big.dump");
    std::fs::write(&archive, b"PGDMP\x01\x0e").unwrap();
    let init = crate::app::backup::BackupInit {
        mode: BrMode::Restore,
        target: crate::app::data_transfer::Namespace { connection: OA_PG.into(), database: "warehouse".into(), schema: "public".into() },
        tables: Vec::new(),
        archive: Some(archive),
    };
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_backup(init, window, cx)));
    settle(visual);
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "custom archive"));
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries == 5000);
    assert!(visual.debug_bounds("backup-item-0").is_some());
    assert!(visual.debug_bounds("backup-item-4000").is_none(), "rows off screen are not drawn");
    let (start, end) = br_snapshot(&app, visual).drawn_list;
    assert!(start == 0 && end > 0 && end < 60, "rows {start}..{end} are drawn of 5000");
    oa_type(visual, "backup-filter", "table_4999");
    assert_eq!(app.read_with(visual, |this, cx| this.backups.values().next().unwrap().read(cx).shown_for_test()), 1);
    br_click(visual, "backup-select-none");
    oa_type(visual, "backup-filter", "");
    assert_eq!(br_snapshot(&app, visual).listed.iter().filter(|(_, selected)| *selected).count(), 4999);
}

// ---- settings -------------------------------------------------------------------------------------------------------------

#[test]
fn the_tool_paths_are_settings_that_persist_and_reach_the_search() {
    let mut settings = crate::settings::Settings::default();
    assert_eq!(settings.query.tool_paths(), crate::backup::tools::ToolPaths::default());
    let registry = crate::settings::registry::registry();
    for (id, title) in [("query.pg_dump_path", "pg_dump"), ("query.pg_restore_path", "pg_restore"), ("query.mysqldump_path", "mysqldump"), ("query.mariadb_dump_path", "mariadb-dump")] {
        let descriptor = registry.iter().find(|descriptor| descriptor.id == id).unwrap_or_else(|| panic!("{id} is a setting"));
        assert_eq!(descriptor.title, title);
        assert_eq!(descriptor.section, crate::settings::registry::Section::Query);
    }
    settings.query.pg_dump_path = "/opt/pg/bin/pg_dump".into();
    settings.query.mariadb_dump_path = "  /opt/maria/bin/mariadb-dump ".into();
    let json = serde_json::to_string(&settings).unwrap();
    let back: crate::settings::Settings = serde_json::from_str(&json).unwrap();
    let paths = back.query.tool_paths();
    assert_eq!((paths.get(crate::backup::tools::Tool::PgDump), paths.get(crate::backup::tools::Tool::MariadbDump), paths.get(crate::backup::tools::Tool::PgRestore)), (Some("/opt/pg/bin/pg_dump"), Some("/opt/maria/bin/mariadb-dump"), None));
    // A file saved before these settings existed loads with them empty.
    let old: crate::settings::Settings = serde_json::from_str("{\"query\": {\"query_timeout_secs\": 5}}").unwrap();
    assert_eq!(old.query.tool_paths(), crate::backup::tools::ToolPaths::default());
}

// ---- live: PostgreSQL (the real pg_dump and pg_restore of PostgreSQL 17) ---------------------------------------------------
//
//   TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex \
//     cargo test -p tablex-gpui live_r6 -- --ignored --test-threads=1
//
// Everything created is named `cf_r6_*`: databases made and dropped through psql (separate from the app's sessions), so a failed test
// leaves nothing either.

const BR_PG_BIN: &str = "/opt/homebrew/opt/postgresql@17/bin";

/// The test server: its address as the tools take it, and psql to make, fill, read and drop what the tests use.
struct BrLivePg {
    uri: String,
    endpoint: crate::backup::endpoint::Endpoint,
    made: std::cell::RefCell<Vec<String>>,
}

impl BrLivePg {
    fn from_env() -> Option<Self> {
        let Ok(uri) = std::env::var("TABLEX_TEST_PG_URI") else {
            eprintln!("skipped: set TABLEX_TEST_PG_URI to the dedicated tablex-test-pg container");
            return None;
        };
        if !std::path::Path::new(BR_PG_BIN).join("pg_dump").exists() {
            eprintln!("skipped: no PostgreSQL 17 client tools in {BR_PG_BIN}");
            return None;
        }
        let endpoint = crate::backup::endpoint::Endpoint::parse(ConnectionKind::Postgres, &uri).expect("a PostgreSQL URI");
        Some(Self { uri, endpoint, made: Default::default() })
    }

    fn psql(&self, database: &str, sql: &str) -> String {
        let output = std::process::Command::new(format!("{BR_PG_BIN}/psql"))
            .env("PGPASSWORD", self.endpoint.password_text().unwrap_or_default())
            .args(["-X", "-A", "-t", "-v", "ON_ERROR_STOP=1", "-h", &self.endpoint.host, "-p", &self.endpoint.port.unwrap_or(5432).to_string(), "-U", self.endpoint.user.as_deref().unwrap_or("postgres"), "-d", &crate::backup::command::dbname_argument(database), "-c", sql])
            .output()
            .expect("psql runs");
        assert!(output.status.success(), "psql: {sql}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn admin(&self, sql: &str) -> String {
        self.psql(&self.endpoint.database, sql)
    }

    /// A new, empty database `cf_r6_<role>_<suffix>`.
    fn create(&self, role: &str) -> String {
        self.create_named(&format!("cf_r6_{role}_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]))
    }

    /// A database of exactly this name (it may hold `=` or spaces).
    fn create_named(&self, name: &str) -> String {
        self.admin(&format!("CREATE DATABASE \"{name}\""));
        self.made.borrow_mut().push(name.to_owned());
        name.to_owned()
    }

    /// A database with a schema of every type the dump has to carry, and its fingerprint.
    fn source(&self) -> (String, String) {
        let name = self.create("src");
        self.psql(
            &name,
            "CREATE SCHEMA cf_r6_s;\n\
             CREATE TYPE cf_r6_s.mood AS ENUM ('sad','ok','happy');\n\
             CREATE TABLE cf_r6_s.items (id serial PRIMARY KEY, name text NOT NULL, price numeric(12,2), at timestamptz, doc jsonb, blob bytea, tags text[], uid uuid, mood cf_r6_s.mood, wide varchar(40) DEFAULT 'x');\n\
             INSERT INTO cf_r6_s.items(name, price, at, doc, blob, tags, uid, mood) SELECT 'item ' || g || ' \u{00E9}\u{4E2D}', g * 1.25, timestamptz '2026-01-01 00:00:00+00' + g * interval '1 hour', jsonb_build_object('n', g, 's', 'x\"y'), decode(lpad(to_hex(g), 4, '0'), 'hex'), ARRAY['a', 'b,c', g::text], ('00000000-0000-4000-8000-' || lpad(g::text, 12, '0'))::uuid, (ARRAY['sad','ok','happy'])[1 + g % 3]::cf_r6_s.mood FROM generate_series(1, 500) g;\n\
             CREATE TABLE cf_r6_s.notes (id int PRIMARY KEY, body text);\n\
             INSERT INTO cf_r6_s.notes VALUES (1, 'one'), (2, 'two'), (3, NULL);\n\
             CREATE VIEW cf_r6_s.cheap AS SELECT id FROM cf_r6_s.items WHERE price < 5;\n\
             CREATE FUNCTION cf_r6_s.twice(a int) RETURNS int LANGUAGE sql AS 'select a * 2';",
        );
        let fingerprint = self.fingerprint(&name);
        (name, fingerprint)
    }

    /// Rows, column types and objects of the scratch schema, as one text.
    fn fingerprint(&self, database: &str) -> String {
        self.psql(
            database,
            "SELECT (SELECT md5(string_agg(i::text, '|' ORDER BY id)) FROM cf_r6_s.items i) || ' ' || \
             (SELECT count(*) FROM cf_r6_s.notes) || ' ' || \
             (SELECT string_agg(column_name || ':' || data_type || ':' || udt_name, ',' ORDER BY ordinal_position) FROM information_schema.columns WHERE table_schema = 'cf_r6_s' AND table_name = 'items') || ' ' || \
             (SELECT count(*) FROM cf_r6_s.cheap) || ' ' || cf_r6_s.twice(21)",
        )
    }

    fn tables(&self, database: &str) -> String {
        self.psql(database, "SELECT coalesce(string_agg(table_name, ',' ORDER BY table_name), '') FROM information_schema.tables WHERE table_schema = 'cf_r6_s' AND table_type = 'BASE TABLE'")
    }
}

impl Drop for BrLivePg {
    fn drop(&mut self) {
        // The databases were made through psql, away from the app's sessions: dropping cannot wait on a lock of a failed test's tab.
        for name in self.made.borrow().iter() {
            let _ = std::process::Command::new(format!("{BR_PG_BIN}/psql"))
                .env("PGPASSWORD", self.endpoint.password_text().unwrap_or_default())
                .args(["-X", "-h", &self.endpoint.host, "-p", &self.endpoint.port.unwrap_or(5432).to_string(), "-U", self.endpoint.user.as_deref().unwrap_or("postgres"), "-d", &crate::backup::command::dbname_argument(&self.endpoint.database), "-c", &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)")])
                .output();
        }
    }
}

/// The fixture app pointed at the real server and the real tools of PostgreSQL 17.
fn br_live_app<'a>(cx: &'a mut TestAppContext, pg: &BrLivePg) -> (gpui::Entity<AppRoot>, &'a mut gpui::VisualTestContext) {
    let (app, visual, _, _) = oa_pg(cx);
    let uri = pg.uri.clone();
    app.update(visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.query.pg_dump_path = format!("{BR_PG_BIN}/pg_dump");
        settings.query.pg_restore_path = format!("{BR_PG_BIN}/pg_restore");
        this.settings = std::sync::Arc::new(settings);
        this.driver_runtime.session_uris.insert(OA_PG.into(), engine_connect::records::RuntimeUri::new(uri));
        this.driver_runtime.server_versions.insert(OA_PG.into(), 171_000);
        cx.notify();
    });
    settle(visual);
    (app, visual)
}

fn br_run_to_end(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> crate::backup::job::Report {
    br_wait(app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_") || snapshot.tool.starts_with("mysqldump") || snapshot.tool.starts_with("mariadb-dump") || snapshot.tool.starts_with("failed"));
    br_click(visual, "backup-start");
    br_wait(app, visual, "the run to end", |snapshot| snapshot.report.is_some());
    br_snapshot(app, visual).report.unwrap()
}

fn br_open(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, mode: BrMode, database: &str, archive: Option<std::path::PathBuf>) {
    br_open_on(app, visual, OA_PG, mode, database, archive);
}

fn br_open_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, mode: BrMode, database: &str, archive: Option<std::path::PathBuf>) {
    let target = crate::app::data_transfer::Namespace { connection: connection.into(), database: database.into(), schema: String::new() };
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_backup(crate::app::backup::BackupInit { mode, target, tables: Vec::new(), archive }, window, cx)));
    settle(visual);
}

fn br_close(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    let tab = br_tab(app, visual);
    app.update(visual, |this, cx| this.close_tabs_now(&[tab], None, cx));
    settle(visual);
}

/// A backup of `database` through the wizard: the format (a row of the select), the flags clicked, the jobs typed.
fn br_live_backup(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, database: &str, format_row: usize, flags: &[&'static str], jobs: Option<&str>, output: &std::path::Path) -> crate::backup::job::Report {
    br_open(app, visual, BrMode::Backup, database, None);
    br_next(visual);
    if format_row > 0 {
        br_choose(visual, "backup-format", format_row);
    }
    for flag in flags {
        br_click(visual, flag);
    }
    if let Some(jobs) = jobs {
        oa_type(visual, "backup-jobs", jobs);
    }
    br_click(visual, "backup-choose-output");
    let chosen = output.to_owned();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    let report = br_run_to_end(app, visual);
    br_close(app, visual);
    report
}

/// A restore of `archive` into `database`; `clear` filters the contents by it and clears what shows.
fn br_live_restore(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, database: &str, archive: &std::path::Path, clear: Option<&str>, flags: &[&'static str]) -> crate::backup::job::Report {
    br_live_restore_on(app, visual, OA_PG, database, archive, clear, flags)
}

#[allow(clippy::too_many_arguments)]
fn br_live_restore_on(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, connection: &str, database: &str, archive: &std::path::Path, clear: Option<&str>, flags: &[&'static str]) -> crate::backup::job::Report {
    br_open_on(app, visual, connection, BrMode::Restore, database, Some(archive.to_owned()));
    br_wait(app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state != "detecting"));
    br_next(visual);
    br_wait(app, visual, "the contents", |snapshot| snapshot.toc_entries > 0);
    if let Some(needle) = clear {
        oa_type(visual, "backup-filter", needle);
        br_click(visual, "backup-select-none");
        oa_type(visual, "backup-filter", "");
    }
    for flag in flags {
        br_click(visual, flag);
    }
    br_next(visual);
    let report = br_run_to_end(app, visual);
    br_close(app, visual);
    report
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg) and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_backup_in_every_format_through_the_wizard(cx: &mut TestAppContext) {
    let Some(pg) = BrLivePg::from_env() else { return };
    let (source, _) = pg.source();
    let (app, visual) = br_live_app(cx, &pg);
    let out = tempfile::tempdir().unwrap();

    // Custom: a real archive that pg_restore lists.
    let custom = out.path().join("custom.dump");
    let report = br_live_backup(&app, visual, &source, 1, &[], None, &custom);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(std::fs::read(&custom).unwrap().starts_with(b"PGDMP"));
    assert!(report.tool.as_deref().is_some_and(|tool| tool.starts_with("pg_dump 17.")), "{report:?}");
    assert!(report.bytes > 1000 && report.elapsed.as_secs() < 60);
    let paths = crate::backup::tools::ToolPaths { pg_restore: format!("{BR_PG_BIN}/pg_restore"), ..Default::default() };
    let toc = crate::backup::job::list_toc(&custom, &paths, &crate::backup::tools::Locator::system(), Some(17), &std::sync::Arc::new(crate::backup::process::CancelToken::default())).unwrap();
    for needle in ["TABLE cf_r6_s.items", "TABLE DATA cf_r6_s.items", "TABLE cf_r6_s.notes", "SCHEMA cf_r6_s", "VIEW cf_r6_s.cheap"] {
        assert!(toc.entries.iter().any(|entry| entry.label().contains(needle)), "{needle} in the contents of the archive");
    }

    // Plain: SQL text; clean and create when asked; never the password.
    let plain = out.path().join("plain.sql");
    let report = br_live_backup(&app, visual, &source, 0, &[], None, &plain);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let text = std::fs::read_to_string(&plain).unwrap();
    assert!(text.contains("CREATE TABLE cf_r6_s.items") && text.contains("COPY cf_r6_s.items") && !text.contains("DROP TABLE") && !text.contains("CREATE DATABASE"), "{}", &text[..text.len().min(600)]);
    let cleaned = out.path().join("clean.sql");
    let report = br_live_backup(&app, visual, &source, 0, &["backup-flag-clean", "backup-flag-no-owner", "backup-flag-no-privileges"], None, &cleaned);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let text = std::fs::read_to_string(&cleaned).unwrap();
    assert!(text.contains("DROP TABLE IF EXISTS cf_r6_s.items") && !text.contains("CREATE DATABASE") && !text.contains("OWNER TO"), "{}", &text[..text.len().min(600)]);
    let created = out.path().join("create.sql");
    let report = br_live_backup(&app, visual, &source, 0, &["backup-flag-create"], None, &created);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(std::fs::read_to_string(&created).unwrap().contains("CREATE DATABASE cf_r6_src_"));

    // Tar, and a directory written by parallel jobs.
    let tar = out.path().join("t.tar");
    let report = br_live_backup(&app, visual, &source, 3, &[], None, &tar);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert_eq!(crate::backup::toc::detect_archive(&tar).unwrap(), crate::backup::toc::ArchiveKind::Tar);
    let directory = out.path().join("dir-backup");
    let report = br_live_backup(&app, visual, &source, 2, &[], Some("3"), &directory);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(directory.join("toc.dat").is_file() && std::fs::read_dir(&directory).unwrap().count() >= 3);
    assert_eq!(crate::backup::toc::detect_archive(&directory).unwrap(), crate::backup::toc::ArchiveKind::Directory);
    assert!(std::fs::read_dir(out.path()).unwrap().flatten().all(|entry| !entry.file_name().to_string_lossy().ends_with(".tablex-part")), "no unfinished file is left");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg) and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_round_trip_restores_rows_and_types_with_a_selection_from_the_contents(cx: &mut TestAppContext) {
    let Some(pg) = BrLivePg::from_env() else { return };
    let (source, fingerprint) = pg.source();
    let (app, visual) = br_live_app(cx, &pg);
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("round.dump");
    assert_eq!(br_live_backup(&app, visual, &source, 1, &[], None, &archive).outcome, BrOutcome::Completed);

    // Everything, into a fresh database: the same rows and the same types.
    let full = pg.create("full");
    let report = br_live_restore(&app, visual, &full, &archive, None, &[]);
    assert_eq!((report.operation, report.outcome.clone()), (BrOperation::Restore, BrOutcome::Completed), "{report:?}");
    assert_eq!(pg.fingerprint(&full), fingerprint, "rows and types survive the round trip");
    assert_eq!(pg.tables(&full), "items,notes");

    // Restoring over what is there fails with the tool's own count of errors, and with the drop option it works.
    let again = br_live_restore(&app, visual, &full, &archive, None, &[]);
    assert!(matches!(&again.outcome, BrOutcome::Failed(message) if message.contains("errors ignored on restore")), "{again:?}");
    let report = br_live_restore(&app, visual, &full, &archive, None, &["backup-flag-restore-clean", "backup-flag-restore-single-transaction"]);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert_eq!(pg.fingerprint(&full), fingerprint);

    // A selection from the contents: everything about `notes` is cleared, so only `items` arrives.
    let part = pg.create("part");
    let report = br_live_restore(&app, visual, &part, &archive, Some("notes"), &["backup-flag-restore-no-owner"]);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert_eq!(pg.tables(&part), "items", "the table that was cleared is not restored");
    assert_eq!(pg.psql(&part, "SELECT count(*) FROM cf_r6_s.items"), "500");
    assert_eq!(pg.psql(&part, "SELECT count(*) FROM cf_r6_s.cheap"), pg.psql(&source, "SELECT count(*) FROM cf_r6_s.cheap"));

    // Parallel jobs from a directory backup.
    let directory = out.path().join("dir-round");
    assert_eq!(br_live_backup(&app, visual, &source, 2, &[], Some("2"), &directory).outcome, BrOutcome::Completed);
    let parallel = pg.create("jobs");
    let (app2, _) = (app.clone(), ());
    br_open(&app2, visual, BrMode::Restore, &parallel, Some(directory.clone()));
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "directory archive"));
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries > 0);
    oa_type(visual, "backup-jobs", "2");
    br_next(visual);
    let report = br_run_to_end(&app, visual);
    br_close(&app, visual);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert_eq!(pg.fingerprint(&parallel), fingerprint);
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg) and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_scopes_a_too_old_tool_and_a_cancel_on_a_blocked_dump() {
    use crate::backup::command::{Content, PgDumpOptions, PgFormat, PgScope};
    use crate::backup::job::{run_backup, BackupJob, DumpOptions, ToolConnection};
    use crate::backup::process::{CancelToken, RunState};
    use crate::backup::tools::{Locator, ToolPaths};
    let Some(pg) = BrLivePg::from_env() else { return };
    let (source, _) = pg.source();
    let out = tempfile::tempdir().unwrap();
    let paths = ToolPaths { pg_dump: format!("{BR_PG_BIN}/pg_dump"), ..Default::default() };
    let connection = || ToolConnection {
        saved: SavedConnection { id: uuid::Uuid::new_v4(), name: "live".into(), uri: pg.uri.clone(), kind: ConnectionKind::Postgres, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false },
        live_uri: Some(engine_connect::records::RuntimeUri::new(pg.uri.clone())),
        tunnel: None,
    };
    let job = |options: PgDumpOptions, output: std::path::PathBuf, server_major: Option<u32>| BackupJob { connection: connection(), database: source.clone(), options: DumpOptions::Postgres(options), output, paths: paths.clone(), server_major, dirs: Default::default() };
    let run = |job: BackupJob| run_backup(job, &Locator::system(), &RunState::new(), &std::sync::Arc::new(CancelToken::default()), &|| {});

    // One table, schema only; one schema, data only.
    let one = out.path().join("one.sql");
    let report = run(job(PgDumpOptions { scope: PgScope::Tables(vec![("cf_r6_s".into(), "notes".into())]), content: Content::SchemaOnly, ..PgDumpOptions::default() }, one.clone(), Some(17)));
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let text = std::fs::read_to_string(&one).unwrap();
    assert!(text.contains("CREATE TABLE cf_r6_s.notes") && !text.contains("cf_r6_s.items") && !text.contains("COPY"), "{text}");
    let data = out.path().join("data.sql");
    let report = run(job(PgDumpOptions { scope: PgScope::Schemas(vec!["cf_r6_s".into()]), content: Content::DataOnly, format: PgFormat::Plain, ..PgDumpOptions::default() }, data.clone(), Some(17)));
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let text = std::fs::read_to_string(&data).unwrap();
    assert!(text.contains("COPY cf_r6_s.items") && text.contains("COPY cf_r6_s.notes") && !text.contains("CREATE TABLE"), "{}", &text[..text.len().min(400)]);

    // The server is "newer" than the only pg_dump: refused with both versions, before a file is touched.
    let refused = out.path().join("refused.sql");
    let report = run(job(PgDumpOptions::default(), refused.clone(), Some(18)));
    let BrOutcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
    assert!(message.contains("older than the server") && message.contains("PostgreSQL 18") && message.contains("17."), "{message}");
    assert!(!refused.exists());

    // A wrong password is the tool's error, with no password in it.
    let mut wrong = job(PgDumpOptions::default(), out.path().join("wrong.sql"), Some(17));
    wrong.connection.live_uri = Some(engine_connect::records::RuntimeUri::new(pg.uri.replace(&format!(":{}@", pg.endpoint.password_text().unwrap()), ":not-the-password@")));
    let report = run(wrong);
    let BrOutcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
    assert!(message.contains("authentication failed") && !message.contains("not-the-password"), "{message}");

    // Cancel while pg_dump waits for a lock another session holds: the real process ends and nothing is left.
    let holder = std::process::Command::new(format!("{BR_PG_BIN}/psql"))
        .env("PGPASSWORD", pg.endpoint.password_text().unwrap())
        .args(["-X", "-h", &pg.endpoint.host, "-p", &pg.endpoint.port.unwrap().to_string(), "-U", pg.endpoint.user.as_deref().unwrap(), "-d", &crate::backup::command::dbname_argument(&source), "-c", "BEGIN; LOCK TABLE cf_r6_s.items IN ACCESS EXCLUSIVE MODE; SELECT pg_sleep(60)"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _holder = Kill(holder);
    while pg.psql(&source, "SELECT count(*) FROM pg_locks WHERE locktype = 'relation' AND mode = 'AccessExclusiveLock' AND relation = 'cf_r6_s.items'::regclass") == "0" {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let blocked = out.path().join("blocked.dump");
    let state = RunState::new();
    let cancel = std::sync::Arc::new(CancelToken::default());
    let worker = {
        let (state, cancel) = (state.clone(), cancel.clone());
        let job = job(PgDumpOptions { format: PgFormat::Custom, ..PgDumpOptions::default() }, blocked.clone(), Some(17));
        std::thread::spawn(move || run_backup(job, &Locator::system(), &state, &cancel, &|| {}))
    };
    while !state.range(0..50).iter().any(|line| line.text.contains("reading user-defined tables") || line.text.contains("finding the columns")) {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    cancel.cancel();
    let report = worker.join().unwrap();
    assert_eq!(report.outcome, BrOutcome::Cancelled, "{report:?}");
    assert!(report.elapsed.as_secs() < 20);
    assert!(!blocked.exists() && br_partial_in(out.path()).is_none());
}

// ---- live: MySQL and MariaDB -----------------------------------------------------------------------------------------------
//
// The host has no mysqldump: point TABLEX_R6_MYSQLDUMP at one (the tests ran with the official macOS arm64 tarball of MySQL 8.0.36
// unpacked into a scratch folder; nothing is installed); MariaDB takes its own mariadb-dump from TABLEX_R6_MARIADB_DUMP when set (ran with
// the Homebrew bottle of MariaDB 13.0.2, also unpacked into a scratch folder). Without either the tests skip and say so. The scripts the tool wrote are replayed
// through the `mysql` / `mariadb` client of the test containers (a MySQL restore is Run SQL File's job, live-tested with R5).
//
//   TABLEX_R6_MYSQLDUMP=/path/to/mysqldump TABLEX_TEST_MYSQL_URI=mysql://tablex:tablex@127.0.0.1:53306/tablex \
//   TABLEX_TEST_MARIADB_URI=mariadb://tablex:tablex@127.0.0.1:53307/tablex TABLEX_R6_MARIADB_DUMP=/path/to/mariadb-dump \
//     cargo test -p tablex-gpui live_r6_my -- --ignored --test-threads=1

struct BrLiveMy {
    container: &'static str,
    client: &'static str,
    uri: String,
    dump: String,
    kind: ConnectionKind,
    made: std::cell::RefCell<Vec<String>>,
    /// The function and the trigger could be made (a server with binary logging refuses them without SUPER).
    routines: std::cell::Cell<bool>,
    triggers: std::cell::Cell<bool>,
}

impl BrLiveMy {
    fn from_env(kind: ConnectionKind) -> Option<Self> {
        let (var, container, client) = if kind == ConnectionKind::Mariadb { ("TABLEX_TEST_MARIADB_URI", "tablex-test-mariadb", "mariadb") } else { ("TABLEX_TEST_MYSQL_URI", "tablex-test-mysql", "mysql") };
        let Ok(uri) = std::env::var(var) else {
            eprintln!("skipped: set {var}");
            return None;
        };
        // MariaDB is dumped with its own mariadb-dump when one is given, else with the mysqldump.
        let given = |var: &str| std::env::var(var).ok().filter(|path| std::path::Path::new(path).is_file());
        let Some(dump) = (if kind == ConnectionKind::Mariadb { given("TABLEX_R6_MARIADB_DUMP") } else { None }).or_else(|| given("TABLEX_R6_MYSQLDUMP")) else {
            eprintln!("NOT VERIFIED: set TABLEX_R6_MYSQLDUMP to a mysqldump binary, or TABLEX_R6_MARIADB_DUMP to a mariadb-dump for MariaDB (none is installed on this host)");
            return None;
        };
        Some(Self { container, client, uri, dump, kind, made: Default::default(), routines: Default::default(), triggers: Default::default() })
    }

    fn sql(&self, sql: &str) -> String {
        let output = std::process::Command::new("docker").args(["exec", "-i", self.container, self.client, "-utablex", "-ptablex", "-N", "-B", "-e", sql]).output().expect("docker exec");
        assert!(output.status.success(), "{sql}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Replays a script into `database` (`""`: the script names its own).
    fn replay(&self, database: &str, script: &std::path::Path) {
        let output = std::process::Command::new("docker")
            .args(["exec", "-i", self.container, self.client, "-utablex", "-ptablex", database])
            .stdin(std::fs::File::open(script).unwrap())
            .output()
            .expect("docker exec");
        assert!(output.status.success(), "replay: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn create(&self, role: &str) -> String {
        let name = format!("cf_r6_my_{role}_{}", &uuid::Uuid::new_v4().to_simple().to_string()[..8]);
        self.sql(&format!("CREATE DATABASE `{name}` DEFAULT CHARACTER SET utf8mb4"));
        self.made.borrow_mut().push(name.clone());
        name
    }

    fn source(&self) -> (String, String) {
        let name = self.create("src");
        self.sql(&format!(
            "USE `{name}`;\n\
             CREATE TABLE items (id int AUTO_INCREMENT PRIMARY KEY, name varchar(60) NOT NULL, price decimal(12,2), at datetime(3), doc json, blob_col blob, flags set('a','b','c'), mood enum('sad','ok'), body text) DEFAULT CHARSET utf8mb4;\n\
             INSERT INTO items (name, price, at, doc, blob_col, flags, mood, body) WITH RECURSIVE n(g) AS (SELECT 1 UNION ALL SELECT g + 1 FROM n WHERE g < 300) SELECT CONCAT('item ', g, ' \u{00E9}\u{4E2D}\u{1F600}'), g * 1.25, TIMESTAMP('2026-01-01 00:00:00') + INTERVAL g HOUR, JSON_OBJECT('n', g), UNHEX(LPAD(HEX(g), 4, '0')), 'a,c', 'ok', REPEAT('x', g % 50) FROM n;\n\
             CREATE TABLE notes (id int PRIMARY KEY, body text);\n\
             INSERT INTO notes VALUES (1, 'one'), (2, NULL);\n\
             CREATE VIEW cheap AS SELECT id FROM items WHERE price < 5;"
        ));
        self.routines.set(self.try_sql(&format!("USE `{name}`; CREATE FUNCTION twice(a int) RETURNS int DETERMINISTIC RETURN a * 2")));
        self.triggers.set(self.try_sql(&format!("USE `{name}`; CREATE TRIGGER items_name BEFORE UPDATE ON items FOR EACH ROW SET NEW.body = CONCAT('t', NEW.id)")));
        eprintln!("{:?} server: function {}, trigger {}", self.kind, self.routines.get(), self.triggers.get());
        let fingerprint = self.fingerprint(&name);
        (name, fingerprint)
    }

    fn try_sql(&self, sql: &str) -> bool {
        std::process::Command::new("docker").args(["exec", "-i", self.container, self.client, "-utablex", "-ptablex", "-N", "-B", "-e", sql]).output().is_ok_and(|output| output.status.success())
    }

    fn fingerprint(&self, database: &str) -> String {
        let routine = if self.routines.get() { "twice(21)" } else { "0" };
        self.sql(&format!(
            "USE `{database}`; SET SESSION group_concat_max_len = 100000000;\n\
             SELECT MD5(GROUP_CONCAT(CONCAT_WS('|', id, name, price, at, doc, HEX(blob_col), flags, mood, body) ORDER BY id SEPARATOR ';')), (SELECT COUNT(*) FROM notes), (SELECT COUNT(*) FROM cheap), {routine}, (SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = '{database}') FROM items;"
        ))
    }
}

impl Drop for BrLiveMy {
    fn drop(&mut self) {
        for name in self.made.borrow().iter() {
            let _ = std::process::Command::new("docker").args(["exec", "-i", self.container, self.client, "-utablex", "-ptablex", "-e", &format!("DROP DATABASE IF EXISTS `{name}`")]).output();
        }
    }
}

/// Registers the live connection with the app (and the mysqldump under test); returns the session key.
fn br_my_register(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, my: &BrLiveMy) -> String {
    let id = uuid::Uuid::new_v4();
    let key = id.to_string();
    let (uri, dump, key_for_app, kind) = (my.uri.clone(), my.dump.clone(), key.clone(), my.kind);
    app.update(visual, |this, cx| {
        this.saved_connections.push(SavedConnection { id, name: "live".into(), uri: uri.clone(), kind, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false });
        this.driver_runtime.session_uris.insert(key_for_app, engine_connect::records::RuntimeUri::new(uri));
        let mut settings = (*this.settings).clone();
        settings.query.mysqldump_path = dump;
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
    settle(visual);
    key
}

/// One backup of `database` through the wizard: the defaults, with `flags` clicked (toggled), written to `output`.
fn br_my_backup(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, key: &str, database: &str, flags: &[&'static str], output: &std::path::Path) -> crate::backup::job::Report {
    let target = crate::app::data_transfer::Namespace { connection: key.to_owned(), database: database.to_owned(), schema: String::new() };
    visual.update(|window, cx| app.update(cx, |this, cx| this.open_backup(crate::app::backup::BackupInit { mode: BrMode::Backup, target, tables: Vec::new(), archive: None }, window, cx)));
    settle(visual);
    br_next(visual);
    for flag in flags {
        br_click(visual, flag);
    }
    br_click(visual, "backup-choose-output");
    let chosen = output.to_owned();
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    let report = br_run_to_end(app, visual);
    br_close(app, visual);
    report
}

fn br_live_my(cx: &mut TestAppContext, kind: ConnectionKind) {
    let Some(my) = BrLiveMy::from_env(kind) else { return };
    let (source, fingerprint) = my.source();
    let (app, visual, _, _) = oa_pg(cx);
    let key = br_my_register(&app, visual, &my);
    let out = tempfile::tempdir().unwrap();
    let backup = |visual: &mut gpui::VisualTestContext, flags: &[&'static str], output: &std::path::Path| br_my_backup(&app, visual, &key, &source, flags, output);

    // The defaults: the database with its CREATE DATABASE, the routines and the triggers.
    let full = out.path().join("full.sql");
    let report = backup(visual, &[], &full);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    // The configured binary ran, and the report names it by what it says of itself.
    let said = std::process::Command::new(&my.dump).arg("--version").output().expect("the dump answers --version");
    let said = String::from_utf8_lossy(&said.stdout).into_owned();
    let tool = report.tool.clone().unwrap_or_default();
    let (name, rest) = tool.split_once(' ').unwrap_or_default();
    let version = rest.split_whitespace().next().unwrap_or_default();
    assert!(tool.ends_with(&format!("({})", my.dump)) && !version.is_empty() && said.contains(version) && my.dump.ends_with(name), "{report:?} for {said}");
    let text = std::fs::read_to_string(&full).unwrap();
    for needle in [format!("CREATE DATABASE /*!32312 IF NOT EXISTS*/ `{source}`"), "CREATE TABLE `items`".to_owned(), "INSERT INTO `items` VALUES".to_owned(), "CREATE TABLE `notes`".to_owned(), "0x".to_owned()].into_iter().chain(my.routines.get().then(|| "twice".to_owned())).chain(my.triggers.get().then(|| "items_name".to_owned())) {
        assert!(text.contains(&needle), "{needle} in the script");
    }
    assert!(!text.contains("tablex:tablex") && !std::fs::read_dir(out.path()).unwrap().flatten().any(|entry| entry.file_name().to_string_lossy().ends_with(".tablex-part")));
    assert!(report.command.as_deref().is_some_and(|command| command.contains("--defaults-file=") && !command.contains("-ptablex") && !command.contains("--password")));

    // Without CREATE DATABASE the script replays into any database: the same rows, types, routine and trigger.
    let bare = out.path().join("bare.sql");
    let report = backup(visual, &["backup-flag-create-database"], &bare);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let target = my.create("dst");
    my.replay(&target, &bare);
    assert_eq!(my.fingerprint(&target), fingerprint, "rows, types, the routine and the trigger survive");

    // Structure only, one INSERT per row.
    let shape = out.path().join("shape.sql");
    let report = backup(visual, &["backup-flag-create-database", "backup-flag-no-data"], &shape);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(!std::fs::read_to_string(&shape).unwrap().contains("INSERT INTO"));
    let rows = out.path().join("rows.sql");
    let report = backup(visual, &["backup-flag-create-database", "backup-flag-extended-insert"], &rows);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(std::fs::read_to_string(&rows).unwrap().matches("INSERT INTO `items`").count() >= 300);

    // Tables of the database only, and the dump of a wrong password is the tool's error without the password.
    use crate::backup::command::{MysqlDumpOptions, MysqlScope};
    use crate::backup::job::{run_backup, BackupJob, DumpOptions, ToolConnection};
    use crate::backup::process::{CancelToken, RunState};
    let paths = crate::backup::tools::ToolPaths { mysqldump: my.dump.clone(), ..Default::default() };
    let connection = |uri: &str| ToolConnection { saved: SavedConnection { id: uuid::Uuid::new_v4(), name: "live".into(), uri: my.uri.clone(), kind, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false }, live_uri: Some(engine_connect::records::RuntimeUri::new(uri)), tunnel: None };
    let run = |uri: &str, options: MysqlDumpOptions, output: std::path::PathBuf| {
        let job = BackupJob { connection: connection(uri), database: source.clone(), options: DumpOptions::Mysql(options), output, paths: paths.clone(), server_major: None, dirs: Default::default() };
        run_backup(job, &crate::backup::tools::Locator::system(), &RunState::new(), &std::sync::Arc::new(CancelToken::default()), &|| {})
    };
    let one = out.path().join("one.sql");
    let report = run(&my.uri, MysqlDumpOptions { scope: MysqlScope::Tables(vec!["notes".into()]), create_database: false, ..MysqlDumpOptions::default() }, one.clone());
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let text = std::fs::read_to_string(&one).unwrap();
    assert!(text.contains("CREATE TABLE `notes`") && !text.contains("CREATE TABLE `items`"));
    let bad = my.uri.replacen("tablex:tablex@", "tablex:not-the-password@", 1);
    let report = run(&bad, MysqlDumpOptions::default(), out.path().join("bad.sql"));
    let BrOutcome::Failed(message) = &report.outcome else { panic!("{report:?}") };
    assert!(message.contains("Access denied") && !message.contains("not-the-password"), "{message}");
    assert!(!out.path().join("bad.sql").exists() && br_partial_in(out.path()).is_none());
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql) and a mysqldump in TABLEX_R6_MYSQLDUMP"]
fn live_r6_mysql_backup_replays_with_the_same_rows_types_routine_and_trigger(cx: &mut TestAppContext) {
    br_live_my(cx, ConnectionKind::Mysql);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb) and a mariadb-dump in TABLEX_R6_MARIADB_DUMP or a mysqldump in TABLEX_R6_MYSQLDUMP"]
fn live_r6_mariadb_backup_replays_with_the_same_rows(cx: &mut TestAppContext) {
    br_live_my(cx, ConnectionKind::Mariadb);
}

/// R6: `--routines` and `--triggers` against a real server. A table with a trigger, a procedure and a function are backed up through the
/// wizard with each flag on and off; the script is replayed into a fresh database where the function is called, the procedure called
/// and the trigger fired. Needs the test user to create them: `log_bin_trust_function_creators=1` on a server with binary logging, and
/// `SHOW_ROUTINE` for the dump (MySQL 8) -- the tests fail, not skip, when the objects cannot be made.
fn br_live_my_routines_and_triggers(cx: &mut TestAppContext, kind: ConnectionKind) {
    let Some(my) = BrLiveMy::from_env(kind) else { return };
    let source = my.create("obj");
    my.sql(&format!(
        "USE `{source}`;\n\
         CREATE TABLE cf_r6_t (id int PRIMARY KEY, label varchar(30) NOT NULL, note varchar(60));\n\
         INSERT INTO cf_r6_t (id, label) VALUES (1, 'one'), (2, 'two');\n\
         CREATE TRIGGER cf_r6_trg BEFORE INSERT ON cf_r6_t FOR EACH ROW SET NEW.note = CONCAT('fired-', NEW.label);\n\
         CREATE PROCEDURE cf_r6_proc(IN a int, OUT b int) SET b = a * 3;\n\
         CREATE FUNCTION cf_r6_fn(a int) RETURNS int DETERMINISTIC RETURN a * 2;"
    ));
    let (app, visual, _, _) = oa_pg(cx);
    let key = br_my_register(&app, visual, &my);
    let out = tempfile::tempdir().unwrap();

    // (flags clicked off, routines in the script, trigger in the script)
    let cases: [(&str, &[&'static str], bool, bool); 4] = [
        ("both", &[], true, true),
        ("neither", &["backup-flag-routines", "backup-flag-triggers"], false, false),
        ("no-routines", &["backup-flag-routines"], false, true),
        ("no-triggers", &["backup-flag-triggers"], true, false),
    ];
    for (name, clicked, routines, triggers) in cases {
        let script = out.path().join(format!("{name}.sql"));
        let flags: Vec<&'static str> = ["backup-flag-create-database"].into_iter().chain(clicked.iter().copied()).collect();
        let report = br_my_backup(&app, visual, &key, &source, &flags, &script);
        assert_eq!(report.outcome, BrOutcome::Completed, "{name}: {report:?}");
        // MariaDB leaves the names of a trigger unquoted, MySQL quotes them.
        let text = std::fs::read_to_string(&script).unwrap().replace('`', "");
        assert!(text.contains("CREATE TABLE cf_r6_t"), "{name}: the table is in the script");
        assert_eq!(text.contains("PROCEDURE cf_r6_proc(") && text.contains("FUNCTION cf_r6_fn("), routines, "{name}: routines in the script\n{text}");
        assert_eq!(text.contains("TRIGGER cf_r6_trg BEFORE INSERT ON cf_r6_t"), triggers, "{name}: trigger in the script\n{text}");
        if routines {
            assert!(text.contains("CREATE DEFINER=tablex@% PROCEDURE cf_r6_proc(") && text.contains("CREATE DEFINER=tablex@% FUNCTION cf_r6_fn("), "{name}: a CREATE for the routines");
        } else {
            assert!(!text.contains("cf_r6_proc") && !text.contains("cf_r6_fn"), "{name}: no routine named in the script");
        }
        assert!(triggers || !text.contains("cf_r6_trg"), "{name}: no trigger named in the script");
        assert!(!triggers || text.contains("/*!50003 CREATE*/"), "{name}: a CREATE for the trigger");

        let target = my.create(&format!("dst_{}", name.replace('-', "")));
        my.replay(&target, &script);
        let count = |table: &str, column: &str, kind: &str| my.sql(&format!("SELECT COUNT(*) FROM information_schema.{table} WHERE {column} = '{target}' AND {kind}"));
        assert_eq!(count("ROUTINES", "ROUTINE_SCHEMA", "ROUTINE_NAME IN ('cf_r6_proc', 'cf_r6_fn')"), if routines { "2" } else { "0" }, "{name}: routines restored");
        assert_eq!(count("TRIGGERS", "TRIGGER_SCHEMA", "TRIGGER_NAME = 'cf_r6_trg'"), if triggers { "1" } else { "0" }, "{name}: trigger restored");
        assert_eq!(my.sql(&format!("SELECT COUNT(*) FROM `{target}`.cf_r6_t")), "2", "{name}: rows restored");
        if routines {
            assert_eq!(my.sql(&format!("SELECT `{target}`.cf_r6_fn(21)")), "42", "{name}: the restored function runs");
            assert_eq!(my.sql(&format!("CALL `{target}`.cf_r6_proc(5, @b); SELECT @b")), "15", "{name}: the restored procedure runs");
        }
        // The restored trigger fires on a new row; without it the note stays NULL.
        my.sql(&format!("INSERT INTO `{target}`.cf_r6_t (id, label) VALUES (900, 'new')"));
        assert_eq!(my.sql(&format!("SELECT IFNULL(note, 'NULL') FROM `{target}`.cf_r6_t WHERE id = 900")), if triggers { "fired-new" } else { "NULL" }, "{name}: the restored trigger fires");
        eprintln!("{kind:?} {name}: routines {routines}, trigger {triggers} verified in the script and after the restore");
    }
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql) and a mysqldump in TABLEX_R6_MYSQLDUMP"]
fn live_r6_mysql_backup_flags_routines_and_triggers_round_trip(cx: &mut TestAppContext) {
    br_live_my_routines_and_triggers(cx, ConnectionKind::Mysql);
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb) and a mariadb-dump in TABLEX_R6_MARIADB_DUMP or a mysqldump in TABLEX_R6_MYSQLDUMP"]
fn live_r6_mariadb_backup_flags_routines_and_triggers_round_trip(cx: &mut TestAppContext) {
    br_live_my_routines_and_triggers(cx, ConnectionKind::Mariadb);
}

// ---- live: through the SSH bastion of the dedicated `tablex-test-sshd` container ---------------------------------------------
//
//   rust-engine/scripts/test-sshd.sh start && source "$(rust-engine/scripts/test-sshd.sh env)"
//   TABLEX_TEST_PG_URI=postgres://tablex:tablex@127.0.0.1:55432/tablex cargo test -p tablex-gpui live_r6_postgresql_through -- --ignored

#[test]
#[ignore = "needs tablex-test-sshd (source its env file), TABLEX_TEST_PG_URI and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_through_the_ssh_tunnel_backs_up_and_closes_the_tunnel() {
    use crate::backup::command::{PgDumpOptions, PgFormat};
    use crate::backup::job::{run_backup, BackupJob, DumpOptions, ToolConnection};
    use crate::backup::process::{CancelToken, RunState};
    use crate::ssh_tunnel::{ConnectFailure, SshSecrets};
    let Some(pg) = BrLivePg::from_env() else { return };
    let Some(bastion) = Bastion::from_env() else {
        eprintln!("skipped: source the env file of rust-engine/scripts/test-sshd.sh");
        return;
    };
    let (source, fingerprint) = pg.source();
    let uri = behind_the_bastion(&pg.uri);
    let dir = tempfile::tempdir().unwrap();
    let access = crate::ssh_tunnel::TunnelAccess {
        opener: std::sync::Arc::new(EngineOpener),
        settings: bastion.settings(SshAuthMethod::KeyFile),
        secrets: SshSecrets::default(),
        known_hosts: Some(dir.path().join("known_hosts")),
        prompts: std::sync::Arc::new(engine_net::NoPrompts),
        asked: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        shared: None,
    };
    // The first connection to a bastion asks whether to trust it: here the answer is yes, as the dialog's button would give.
    match crate::ssh_tunnel::open_for_session(&access, ConnectionKind::Postgres, &uri) {
        Err(ConnectFailure::HostKeyUnknown(request)) => {
            let spec = crate::ssh_tunnel::trust_spec(&request, access.known_hosts.as_deref().unwrap());
            access.opener.trust_host_key(&spec, &request.host, request.port, &request.public_key).expect("the host key is recorded");
        }
        Ok(_) => panic!("a bastion never seen before is asked about first"),
        Err(other) => panic!("{}", other.message()),
    }
    let before = bastion.sessions();
    let paths = crate::backup::tools::ToolPaths { pg_dump: format!("{BR_PG_BIN}/pg_dump"), pg_restore: format!("{BR_PG_BIN}/pg_restore"), ..Default::default() };
    let output = dir.path().join("through.dump");
    let job = BackupJob {
        connection: ToolConnection {
            saved: SavedConnection { id: uuid::Uuid::new_v4(), name: "tunnelled".into(), uri: uri.clone(), kind: ConnectionKind::Postgres, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: true },
            live_uri: Some(engine_connect::records::RuntimeUri::new(uri.clone())),
            tunnel: Some(access),
        },
        database: source,
        options: DumpOptions::Postgres(PgDumpOptions { format: PgFormat::Custom, ..PgDumpOptions::default() }),
        output: output.clone(),
        paths: paths.clone(),
        server_major: Some(17),
        dirs: Default::default(),
    };
    let state = RunState::new();
    let report = run_backup(job, &crate::backup::tools::Locator::system(), &state, &std::sync::Arc::new(CancelToken::default()), &|| {});
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(report.via.as_deref().is_some_and(|via| via.contains(&bastion.host) || via.contains("tablex@")), "{:?}", report.via);
    // The database is `host.docker.internal` to the tool, which only the bastion's container resolves: the dump went through the tunnel.
    assert!(report.command.as_deref().is_some_and(|command| command.contains("--host=host.docker.internal")), "{:?}", report.command);
    let toc = crate::backup::job::list_toc(&output, &paths, &crate::backup::tools::Locator::system(), Some(17), &std::sync::Arc::new(crate::backup::process::CancelToken::default())).unwrap();
    assert!(toc.entries.iter().any(|entry| entry.label().contains("TABLE DATA cf_r6_s.items")));
    // And what it dumped restores to the same rows.
    let target = pg.create("tunnel");
    let restored = std::process::Command::new(format!("{BR_PG_BIN}/pg_restore"))
        .env("PGPASSWORD", pg.endpoint.password_text().unwrap())
        .args(["-h", &pg.endpoint.host, "-p", &pg.endpoint.port.unwrap().to_string(), "-U", pg.endpoint.user.as_deref().unwrap(), "-d", &crate::backup::command::dbname_argument(&target)])
        .arg(&output)
        .output()
        .unwrap();
    assert!(restored.status.success(), "{}", String::from_utf8_lossy(&restored.stderr));
    assert_eq!(pg.fingerprint(&target), fingerprint);
    // The tunnel was closed with the job.
    let mut gone = false;
    for _ in 0..100 {
        if bastion.sessions() == before {
            gone = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(gone, "no SSH connection of the job is left");
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg) and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_restore_brings_the_navigator_up_to_date(cx: &mut TestAppContext) {
    let Some(pg) = BrLivePg::from_env() else { return };
    let (source, fingerprint) = pg.source();
    let out = tempfile::tempdir().unwrap();
    let archive = out.path().join("nav.dump");
    let dumped = std::process::Command::new(format!("{BR_PG_BIN}/pg_dump"))
        .env("PGPASSWORD", pg.endpoint.password_text().unwrap())
        .args(["-h", &pg.endpoint.host, "-p", &pg.endpoint.port.unwrap().to_string(), "-U", pg.endpoint.user.as_deref().unwrap(), "-Fc", "-f"])
        .arg(&archive)
        .arg(&source)
        .output()
        .unwrap();
    assert!(dumped.status.success());
    let target = pg.create("nav");
    // The real connection, on the real driver, with its navigator open on the empty database.
    let mut rig = nav_rig(cx, ConnectionKind::Postgres, ("dbengine-postgres", "abi-v2"), &pg.uri);
    let key = rig.key.clone();
    rig.app.update(rig.visual, |this, cx| {
        let mut settings = (*this.settings).clone();
        settings.query.pg_restore_path = format!("{BR_PG_BIN}/pg_restore");
        this.settings = std::sync::Arc::new(settings);
        cx.notify();
    });
    rig.expand(&connection_node(&key));
    rig.expand(&database_node(&key, &target));
    let schemas = |rig: &SqlRig<'_>| -> Vec<String> {
        rig.app.read_with(rig.visual, |this, _| {
            this.navigator.loaded_children(&database_node(&key, &target)).unwrap_or_default().iter().filter_map(|id| if let NavigatorNodeId::Schema { schema, .. } = id { Some(schema.clone()) } else { None }).collect()
        })
    };
    assert!(!schemas(&rig).contains(&"cf_r6_s".to_owned()), "{:?}", schemas(&rig));

    let report = br_live_restore_on(&rig.app, rig.visual, &key, &target, &archive, None, &[]);
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    let app = rig.app.clone();
    let (watched_key, watched_target) = (key.clone(), target.clone());
    wait_until(rig.visual, "the navigator to list the restored schema", |visual| {
        app.read_with(visual, |this, _| {
            this.navigator.loaded_children(&database_node(&watched_key, &watched_target)).unwrap_or_default().iter().any(|id| matches!(id, NavigatorNodeId::Schema { schema, .. } if schema == "cf_r6_s"))
        })
    });
    assert_eq!(pg.fingerprint(&target), fingerprint);
}


// ---- review fixes ---------------------------------------------------------------------------------------------------------

/// A backup that runs (its tool sleeps) from the schema row to the Run step's report-less progress, into `out`.
fn br_start_running(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tools: &BrTools, out: &std::path::Path) {
    tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'half' > \"$out\"\necho started >&2\nsleep 60");
    oa_menu_on(app, visual, &br_group(), "object-menu-backup");
    br_next(visual);
    br_click(visual, "backup-choose-output");
    let chosen = out.join("warehouse.sql");
    visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
    settle(visual);
    br_wait(app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
    br_next(visual);
    br_wait(app, visual, "the tool to be found", |snapshot| snapshot.tool.starts_with("pg_dump"));
    br_click(visual, "backup-start");
    br_wait(app, visual, "the tool to start", |snapshot| snapshot.running && snapshot.progress.as_ref().is_some_and(|progress| progress.lines_seen >= 2));
}

#[gpui::test]
fn closing_the_window_or_quitting_while_a_backup_runs_asks_first_and_then_stops_the_tool_and_cleans_up(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let out = tempfile::tempdir().unwrap();
    br_start_running(&app, visual, &tools, out.path());
    let pid = tools.pid().expect("the tool recorded its process id");
    wait_until(visual, "the unfinished file", |_| br_partial_in(out.path()).is_some());
    let passfile = tools.record().lines().find_map(|line| line.strip_prefix("PGPASSFILE=")).and_then(|line| line.split(' ').next()).map(std::path::PathBuf::from).unwrap();
    assert!(passfile.exists(), "the password file exists while the tool runs");

    // The window's close box: asks, and nothing is stopped until the answer.
    assert!(!app.update_in(visual, |this, window, cx| this.window_may_close(window, cx)), "the window does not close under a running backup");
    settle(visual);
    assert!(visual.debug_bounds("discard-pending-body").is_some(), "closing the window asks");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(br_alive(pid) && app.read_with(visual, |this, _| !this.backup_controls.is_empty()), "kept, and still running");

    // Quit: asks as well.
    assert!(app.update_in(visual, |this, window, cx| this.request_quit(window, cx)), "quitting asks");
    settle(visual);
    assert!(visual.debug_bounds("discard-pending-body").is_some());
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(br_alive(pid));

    // Discard: the tool is stopped, its private files and the unfinished backup are gone, and only then does the window close.
    assert!(!app.update_in(visual, |this, window, cx| this.window_may_close(window, cx)));
    settle(visual);
    br_click(visual, "discard-pending-confirm");
    wait_until(visual, "the tool to be stopped", |_| !br_alive(pid));
    wait_until(visual, "the cleaning up", |visual| {
        settle(visual);
        br_partial_in(out.path()).is_none() && !passfile.exists() && app.read_with(visual, |this, _| this.backup_controls.is_empty())
    });
    assert!(!out.path().join("warehouse.sql").exists());
}

#[gpui::test]
fn the_end_of_the_application_stops_a_running_backup_and_waits_for_its_files_to_be_removed(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let out = tempfile::tempdir().unwrap();
    br_start_running(&app, visual, &tools, out.path());
    let pid = tools.pid().unwrap();
    wait_until(visual, "the unfinished file", |_| br_partial_in(out.path()).is_some());
    // What the platform runs when the application quits (also with no window): the future it polls for a moment.
    let passfile = tools.record().lines().find_map(|line| line.strip_prefix("PGPASSFILE=")).and_then(|line| line.split(' ').next()).map(std::path::PathBuf::from).unwrap();
    assert!(passfile.exists());
    let quit = app.update(visual, |this, cx| this.backups_stop_for_quit(cx));
    // Not after the workers had time to: the password file is gone the moment the quit handler returns.
    assert!(!passfile.exists() && !passfile.parent().unwrap().exists(), "the private directory is removed by the quit handler itself");
    visual.cx.foreground_executor().spawn(quit).detach();
    wait_until(visual, "the tool to be stopped", |_| !br_alive(pid));
    wait_until(visual, "the unfinished file to be removed", |_| br_partial_in(out.path()).is_none());
}

#[gpui::test]
fn a_database_name_with_an_equals_sign_or_options_in_it_is_a_name_and_not_a_connection_string(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let out = tempfile::tempdir().unwrap();
    for (index, name) in ["realdb sslmode=disable", "a=b", "postgresql://evil@elsewhere/x"].into_iter().enumerate() {
        tools.install("pg_dump", "pg_dump (PostgreSQL) 17.4", "printf 'dump' > \"$out\"\nexit 0");
        br_open(&app, visual, BrMode::Backup, name, None);
        br_next(visual);
        br_click(visual, "backup-choose-output");
        let chosen = out.path().join(format!("{index}.sql"));
        visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
        settle(visual);
        br_wait(&app, visual, "the output to be chosen", |snapshot| snapshot.output.is_some());
        br_next(visual);
        let report = br_run_to_end(&app, visual);
        br_close(&app, visual);
        assert_eq!(report.outcome, BrOutcome::Completed, "{name}: {report:?}");
        let record = tools.record();
        let args = record.lines().next().unwrap();
        assert!(args.contains(&format!("--dbname=dbname='{name}'")), "{name}: {args}");
        assert_eq!(args.matches("--dbname=").count(), 1);
        // The connection's own TLS mode still stands: the name did not override it.
        assert!(record.contains("PGSSLMODE=require"), "{record}");
        assert!(args.contains("--host=127.0.0.1"), "{args}");
    }
}

#[gpui::test]
fn an_archive_chosen_for_one_connection_is_not_restorable_into_a_mysql_one_after_a_switch(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("a.dump");
    std::fs::write(&archive, b"PGDMP\x01\x0e").unwrap();
    br_open(&app, visual, BrMode::Restore, "warehouse", Some(archive));
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "custom archive"));
    assert!(br_snapshot(&app, visual).can_leave_source);
    // The connection is switched to a MySQL one (the namespace being the one of its own).
    app.update(visual, |this, cx| this.backups.values().next().unwrap().clone().update(cx, |view, cx| view.switch_for_test(ConnectionKind::Mysql, "shop", cx)));
    settle(visual);
    let snapshot = br_snapshot(&app, visual);
    assert!(!snapshot.can_leave_source && snapshot.wrong_server_note, "a pg_dump archive is not restored into MySQL");
    assert!(visual.debug_bounds("backup-wrong-server").is_some());
    br_click(visual, "backup-next");
    assert_eq!(br_snapshot(&app, visual).step, "source");
    app.update(visual, |this, cx| this.backups.values().next().unwrap().clone().update(cx, |view, cx| view.switch_for_test(ConnectionKind::Postgres, "warehouse", cx)));
    settle(visual);
    assert!(br_snapshot(&app, visual).can_leave_source);
    let _ = tools;
}

#[gpui::test]
fn a_restore_with_nothing_selected_is_refused_with_the_reason_and_parallel_jobs_are_for_archives_that_have_them(cx: &mut TestAppContext) {
    let (app, visual, tools) = br_pg(cx);
    tools.install("pg_restore", "pg_restore (PostgreSQL) 17.4", "case \"$*\" in *--list*) printf '218; 1259 16385 TABLE public orders tablex\\n3352; 0 16385 TABLE DATA public orders tablex\\n'; exit 0;; esac\nexit 0");
    let dir = tempfile::tempdir().unwrap();
    let custom = dir.path().join("a.dump");
    std::fs::write(&custom, b"PGDMP\x01\x0e").unwrap();
    let mut tar = vec![0u8; 512];
    tar[257..262].copy_from_slice(b"ustar");
    let tar_path = dir.path().join("a.tar");
    std::fs::write(&tar_path, tar).unwrap();

    br_open(&app, visual, BrMode::Restore, "warehouse", Some(custom));
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "custom archive"));
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries == 2);
    assert!(visual.debug_bounds("backup-jobs").is_some(), "a custom archive restores in parallel");
    br_click(visual, "backup-select-none");
    br_next(visual);
    let snapshot = br_snapshot(&app, visual);
    assert_eq!((snapshot.step, snapshot.message.as_deref()), ("settings", Some("Nothing is selected to restore: select at least one entry of the backup.")));
    br_click(visual, "backup-select-all");
    br_next(visual);
    assert_eq!(br_snapshot(&app, visual).step, "run");
    br_close(&app, visual);

    // A tar archive: pg_restore refuses parallel jobs there, so they are not offered.
    br_open(&app, visual, BrMode::Restore, "warehouse", Some(tar_path));
    br_wait(&app, visual, "the archive to be read", |snapshot| snapshot.archive.as_ref().is_some_and(|(_, state)| state == "tar archive"));
    br_next(visual);
    br_wait(&app, visual, "the contents", |snapshot| snapshot.toc_entries == 2);
    assert!(visual.debug_bounds("backup-jobs").is_none(), "no parallel jobs for a tar archive");
    assert!(visual.debug_bounds("backup-flag-restore-single-transaction").is_some());
}

#[test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg) and the PostgreSQL 17 client tools"]
fn live_r6_postgresql_a_database_whose_name_is_a_connection_string_is_dumped_by_its_name() {
    use crate::backup::command::PgDumpOptions;
    use crate::backup::job::{run_backup, BackupJob, DumpOptions, ToolConnection};
    use crate::backup::process::{CancelToken, RunState};
    let Some(pg) = BrLivePg::from_env() else { return };
    let name = format!("cf_r6_a=b {}", &uuid::Uuid::new_v4().to_simple().to_string()[..6]);
    pg.create_named(&name);
    pg.psql(&name, "CREATE TABLE cf_r6_marker AS SELECT 42 AS answer");
    let out = tempfile::tempdir().unwrap();
    let output = out.path().join("equals.sql");
    let paths = crate::backup::tools::ToolPaths { pg_dump: format!("{BR_PG_BIN}/pg_dump"), ..Default::default() };
    let job = BackupJob {
        connection: ToolConnection {
            saved: SavedConnection { id: uuid::Uuid::new_v4(), name: "live".into(), uri: pg.uri.clone(), kind: ConnectionKind::Postgres, save_password: false, is_pinned: false, disabled: false, ssh_tunnel: false },
            live_uri: Some(engine_connect::records::RuntimeUri::new(pg.uri.clone())),
            tunnel: None,
        },
        database: name.clone(),
        options: DumpOptions::Postgres(PgDumpOptions::default()),
        output: output.clone(),
        paths,
        server_major: Some(17),
        dirs: Default::default(),
    };
    let report = run_backup(job, &crate::backup::tools::Locator::system(), &RunState::new(), &std::sync::Arc::new(CancelToken::default()), &|| {});
    assert_eq!(report.outcome, BrOutcome::Completed, "{report:?}");
    assert!(std::fs::read_to_string(&output).unwrap().contains("cf_r6_marker"));
}
