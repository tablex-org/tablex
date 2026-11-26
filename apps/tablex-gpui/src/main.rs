// A release build on Windows is a GUI program: without this it opens a console window next to the app.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

// The interface texts: `t!` with a key like `area.name` reads them in the language of `i18n::code()` (a plain
// `rust_i18n::t!` would read the process-wide locale, which tests must not share).
#[cfg(feature = "ui")]
rust_i18n::i18n!("locales", fallback = "en");
#[cfg(feature = "ui")]
macro_rules! t {
    ($key:expr $(, $($args:tt)*)?) => {
        rust_i18n::t!($key, locale = crate::i18n::code() $(, $($args)*)?)
    };
}
// `t!` as a `SharedString`, for the components that take one by `From` and not by `Into` (a checkbox label).
#[cfg(feature = "ui")]
macro_rules! ts {
    ($($args:tt)*) => {
        gpui::SharedString::from(t!($($args)*))
    };
}

#[cfg(feature = "ui")]
mod affected_rows;
#[cfg(feature = "ui")]
mod agent_access;
mod aggregate;
#[cfg(feature = "ui")]
mod binary_value;
mod bson_page;
#[cfg(feature = "ui")]
mod ai;
#[cfg(feature = "ui")]
mod backup;
#[cfg(feature = "ui")]
mod assets;
mod auto_refresh;
mod catalog;
#[cfg(feature = "ui")]
mod cell_detail;
#[cfg(feature = "ui")]
mod chart_data;
#[cfg(feature = "ui")]
mod chart_svg;
#[cfg(feature = "ui")]
mod cell_value;
#[cfg(feature = "ui")]
mod columns_panel;
mod completion;
#[cfg(feature = "ui")]
mod connection_dashboard;
#[cfg(feature = "ui")]
mod connection_dialog;
mod connection_form;
mod connection_tls;
mod connection_import;
mod connections;
mod credentials;
#[cfg(feature = "ui")]
mod ddl_refresh;
mod drivers;
#[cfg(feature = "ui")]
mod error_log;
#[cfg(feature = "ui")]
mod gpui_session;
#[cfg(feature = "ui")]
mod grid;
#[cfg(feature = "ui")]
mod grid_arrow;
#[cfg(all(feature = "ui", test))]
mod grid_bench;
#[cfg(feature = "ui")]
mod grid_bson;
#[cfg(feature = "ui")]
mod grid_source;
#[cfg(feature = "ui")]
mod i18n;
#[cfg(feature = "ui")]
mod import;
#[cfg(feature = "ui")]
mod json_text;
// The pipeline parser / validator the MongoDB driver compiles, shared so the
// editor accepts exactly what the driver runs.
mod mongo_admin;
#[allow(dead_code)]
#[path = "../../../rust-engine/crates/dbengine-mongo/src/aggregate/mod.rs"]
mod mongo_aggregate;
mod mongo_result;
mod mongo_script;
mod mongo_sql;
#[cfg(feature = "ui")]
mod mongo_sql_highlight;
#[cfg(feature = "ui")]
mod mongo_shell;
mod navigator;
#[cfg(feature = "ui")]
mod panes;
#[cfg(feature = "ui")]
mod panes_view;
#[cfg(all(feature = "ui", feature = "perf-diagnostics"))]
mod perf_diagnostics;
#[cfg(all(feature = "ui", test))]
mod perf_bench;
#[cfg(test)]
mod perf_counters;
#[cfg(feature = "ui")]
mod plan_view;
mod pinned_column;
mod platform;
#[cfg(feature = "ui")]
mod properties_model;
mod query;
mod query_mode;
mod redis;
mod saved_sql;
#[cfg(feature = "ui")]
mod script_run;
#[cfg(feature = "ui")]
mod selection_stats;
mod session;
mod sessions;
#[cfg(feature = "ui")]
mod settings;
mod sidebar_preferences;
mod sql_account;
mod sql_ddl;
#[cfg(feature = "ui")]
mod sql_file;
mod sql_filter;
#[cfg(feature = "ui")]
mod sql_format;
mod sql_params;
mod sql_script;
mod sql_table;
#[cfg(feature = "ui")]
mod sql_write;
mod ssh_tunnel;
mod staged_sql;
mod transaction;
mod ui_fixture;
#[cfg(feature = "ui")]
mod ui_fixture_diagram;
#[cfg(feature = "ui")]
mod ui_fixture_properties;
mod ui_fixture_redis;
#[cfg(feature = "ui")]
mod ui_fixture_showcase;
mod ui_scenario;
#[cfg(feature = "ui")]
mod vim;
#[cfg(feature = "ui")]
mod workspace;

#[cfg(feature = "ui")]
mod app;
#[cfg(feature = "ui")]
mod theme;
#[cfg(feature = "ui")]
mod ui;

#[cfg(feature = "ui")]
fn main() {
    let scenario = match ui_scenario::UiScenario::from_args(std::env::args()) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("tablex UI scenario arguments: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) =
        ui_scenario::ensure_feature_enabled(scenario.as_ref(), cfg!(feature = "ui-scenarios"))
    {
        eprintln!("tablex UI scenario arguments: {error}");
        std::process::exit(2);
    }
    let scenario = match scenario
        .map(ui_scenario::UiScenario::with_environment)
        .transpose()
    {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("tablex UI scenario environment: {error}");
            std::process::exit(2);
        }
    };
    // Without a display GPUI starts headless: no window, nothing printed, and the process stays
    // running in the background (holding the MCP channel). Say why and exit instead.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    if scenario.is_none() && !has_display(&gpui::GraphicalEnvironment::detect()) {
        eprintln!("{}", t!("startup.no_display"));
        std::process::exit(1);
    }
    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_component::init(cx);
            theme::init(cx);
            // A scenario is a deterministic capture: the capture runner waits for the
            // window to stop changing, so nothing may keep animating (the connecting
            // dot's pulse renders its still frame).
            if scenario.is_some() {
                cx.set_reduce_motion(true);
            }
            // Dev-only: report foreground hangs into the scenario telemetry.
            #[cfg(feature = "perf-diagnostics")]
            if scenario.is_some() {
                perf_diagnostics::install(cx);
            }
            // Moves the old files into the app database (first start of a new version) on a
            // thread of its own: the stores are ready before the first read, and a store read
            // earlier just waits for its own import. A scenario never touches the user's data.
            if scenario.is_none() {
                std::thread::spawn(engine_storage::warm_up);
            }
            app::open_with_scenario(scenario, cx);
        });
}

/// Whether the session names a display server GPUI can open a window on (`ZED_HEADLESS` aside).
#[cfg(all(feature = "ui", any(target_os = "linux", target_os = "freebsd")))]
fn has_display(environment: &gpui::GraphicalEnvironment) -> bool {
    environment.guess_compositor() != "headless"
}

#[cfg(not(feature = "ui"))]
fn main() {
    let scenario = match ui_scenario::UiScenario::from_args(std::env::args()) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("tablex UI scenario arguments: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = ui_scenario::ensure_feature_enabled(scenario.as_ref(), false) {
        eprintln!("tablex UI scenario arguments: {error}");
        std::process::exit(2);
    }
}

#[cfg(all(test, feature = "ui"))]
mod bootstrap_tests {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    #[test]
    fn a_session_without_a_display_is_refused() {
        let environment = |wayland: Option<&str>, x11: Option<&str>| gpui::GraphicalEnvironment {
            wayland_display: wayland.map(Into::into),
            x11_display: x11.map(Into::into),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            current_desktop: None,
            activation_token: None,
        };
        assert!(!super::has_display(&environment(None, None)));
        assert!(!super::has_display(&environment(Some(""), Some(""))));
        assert!(super::has_display(&environment(None, Some(":0"))));
        assert!(super::has_display(&environment(Some("wayland-0"), None)));
        assert_eq!(
            rust_i18n::t!("startup.no_display", locale = "en"),
            "tablex needs a desktop session: neither WAYLAND_DISPLAY nor DISPLAY is set. Start it from your desktop, or from a terminal inside the desktop session."
        );
    }

    /// macOS renders zero glyphs unless `gpui-pre-platform` is built with its
    /// `font-kit` feature (`MacPlatform::new` falls back to `NoopTextSystem`
    /// and logs "compiled without the `font-kit` feature, so no text will be
    /// rendered"). The `#[gpui::test]` harness always uses `NoopTextSystem`,
    /// so only the manifest wiring can guard this; screenshot evidence in the
    /// UI loop is the real acceptance.
    #[test]
    fn macos_platform_enables_font_kit() {
        let manifest = include_str!("../Cargo.toml");
        let line = manifest
            .lines()
            .find(|line| line.contains("gpui-pre-platform"))
            .expect("tablex-gpui must depend on gpui-pre-platform");
        assert!(
            line.contains("font-kit"),
            "gpui-pre-platform must enable its `font-kit` feature or macOS renders no text: {line}"
        );
    }
}
