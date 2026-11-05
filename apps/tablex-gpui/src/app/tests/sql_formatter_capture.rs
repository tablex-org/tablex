// Offscreen captures of the SQL formatter screens, through GPUI's headless Metal renderer with the real text
// system. Dev-only:
//
//   TABLEX_CAPTURE_DIR=/abs/dir cargo test -p tablex-gpui --features bench,ui-scenarios \
//     capture_sql_formatter_screens -- --ignored --nocapture --test-threads=1
//
// One PNG per screen and theme:
//   `driver-manager-sql-formatter`   the Tools section with the MCP server and SQL Formatter (sqruff) side by side:
//                                    Install, the licence and source line, what it is for
//   `settings-format-sql-sqruff`     Settings > Editor > Format SQL with SQL Formatter (sqruff) chosen and installed
//   `settings-format-sql-external`   the same group with the external command chosen, a preset applied, and
//                                    the presets found on the PATH (the PATH of a machine with these tools)

fn sql_formatter_found(installed: bool) -> super::settings_format::FormatScan {
    use super::settings_format::{FormatScan, FormatterToolState};
    use crate::sql_format::external::PRESETS;
    FormatScan {
        tool: if installed {
            FormatterToolState::Installed { version: "0.1.0".into() }
        } else {
            FormatterToolState::NotInstalled
        },
        presets: PRESETS.iter().copied().filter(|preset| matches!(preset.id, "sqruff" | "sqlfluff-format" | "pg_format")).collect(),
    }
}

#[test]
#[ignore = "writes PNGs to TABLEX_CAPTURE_DIR"]
fn capture_sql_formatter_screens() {
    let Ok(dir) = std::env::var("TABLEX_CAPTURE_DIR") else {
        eprintln!("skipped: set TABLEX_CAPTURE_DIR to an absolute directory");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (theme_name, mode) in [("light", gpui_component::ThemeMode::Light), ("dark", gpui_component::ThemeMode::Dark)] {
        for name in ["driver-manager-sql-formatter", "settings-format-sql-sqruff", "settings-format-sql-external"] {
            let kind = if name.starts_with("driver-manager") { "driver-manager-tools" } else { "settings" };
            let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", kind, "--ui-fixture", "sample"])
                .unwrap()
                .unwrap();
            // The Format SQL group is far down the Editor page: a tall window shows it whole.
            let height = if kind == "settings" { 2350.0 } else { 800.0 };
            let (mut cx, handle, app) = shell_capture_window_sized(mode, scenario, gpui::size(gpui::px(1280.0), gpui::px(height)));
            shell_capture_frames(&mut cx, handle);
            if kind == "settings" {
                // Let the page's own look at the machine finish, then show what a machine with these tools finds.
                cx.update_window(handle, |_, _window, app_cx| {
                    app.update(app_cx, |this, cx| {
                        let installed = name.ends_with("sqruff");
                        let page = this.settings_ui.page.as_mut().expect("the settings page");
                        if installed {
                            page.draft.editor.formatter = crate::settings::FormatterChoice::Sqruff;
                        } else {
                            page.draft.editor.formatter = crate::settings::FormatterChoice::External;
                            page.draft.editor.format_external_program = "sqlfluff".into();
                            page.draft.editor.format_external_args = "format --dialect {dialect} -".into();
                            page.request_resync("editor.format_external_program");
                            page.request_resync("editor.format_external_args");
                        }
                        this.format_ui_set_scan_for_test(sql_formatter_found(installed));
                        cx.notify();
                    });
                })
                .unwrap();
            }
            shell_capture_frames(&mut cx, handle);
            let image = cx.capture_screenshot(handle).expect("the headless renderer draws the window");
            let path = dir.join(format!("{name}-{theme_name}.png"));
            image.save(&path).expect("write the PNG");
            eprintln!("wrote {}", path.display());
        }
    }
}
