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
mod ui_scenario;
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
            eprintln!("TableX UI scenario arguments: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) =
        ui_scenario::ensure_feature_enabled(scenario.as_ref(), cfg!(feature = "ui-scenarios"))
    {
        eprintln!("TableX UI scenario arguments: {error}");
        std::process::exit(2);
    }
    let scenario = match scenario
        .map(ui_scenario::UiScenario::with_environment)
        .transpose()
    {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("TableX UI scenario environment: {error}");
            std::process::exit(2);
        }
    };
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
            app::open_with_scenario(scenario, cx);
        });
}

#[cfg(not(feature = "ui"))]
fn main() {
    let scenario = match ui_scenario::UiScenario::from_args(std::env::args()) {
        Ok(scenario) => scenario,
        Err(error) => {
            eprintln!("TableX UI scenario arguments: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = ui_scenario::ensure_feature_enabled(scenario.as_ref(), false) {
        eprintln!("TableX UI scenario arguments: {error}");
        std::process::exit(2);
    }
}

#[cfg(all(test, feature = "ui"))]
mod bootstrap_tests {
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
