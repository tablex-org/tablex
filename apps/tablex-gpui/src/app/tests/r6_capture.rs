// Offscreen captures of the backup and restore wizard: the Source of a backup with tables chosen, its options, a run in progress with the
// tool's log, the summary, the contents of an archive to restore, and the restore of an SQL script (headless Metal renderer, real text
// system). Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_backup_screens -- --ignored --nocapture --test-threads=1

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_backup_screens() {
    use crate::app::backup::{BackupInit, Mode};
    use crate::backup::job::{Operation, Outcome, Report};
    use crate::backup::process::{RunState, Stream};
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let data = tempfile::tempdir().unwrap();
    // A fake pg_restore that lists an archive, so the contents of a restore are real list rows.
    let bin = data.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let restore = bin.join("pg_restore");
    let mut listing = String::from("echo '; Archive created at 2026-10-26 10:15:02 CEST'\n");
    for (id, kind, name) in [(10, "SCHEMA", "- public"), (212, "TABLE", "public customers"), (213, "TABLE", "public orders"), (214, "TABLE", "public order_items"), (215, "SEQUENCE", "public orders_id_seq"), (3301, "TABLE DATA", "public customers"), (3302, "TABLE DATA", "public orders"), (3303, "TABLE DATA", "public order_items"), (3402, "CONSTRAINT", "public orders orders_pkey"), (3403, "FK CONSTRAINT", "public order_items order_items_order_id_fkey")] {
        listing.push_str(&format!("echo '{id}; 1259 {id} {kind} {name} tablex'\n"));
    }
    std::fs::write(&restore, format!("#!/bin/sh\ncase \"$1\" in --version) echo 'pg_restore (PostgreSQL) 17.4'; exit 0;; esac\n{listing}exit 0\n")).unwrap();
    std::fs::set_permissions(&restore, std::fs::Permissions::from_mode(0o755)).unwrap();
    let dump = bin.join("pg_dump");
    std::fs::write(&dump, "#!/bin/sh\ncase \"$1\" in --version) echo 'pg_dump (PostgreSQL) 17.4'; exit 0;; esac\nexit 0\n").unwrap();
    std::fs::set_permissions(&dump, std::fs::Permissions::from_mode(0o755)).unwrap();
    let archive = data.path().join("warehouse-20261026-101502.dump");
    std::fs::write(&archive, b"PGDMP\x01\x0e").unwrap();
    let script = data.path().join("warehouse-20261026-101502.sql");
    std::fs::write(&script, "-- PostgreSQL database dump\nCREATE TABLE t(a int);\n").unwrap();

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
        let (dump_path, restore_path) = (dump.display().to_string(), restore.display().to_string());
        cx.update_window(handle, |_, _, app_cx| {
            app.update(app_cx, |this, cx| {
                this.seed_connection_tree_scenario();
                let mut settings = (*this.settings).clone();
                settings.query.pg_dump_path = dump_path.clone();
                settings.query.pg_restore_path = restore_path.clone();
                this.settings = Arc::new(settings);
                this.driver_runtime.server_versions.insert(postgres.into(), 170_400);
                cx.notify();
            })
        })
        .unwrap();
        settle(&mut cx);

        // A backup of three tables: Source, then the options.
        let tables = vec!["orders".to_owned(), "customers".to_owned(), "order_items".to_owned()];
        cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_backup(BackupInit { mode: Mode::Backup, target: target.clone(), tables: tables.clone(), archive: None }, window, cx))).unwrap();
        settle(&mut cx);
        let wizard = cx.update(|app_cx| app.read(app_cx).backups.values().next().cloned().expect("the wizard"));
        shoot(&mut cx, "backup-source");
        let output = std::path::PathBuf::from("/Users/ann/Backups/warehouse-20261026-101502.dump");
        cx.update_window(handle, |_, window, app_cx| wizard.update(app_cx, |view, cx| view.seed_options(1, output.clone(), window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "backup-options");

        // Running, with the tool's verbose lines.
        let state = RunState::new();
        state.push(Stream::Command, "$ pg_dump --host=db.example.com --port=5432 --username=ann --no-password --dbname=warehouse --format=c --file=/Users/ann/Backups/warehouse-20261026-101502.dump.tablex-part --table=\"public\".\"orders\" --table=\"public\".\"customers\" --verbose");
        for line in [
            "pg_dump: last built-in OID is 16383",
            "pg_dump: reading extensions",
            "pg_dump: identifying extension members",
            "pg_dump: reading schemas",
            "pg_dump: reading user-defined tables",
            "pg_dump: reading user-defined functions",
            "pg_dump: reading indexes",
            "pg_dump: reading constraints",
            "pg_dump: reading triggers",
            "pg_dump: flagging inherited columns in subtables",
            "pg_dump: dumping contents of table \"public.customers\"",
            "pg_dump: dumping contents of table \"public.order_items\"",
            "pg_dump: dumping contents of table \"public.orders\"",
        ] {
            state.push(Stream::Err, line);
        }
        state.set_bytes(48_300_000);
        cx.update_window(handle, |_, _, app_cx| wizard.update(app_cx, |view, cx| view.seed_run(state.clone(), std::time::Duration::from_secs(23), cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "backup-running");
        let report = Report {
            operation: Operation::Backup,
            outcome: Outcome::Completed,
            tool: Some("pg_dump 17.4 (/opt/homebrew/opt/postgresql@17/bin/pg_dump)".into()),
            command: Some("pg_dump --host=db.example.com --port=5432 --username=ann --no-password --dbname=warehouse --format=c --file=/Users/ann/Backups/warehouse-20261026-101502.dump.tablex-part --verbose".into()),
            output: Some(output.clone()),
            bytes: 61_400_000,
            elapsed: std::time::Duration::from_secs_f64(27.4),
            errors: Vec::new(),
            leftover: None,
            via: Some("deploy@bastion.example.com:2222".into()),
        };
        cx.update_window(handle, |_, _, app_cx| wizard.update(app_cx, |view, cx| view.seed_report(report.clone(), state.clone(), cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "backup-summary");

        // A restore: the contents of a custom archive, then an SQL script.
        let tab = cx.update(|app_cx| app.read(app_cx).workspace.active_tab().unwrap().id);
        cx.update_window(handle, |_, _, app_cx| app.update(app_cx, |this, cx| this.close_tabs_now(&[tab], None, cx))).unwrap();
        settle(&mut cx);
        cx.update_window(handle, |_, window, app_cx| app.update(app_cx, |this, cx| this.open_backup(BackupInit { mode: Mode::Restore, target: target.clone(), tables: Vec::new(), archive: Some(archive.clone()) }, window, cx))).unwrap();
        settle(&mut cx);
        let wizard = cx.update(|app_cx| app.read(app_cx).backups.values().next().cloned().expect("the wizard"));
        cx.update_window(handle, |_, window, app_cx| wizard.update(app_cx, |view, cx| view.next(window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "restore-contents");
        cx.update_window(handle, |_, window, app_cx| wizard.update(app_cx, |view, cx| view.set_archive(script.clone(), window, cx))).unwrap();
        settle(&mut cx);
        shoot(&mut cx, "restore-script");
    }
}
