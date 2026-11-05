use super::*;

pub(super) struct ScenarioUiState {
    pub(super) service: Option<FixtureService>,
    #[cfg(feature = "ui-scenarios")]
    pub(super) last_render_state: Option<serde_json::Value>,
    #[cfg(feature = "ui-scenarios")]
    pub(super) ready_emitted: bool,
    /// Last emitted sidebar bounds per probe key: ((x, y, w, h), label).
    /// Labels ride along because recycled uniform_list slots reuse
    /// geometry when rows scroll or collapse.
    #[cfg(feature = "ui-scenarios")]
    pub(super) sidebar_bounds: std::collections::HashMap<String, ((f32, f32, f32, f32), String)>,
    /// `table-properties-edit`: the tab whose edit mode and sample edits are still to be staged.
    pub(super) properties_edit_seed: Option<u64>,
    #[cfg(test)]
    pub(super) test_terminal_count: u64,
}

impl ScenarioUiState {
    pub(super) fn new(service: Option<FixtureService>) -> Self {
        Self {
            service,
            #[cfg(feature = "ui-scenarios")]
            last_render_state: None,
            #[cfg(feature = "ui-scenarios")]
            ready_emitted: false,
            #[cfg(feature = "ui-scenarios")]
            sidebar_bounds: std::collections::HashMap::new(),
            properties_edit_seed: None,
            #[cfg(test)]
            test_terminal_count: 0,
        }
    }
}

impl AppRoot {
    pub(super) fn seed_scenario_query(&mut self) {
        if self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::TableDiagram
        }) {
            self.seed_scenario_table_diagram();
            return;
        }
        if self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::DatabaseDiagram
        }) {
            let database = crate::ui_fixture_diagram::DATABASE;
            self.workspace.open_or_activate_diagram_tab(
                crate::ui_fixture::SQLITE_ID,
                database,
                Some(database),
            );
            return;
        }
        if self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::MongoGrid
        }) {
            self.seed_scenario_mongo_query();
            return;
        }
        let tab_id = self.workspace.open_query_tab_with_metadata(
            "orders",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: Some("public".into()),
                collection: "orders".into(),
                query_mode: Some(QueryMode::PostgresTable),
                ..Default::default()
            },
        );
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.output_limit = 50;
            tab.sql = "SELECT * FROM public.orders".into();
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination = crate::query::PaginationState::new(50);
        }
        let is_loading = self
            .scenario_ui
            .service
            .as_ref()
            .is_some_and(|service| service.scenario().fixture == FixtureKind::Loading);
        if is_loading {
            let _ = self
                .workspace
                .runtime_mut(tab_id)
                .and_then(|runtime| runtime.query.begin());
            return;
        }
        self.install_scenario_structure(tab_id);
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        let result = service.query_page(
            crate::ui_fixture::POSTGRES_ID,
            "warehouse",
            "public",
            "orders",
            0,
            50,
        );
        let run = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.query.begin());
        let Some(run) = run else {
            return;
        };
        self.apply_query_result(tab_id, run, result);
        if self
            .scenario_ui
            .service
            .as_ref()
            .is_some_and(|service| service.scenario().fixture == FixtureKind::PendingEdits)
        {
            self.seed_scenario_pending_edit(tab_id);
        }
        if self
            .scenario_ui
            .service
            .as_ref()
            .is_some_and(|service| service.scenario().fixture == FixtureKind::Disconnected)
        {
            self.seed_scenario_dropped_connection(tab_id);
        }
        let kind = self
            .scenario_ui
            .service
            .as_ref()
            .map(|service| service.scenario().kind);
        match kind {
            Some(crate::ui_scenario::ScenarioKind::GroupedTabs) => {
                self.seed_scenario_grouped_tabs(tab_id, false);
            }
            Some(crate::ui_scenario::ScenarioKind::GroupedTabsLone) => {
                self.seed_scenario_grouped_tabs(tab_id, true);
            }
            Some(crate::ui_scenario::ScenarioKind::ConnectionOverrides) => {
                self.seed_scenario_grouped_tabs(tab_id, false);
                self.seed_scenario_connection_types();
            }
            Some(crate::ui_scenario::ScenarioKind::TablePropertiesEdit) => {
                self.seed_scenario_properties_edit(tab_id);
            }
            _ => {}
        }
        if matches!(
            kind,
            Some(
                crate::ui_scenario::ScenarioKind::SplitPanes
                    | crate::ui_scenario::ScenarioKind::SplitPanesDrop
            )
        ) {
            self.seed_scenario_split_panes(
                tab_id,
                kind == Some(crate::ui_scenario::ScenarioKind::SplitPanesDrop),
            );
        }
    }

    /// `connection-overrides`: Postgres is a Production connection (a stricter
    /// query timeout of its own), SQLite a Development one.
    fn seed_scenario_connection_types(&mut self) {
        use crate::settings::connection::{ConnectionProfile, OverrideValue, DEVELOPMENT, PRODUCTION};
        use crate::ui_fixture::{POSTGRES_ID, SQLITE_ID};
        let mut production = ConnectionProfile {
            type_id: PRODUCTION.into(),
            ..Default::default()
        };
        production
            .overrides
            .set("query.query_timeout_secs", OverrideValue::Number(60.0));
        self.connection_policy.set_profile(POSTGRES_ID, production);
        self.connection_policy.set_profile(
            SQLITE_ID,
            ConnectionProfile {
                type_id: DEVELOPMENT.into(),
                ..Default::default()
            },
        );
    }

    /// `connection-overrides`: the Edit Connection dialog of the Postgres
    /// fixture, opened on its Overrides tab.
    pub(super) fn open_scenario_overrides_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(crate::ui_fixture::POSTGRES_ID))
            .cloned();
        self.open_connection_dialog(saved, window, cx);
    }

    /// `grouped-tabs`: two panes side by side in the grouped-rows strip. Left:
    /// Postgres and SQLite (two chips on a chip row, the active group's tabs
    /// below). Right: one MongoDB group, whose lone chip is on an aligned
    /// chip row. `lone`: a single pane with one (Postgres) group, so its chip
    /// leads the tabs row after the pinned and global tabs.
    fn seed_scenario_grouped_tabs(&mut self, orders_tab: u64, lone: bool) {
        use crate::ui_fixture::{POSTGRES_ID, SQLITE_ID};
        // (title, connection, database, table + mode; `None` = plain SQL tab)
        let tabs = [
            ("customers", POSTGRES_ID, "warehouse", Some(("customers", QueryMode::PostgresTable))),
            ("products", POSTGRES_ID, "warehouse", Some(("products", QueryMode::PostgresTable))),
            ("Query", POSTGRES_ID, "warehouse", None),
            ("Track", SQLITE_ID, "main", Some(("Track", QueryMode::SqliteTable))),
            ("Album", SQLITE_ID, "main", Some(("Album", QueryMode::SqliteTable))),
            ("Query", SQLITE_ID, "main", None),
        ];
        let tabs = if lone { &tabs[..3] } else { &tabs[..] };
        let mut pinned = None;
        for (title, connection, database, table) in tabs.iter().cloned() {
            let id = self.workspace.open_query_tab_with_metadata(
                title,
                TabMetadata {
                    connection_id: connection.into(),
                    database: database.into(),
                    collection: table.as_ref().map_or_else(String::new, |(name, _)| (*name).into()),
                    query_mode: table.map(|(_, mode)| mode),
                    ..Default::default()
                },
            );
            if title == "Query" && connection == POSTGRES_ID {
                pinned = Some(id);
            }
        }
        // A pinned tab leads the tabs row whichever group is shown.
        if let Some(id) = pinned {
            self.workspace.set_pinned(id, true);
        }
        // Connection-state dots: Postgres connected, SQLite failed.
        if let Some(attempt) = self.sessions.begin_connect(POSTGRES_ID) {
            self.sessions.finish_connect(POSTGRES_ID, attempt, Ok(()));
        }
        if let Some(attempt) = self.sessions.begin_connect(SQLITE_ID) {
            self.sessions
                .finish_connect(SQLITE_ID, attempt, Err("unable to open database file".into()));
        }
        self.sync_connected_ids();
        self.workspace.set_group_by(crate::sidebar_preferences::TabGroupBy::Connection);
        self.workspace.set_tab_rows(true);
        if lone {
            self.workspace.activate_tab(orders_tab);
            return;
        }
        // A second pane (it inherits the settings) with one MongoDB group.
        let mongo = crate::ui_fixture::MONGO_ID;
        let (invoices, _) = self.workspace.open_or_activate_collection_tab(mongo, "shop", "invoices");
        let (customers, _) = self.workspace.open_or_activate_collection_tab(mongo, "shop", "customers");
        self.open_mongo_script_tab(mongo, "banvietbank", "txn");
        let script = self.workspace.active_tab().map(|tab| tab.id);
        if self.workspace.split_tab(invoices, crate::panes::Edge::Right) {
            if let Some(right) = self.workspace.pane_of(invoices) {
                for tab in [customers].into_iter().chain(script) {
                    self.workspace.drop_tab(
                        tab,
                        crate::panes::PaneDrop::Tabs {
                            pane: right,
                            index: None,
                        },
                    );
                }
                self.workspace.activate_tab(invoices);
            }
        }
        self.workspace.activate_tab(orders_tab);
    }

    /// `split-panes`: the orders grid stays in the left pane (focused); a SQL
    /// editor tab moves into a new pane on the right, 55/45.
    fn seed_scenario_split_panes(&mut self, orders_tab: u64, pin_drop_zone: bool) {
        let query = self.workspace.open_query_tab_with_metadata(
            "Query",
            TabMetadata {
                connection_id: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                ..Default::default()
            },
        );
        if let Some(tab) = self.workspace.tab_mut(query) {
            tab.sql = "SELECT id, status, total\nFROM public.orders\nWHERE total > 100\nORDER BY id;"
                .into();
        }
        self.workspace
            .open_query_tab_with_metadata("Notes", TabMetadata::default());
        self.workspace.activate_tab(query);
        self.workspace.split_tab(query, crate::panes::Edge::Right);
        if let crate::panes::Node::Split(split) = self.workspace.panes().root() {
            let id = split.id();
            self.workspace.set_split_ratios(id, &[0.55, 0.45]);
        }
        self.workspace.activate_tab(orders_tab);
        if pin_drop_zone {
            if let Some(pane) = self.workspace.pane_of(query) {
                self.shell.tab_bar.pane_drop = Some(crate::panes_view::DropHover {
                    pane,
                    zone: crate::panes_view::DropZone::Edge(crate::panes::Edge::Right),
                });
            }
        }
    }

    /// `disconnected` fixture: the Postgres session was live and is gone, so
    /// the orders tab (which holds rows) shows the reconnect banner, and a
    /// second Query tab with no result shows the full placeholder.
    fn seed_scenario_dropped_connection(&mut self, orders_tab: u64) {
        let connection = crate::ui_fixture::POSTGRES_ID;
        if let Some(attempt) = self.sessions.begin_connect(connection) {
            self.sessions.finish_connect(connection, attempt, Ok(()));
        }
        self.sessions.disconnect(connection);
        self.sync_connected_ids();
        self.workspace.open_query_tab_with_metadata(
            "Query",
            TabMetadata {
                connection_id: connection.into(),
                database: "warehouse".into(),
                ..Default::default()
            },
        );
        self.workspace.activate_tab(orders_tab);
    }

    /// `table-diagram`: the Track table of the Chinook-like SQLite catalog,
    /// with the Diagram presentation open. (`database-diagram` opens the
    /// whole-database diagram tab instead; see `seed_scenario_query`.)
    fn seed_scenario_table_diagram(&mut self) {
        let table = crate::ui_fixture_diagram::FOCUS_TABLE;
        let database = crate::ui_fixture_diagram::DATABASE;
        let tab_id = self.workspace.open_query_tab_with_metadata(
            table,
            TabMetadata {
                connection_id: crate::ui_fixture::SQLITE_ID.into(),
                database: database.into(),
                schema: Some(database.into()),
                collection: table.into(),
                query_mode: Some(QueryMode::SqliteTable),
                ..Default::default()
            },
        );
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.output_limit = 50;
            tab.sql = format!("SELECT * FROM \"{database}\".\"{table}\"");
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination = crate::query::PaginationState::new(50);
        }
        self.install_scenario_structure(tab_id);
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        let result = service.query_sql_table_page(
            (crate::ui_fixture::SQLITE_ID, database, database, table),
            0,
            50,
            ("", ""),
        );
        let run = self
            .workspace
            .runtime_mut(tab_id)
            .and_then(|runtime| runtime.query.begin());
        if let Some(run) = run {
            self.apply_query_result(tab_id, run, result);
        }
        self.show_diagram_presentation_for_scenario(tab_id);
    }

    fn seed_scenario_mongo_query(&mut self) {
        let tab_id = self.workspace.open_query_tab_with_metadata(
            "fixture_collection_00",
            TabMetadata {
                connection_id: crate::ui_fixture::MONGO_ID.into(),
                database: "fixture_db_0".into(),
                schema: None,
                collection: "fixture_collection_00".into(),
                query_mode: Some(QueryMode::MongoFind),
                ..Default::default()
            },
        );
        if let Some(tab) = self.workspace.tab_mut(tab_id) {
            tab.output_limit = 50;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pagination = crate::query::PaginationState::new(50);
        }
        let is_loading = self
            .scenario_ui
            .service
            .as_ref()
            .is_some_and(|service| service.scenario().fixture == FixtureKind::Loading);
        if is_loading {
            let _ = self
                .workspace
                .runtime_mut(tab_id)
                .and_then(|runtime| runtime.query.begin());
            return;
        }
        self.install_scenario_structure(tab_id);
        self.run_scenario_query_for_tab(tab_id);
        if self
            .scenario_ui
            .service
            .as_ref()
            .is_some_and(|service| service.scenario().fixture == FixtureKind::PendingEdits)
        {
            self.seed_scenario_pending_mongo_edit(tab_id);
        }
        // Capture-only: `TABLEX_SEED_AGGREGATE=text|stages|write` also opens an
        // Aggregate tab (like `TABLEX_SEED_CONNECTION_TREE` seeds the tree).
        if let Ok(mode) = std::env::var("TABLEX_SEED_AGGREGATE") {
            self.seed_scenario_aggregate(&mode);
        }
        // Capture-only: `TABLEX_SEED_SCRIPT=1` opens a Mongo New Query tab.
        if std::env::var("TABLEX_SEED_SCRIPT").as_deref() == Ok("1") {
            self.open_mongo_script_tab(
                crate::ui_fixture::MONGO_ID,
                "fixture_db_0",
                "fixture_collection_00",
            );
        }
    }

    fn seed_scenario_aggregate(&mut self, mode: &str) {
        use crate::aggregate::{AggregateDraft, AggregateOptions, EditorMode};
        let text = match mode {
            "write" => {
                "[{ $match: { active: true } }, { $sort: { count: -1 } }, { $out: \"top_active\" }]"
            }
            _ => "[{ $match: { active: true } }, { $sort: { count: -1 } }, { $limit: 5 }]",
        };
        let draft = AggregateDraft::restore(
            text,
            AggregateOptions::default(),
            if mode == "stages" { EditorMode::Stages } else { EditorMode::Text },
            true,
            None,
            None,
        );
        self.workspace.open_aggregate_tab(
            crate::ui_fixture::MONGO_ID,
            "fixture_db_0",
            "fixture_collection_00",
            draft,
        );
    }

    pub(super) fn run_scenario_query_for_tab(&mut self, tab_id: u64) {
        let Some(tab) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
        else {
            return;
        };
        let (run, offset, limit, filter, sort) = {
            let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
                return;
            };
            let Some(run) = runtime.query.begin() else {
                return;
            };
            (
                run,
                runtime.pagination.offset().max(0) as u64,
                tab.output_limit,
                runtime.mongo.filter.clone(),
                runtime.mongo.sort.clone(),
            )
        };
        let Some(service) = self.scenario_ui.service.as_mut() else {
            return;
        };
        let result = service.query_mongo_page(
            &tab.connection_id,
            &tab.database,
            &tab.collection,
            offset,
            limit,
            Some(&filter),
            Some(&sort),
        );
        self.apply_fixture_mongo_query_result(tab_id, run, result);
    }

    fn seed_scenario_pending_mongo_edit(&mut self, tab_id: u64) {
        let Some((database, collection, identity)) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .and_then(|tab| {
                let page_id = self.workspace.runtime(tab_id)?.driver_page_id?;
                let fields = self
                    .scenario_ui
                    .service
                    .as_ref()?
                    .fixture()
                    .inspect_mongo_row(page_id, 0)
                    .ok()?;
                let identity = fields.mutation_identity.clone();
                Some((
                    tab.database.clone(),
                    tab.collection.clone(),
                    identity,
                    fields,
                ))
            })
            .and_then(|(database, collection, identity, fields)| {
                fields.fields.iter().find(|field| field.name == "name")?;
                Some((database, collection, identity))
            })
        else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.mutations.stage(PendingMongoMutation::Update {
                request: MutationRequest {
                    // Fixture-only: scenario mode never reaches a driver.
                    operation_id: crate::mongo_result::STAGED_OPERATION_ID,
                    kind: "update".into(),
                    database: Some(database),
                    object: Some(collection),
                    identity,
                    payload: serde_json::json!({"$set": {"name": "pending fixture edit"}}),
                },
                row: 0,
                field: "name".into(),
                value: serde_json::json!("pending fixture edit"),
                display_text: "pending fixture edit".into(),
            });
            rebuild_mongo_pending_overlay(runtime);
        }
    }

    fn seed_scenario_pending_edit(&mut self, tab_id: u64) {
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        let position = crate::grid::CellPosition { row: 1, column: 1 };
        let Some(original_text) = runtime.grid.full_cell_text(position) else {
            return;
        };
        let original_is_null = runtime.grid.cell_is_null(position);
        let display_text = Arc::<str>::from("edited fixture");
        runtime
            .pending_edits
            .push(crate::workspace::PendingCellEdit {
                row: position.row,
                column: position.column,
                original_text: Arc::<str>::from(original_text),
                original_is_null,
                display_text: display_text.clone(),
                request: crate::staged_sql::UpdateCellRequest {
                    database: "warehouse".into(),
                    schema: "public".into(),
                    table: "orders".into(),
                    key_column: "id".into(),
                    key_value: MutationValue::Number("2".into()),
                    column: "name".into(),
                    value: MutationValue::Text(display_text.to_string()),
                },
                key: Default::default(),
            });
        runtime.set_pending_cell_text(position, display_text);
    }

    pub(super) fn run_scenario_query(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        if tab.query_mode.is_mongo_find() {
            self.run_scenario_query_for_tab(tab.id);
            return;
        }
        if tab.query_mode.is_redis() {
            self.run_scenario_redis(tab, true, cx);
            return;
        }
        let (run, offset, limit) = {
            let Some(runtime) = self.workspace.runtime_mut(tab.id) else {
                return;
            };
            let Some(run) = runtime.query.begin() else {
                return;
            };
            (
                run,
                runtime.pagination.offset().max(0) as u64,
                tab.output_limit,
            )
        };
        self.install_scenario_structure(tab.id);
        let mongo_options = (tab.query_mode.is_mongo_find()).then(|| {
            self.workspace
                .runtime(tab.id)
                .map(|runtime| {
                    (
                        runtime
                            .mongo
                            .applied_filter
                            .as_ref()
                            .and_then(|filter| serde_json::to_string(filter).ok())
                            .unwrap_or_else(|| runtime.mongo.filter.clone()),
                        runtime.mongo.sort.clone(),
                    )
                })
                .unwrap_or_else(|| ("{}".into(), r#"{"_id":1}"#.into()))
        });
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        let result = if tab.query_mode.is_table_data() {
            let (where_clause, order_by) =
                crate::sql_table::normalized_fragments(&tab.applied_where, &tab.applied_order_by);
            service.query_sql_table_page(
                (
                    &tab.connection_id,
                    &tab.database,
                    tab.schema.as_deref().unwrap_or(""),
                    &tab.collection,
                ),
                offset,
                limit,
                (&where_clause, &order_by),
            )
        } else if let Some((filter, sort)) = mongo_options.as_ref() {
            service.query_page_with_options(
                &tab.connection_id,
                &tab.database,
                tab.schema.as_deref().unwrap_or(""),
                &tab.collection,
                offset,
                limit,
                Some(filter),
                Some(sort),
            )
        } else {
            service.query_page(
                &tab.connection_id,
                &tab.database,
                tab.schema.as_deref().unwrap_or(""),
                &tab.collection,
                offset,
                limit,
            )
        };
        self.apply_query_result(tab.id, run, result);
    }

    pub(super) fn install_scenario_structure(&mut self, tab_id: u64) {
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.set_structure_columns(service.fixture().catalog_columns());
        }
    }

    pub(super) fn save_scenario_form(
        &mut self,
        inputs: &ConnectionDialogInputs,
        initial_form: ConnectionFormState,
        require_password: bool,
        operation: &'static str,
        cx: &mut Context<Self>,
    ) -> Result<ConnectionFormState, String> {
        let Some(service) = self.scenario_ui.service.as_mut() else {
            return Err("UI scenario fixture is not enabled".to_string());
        };
        let mut form = inputs.read_form(cx, initial_form);
        if require_password {
            form.validate_recovery()?;
        } else {
            form.validate()?;
        }
        let record = form.to_record(current_time(), None)?;
        let record_id = record.id.clone();
        // The fixture store keeps no network settings: the app remembers them for the dialog
        // and for the connect.
        if let Ok(id) = uuid::Uuid::parse_str(&record_id) {
            self.tunnel_ui.settings_override.insert(
                id.to_string(),
                crate::ssh_tunnel::SshSettings::from_network_json(&record.network_settings_json),
            );
        }
        service.save(record);
        self.saved_connections = service.saved_connections().to_vec();
        if let Some(profile) = form.profile.clone() {
            self.connection_policy.set_profile(&record_id, profile);
        }
        self.navigator
            .set_connections(self.saved_connections.clone());
        self.emit_scenario_event("save", inputs.is_picker(), operation, None);
        cx.notify();
        Ok(form)
    }

    pub(super) fn emit_scenario_event(
        &self,
        event: &'static str,
        picker: bool,
        operation: &'static str,
        outcome: Option<&'static str>,
    ) {
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        let (window_width, window_height) = if service.scenario().fixture == FixtureKind::Narrow {
            (720, 650)
        } else {
            (1100, 850)
        };
        let mut checkpoint = serde_json::json!({
            "event": event,
            "scenario": service.scenario().name(),
            "fixture": service.scenario().fixture_name(),
            "pid": std::process::id(),
            "test_count": service.test_count(),
            "save_count": service.save_count(),
            "tab": self.workspace.active_tab().map(|tab| tab.title.clone()).unwrap_or_default(),
            "form_step": if picker { "picker" } else { "config" },
            "operation": operation,
            "window_width": window_width,
            "window_height": window_height,
        });
        if event == "ready" {
            checkpoint["rendered"] = serde_json::Value::Bool(true);
        }
        if let Some(outcome) = outcome {
            checkpoint["outcome"] = serde_json::Value::String(outcome.to_string());
        }
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{checkpoint}");
        let _ = stdout.flush();
    }

    #[cfg(feature = "ui-scenarios")]
    pub(super) fn emit_render_state(&mut self) {
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        // The state of the tab the user works in.
        let active_id = self.workspace.active_tab().map(|tab| tab.id).unwrap_or_default();
        let view = self.view(active_id);
        let grid = self.grid(active_id);
        let selected_range = view.grid_ui.selection.borrow().range.map(|range| {
            let range = range.normalized();
            serde_json::json!({
                "start": {"row": range.start.row, "column": range.start.column},
                "end": {"row": range.end.row, "column": range.end.column},
            })
        });
        let pending = self
            .workspace
            .active_tab()
            .and_then(|tab| self.workspace.runtime(tab.id))
            .map(|runtime| runtime.pending_edits.len() + runtime.mongo.mutations.len())
            .unwrap_or_default();
        let (page_offset, total_rows, page_generation, query_generation, query_status) = self
            .workspace
            .active_tab()
            .and_then(|tab| self.workspace.runtime(tab.id))
            .map(|runtime| {
                let total_rows = runtime
                    .pagination
                    .exact_total_count()
                    .and_then(|total| usize::try_from(total).ok())
                    .unwrap_or_else(|| match runtime.query.status() {
                        crate::query::QueryStatus::Ready { rows } => *rows,
                        _ => 0,
                    });
                (
                    runtime.pagination.offset().max(0) as usize,
                    total_rows,
                    runtime.pagination.page(),
                    runtime.query.generation(),
                    runtime.query.status_name(),
                )
            })
            .unwrap_or((0, 0, 0, 0, "Idle"));
        let page_rows = grid.row_count();
        let is_bson = matches!(grid.source(), GridContentSource::Bson(_));
        let first_value = (page_rows > 0)
            .then(|| {
                grid
                    .full_cell_text(crate::grid::CellPosition { row: 0, column: 0 })
            })
            .flatten();
        let first_row_id = first_value
            .as_ref()
            .filter(|_| !is_bson)
            .and_then(|id| id.parse::<u64>().ok());
        let first_document_id = first_value.filter(|_| is_bson);
        let visible_nodes = self.navigator.visible_rows();
        let visible_labels: Vec<&str> = visible_nodes
            .iter()
            .take(8)
            .map(|row| row.label.as_str())
            .collect();
        let state = serde_json::json!({
            "event": "render-state",
            "scenario": service.scenario().name(),
            "fixture": service.scenario().fixture_name(),
            "pid": std::process::id(),
            "selected_node": self.navigator.selected().map(|node| format!("{node:?}")),
            "expanded_nodes": visible_nodes.iter().filter(|row| row.expanded).count(),
            "visible_nodes": visible_nodes.len(),
            "visible_labels": visible_labels,
            "selection_count": self.navigator.selected_ids().len(),
            "multi_mode": self.navigator.connection_multi_mode(),
            "loading_nodes": visible_nodes.iter().filter(|row| matches!(row.load, Some(crate::navigator::NodeLoadState::Loading))).count(),
            "active_tab": self.workspace.active_tab().map(|tab| tab.title.clone()),
            "active_schema": self.workspace.active_tab().and_then(|tab| tab.schema.clone()),
            "active_connection": self.workspace.active_tab().map(|tab| tab.connection_id.clone()),
            "visible_result_range": {"start": page_offset, "end": page_offset + page_rows},
            "page_offset": page_offset,
            "page_rows": page_rows,
            "total_rows": total_rows,
            "page_generation": page_generation,
            "query_generation": query_generation,
            "query_status": query_status,
            "first_row_id": first_row_id,
            "first_document_id": first_document_id,
            "result_source": if is_bson { "bson" } else { "arrow" },
            "selected_range": selected_range,
            "pending_count": pending,
            "deleted_count": self.workspace.active_tab().and_then(|tab| self.workspace.runtime(tab.id)).map(|runtime| runtime.pending_deleted_rows.len()).unwrap_or_default(),
            "inline_error": view.grid_ui.inline_edit.as_ref().is_some_and(|edit| edit.error.is_some()),
            "column_widths": &view.grid_ui.layout.widths,
        });
        if self.scenario_ui.last_render_state.as_ref() != Some(&state) {
            self.scenario_ui.last_render_state = Some(state.clone());
            crate::ui_scenario::emit_ui_state(state);
            if !self.scenario_ui.ready_emitted
                && service.scenario().kind != crate::ui_scenario::ScenarioKind::NewConnection
            {
                crate::ui_scenario::emit_ready();
                self.scenario_ui.ready_emitted = true;
            }
        }
    }

    pub(super) fn start_scenario_connect(
        &mut self,
        connection: String,
        name: String,
        request: Option<LoadRequest>,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        let Some(attempt) = self.sessions.begin_connect(&connection) else {
            if let Some(request) = request {
                if self.sessions.is_connected(&connection) {
                    self.dispatch_scenario_load(request, cx);
                } else if let Some(generation) = self.navigator.load_generation(&request) {
                    self.navigator_ui
                        .pending_loads
                        .entry(connection)
                        .or_default()
                        .push((request, generation));
                }
            }
            return;
        };
        let (response, delay) = service.connect(&name);
        let executor = cx.background_executor().clone();
        let task = cx.background_executor().spawn(async move {
            executor.timer(delay).await;
            response.outcome
        });
        let task = cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                let result = match outcome {
                    FixtureOutcome::Success => Ok(()),
                    FixtureOutcome::Error => {
                        Err("Connection failed: simulated fixture error".into())
                    }
                    FixtureOutcome::Timeout => {
                        Err("Connection timed out: simulated fixture timeout".into())
                    }
                };
                if this.sessions.finish_connect(&connection, attempt, result) {
                    this.sync_sidebar_connections(cx);
                    if matches!(outcome, FixtureOutcome::Success) {
                        this.rerun_tabs_after_reconnect(&connection, cx);
                        let is_mongo = this
                            .saved_connections
                            .iter()
                            .find(|saved| saved.matches_id(&connection))
                            .is_some_and(|saved| {
                                matches!(saved.kind, ConnectionKind::Mongodb | ConnectionKind::Redis)
                            });
                        if is_mongo {
                            let parent = NavigatorNodeId::Connection {
                                connection: connection.clone(),
                            };
                            let expanded = this
                                .navigator
                                .visible_rows()
                                .iter()
                                .any(|row| row.id == parent && row.expanded);
                            this.navigator.enable_provider(&connection);
                            this.navigator_ui.pending_loads.remove(&connection);
                            if expanded {
                                if let Some(provider_request) =
                                    this.navigator.toggle_expansion(&parent).request
                                {
                                    this.dispatch_scenario_load(provider_request, cx);
                                }
                            }
                        } else {
                            if let Some(request) = request {
                                this.dispatch_scenario_load(request, cx);
                            }
                            if let Some(requests) =
                                this.navigator_ui.pending_loads.remove(&connection)
                            {
                                for (request, generation) in requests {
                                    if this.navigator.load_generation(&request) == Some(generation)
                                    {
                                        this.dispatch_scenario_load(request, cx);
                                    }
                                }
                            }
                        }
                    } else {
                        let error = match outcome {
                            FixtureOutcome::Error => "Connection failed: simulated fixture error",
                            FixtureOutcome::Timeout => {
                                "Connection timed out: simulated fixture timeout"
                            }
                            FixtureOutcome::Success => unreachable!(),
                        }
                        .to_string();
                        if let Some(request) = request {
                            if let Some(generation) = this.navigator.load_generation(&request) {
                                this.navigator
                                    .apply_load(&request, generation, Err(error.clone()));
                            }
                        }
                        if let Some(requests) = this.navigator_ui.pending_loads.remove(&connection)
                        {
                            for (request, generation) in requests {
                                this.navigator
                                    .apply_load(&request, generation, Err(error.clone()));
                            }
                        }
                    }
                }
                this.navigator_ui.generation = this.navigator.generation();
                this.continue_sidebar_locate(cx);
                cx.notify();
            });
        });
        task.detach();
    }

    /// Capture-only deterministic tree for the connection-tree/sample
    /// screenshot (gated on TABLEX_SEED_CONNECTION_TREE=1 by the caller):
    /// Mongo provider root plus its first database expanded, and Postgres
    /// warehouse/public/Tables expanded with the orders object selected.
    /// Loads apply synchronously from the fixture so the capture never
    /// depends on async timing. Unit tests call this directly when they
    /// need a populated tree; the constructor path stays collapsed.
    pub(super) fn seed_connection_tree_scenario(&mut self) {
        #[cfg(feature = "ui-scenarios")]
        if self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::ConnectionTree
                && service.scenario().fixture == FixtureKind::States
        }) {
            self.seed_connection_states_scenario();
            return;
        }
        let is_tree_sample = self.scenario_ui.service.as_ref().is_some_and(|service| {
            service.scenario().kind == crate::ui_scenario::ScenarioKind::ConnectionTree
                && service.scenario().fixture == FixtureKind::Sample
        });
        if !is_tree_sample {
            return;
        }
        // Deterministic representative tree: Mongo provider root plus its first
        // database expanded, and Postgres warehouse/public/Tables expanded with
        // the orders object selected. Loads apply synchronously from the
        // fixture so the capture never depends on async timing.
        self.navigator.enable_provider(crate::ui_fixture::MONGO_ID);
        let mongo_root = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::MONGO_ID.into(),
        };
        self.apply_scenario_expansion(&mongo_root);
        if let Some(database) =
            self.navigator
                .find_provider_child(&mongo_root, "database", "fixture_db_0", None)
        {
            self.apply_scenario_expansion(&database);
        }
        let postgres_root = NavigatorNodeId::Connection {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
        };
        self.apply_scenario_expansion(&postgres_root);
        let warehouse = NavigatorNodeId::Database {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
        };
        self.apply_scenario_expansion(&warehouse);
        let public = NavigatorNodeId::Schema {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
            schema: "public".into(),
        };
        // Schema expansion synthesizes Tables/Views groups without a load.
        self.navigator.toggle_expansion(&public);
        let tables = NavigatorNodeId::ObjectGroup {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
            schema: "public".into(),
            group: ObjectGroup::Tables,
        };
        self.apply_scenario_expansion(&tables);
        // Select the first Mongo collection: its connection tint is green, so
        // the capture shows the selected tinted row.
        let database =
            self.navigator
                .find_provider_child(&mongo_root, "database", "fixture_db_0", None);
        let collection = database.as_ref().and_then(|database| {
            self.navigator.find_provider_child(
                database,
                "collection",
                "fixture_db_0",
                Some("fixture_collection_00"),
            )
        });
        if let Some(collection) = collection {
            self.navigator.select_visible(&collection, false, false);
        } else {
            let orders = NavigatorNodeId::Object {
                connection: crate::ui_fixture::POSTGRES_ID.into(),
                database: "warehouse".into(),
                schema: "public".into(),
                object: "orders".into(),
            };
            self.navigator.select_visible(&orders, false, false);
        }
        self.navigator_ui.generation = self.navigator.generation();
    }

    /// `connection-tree` / `states`: one connection per status, to read the
    /// status dot and the failure marker on plain, tinted and selected rows.
    /// Mongo connected (green tint, PROD), Postgres connected with a database
    /// whose load failed, SQLite disconnected, Billing failed (long error, PROD),
    /// Reporting connecting, Edge behind SSH, Staging connected and selected.
    #[cfg(feature = "ui-scenarios")]
    fn seed_connection_states_scenario(&mut self) {
        use crate::settings::connection::{ConnectionProfile, DEVELOPMENT, PRODUCTION};
        use crate::ui_fixture::{
            BILLING_ID, EDGE_ID, MONGO_ID, POSTGRES_ID, REPORTING_ID, STAGING_ID,
        };
        for id in [MONGO_ID, POSTGRES_ID, EDGE_ID, STAGING_ID] {
            if let Some(attempt) = self.sessions.begin_connect(id) {
                self.sessions.finish_connect(id, attempt, Ok(()));
            }
        }
        if let Some(attempt) = self.sessions.begin_connect(BILLING_ID) {
            self.sessions.finish_connect(
                BILLING_ID,
                attempt,
                Err("connection to server at \"billing.internal\" (10.0.4.17), port 5432 failed: \
                     Connection timed out. Is the server running on that host and accepting TCP/IP \
                     connections? Check the firewall and the listen address, then try again"
                    .into()),
            );
        }
        self.sessions.begin_connect(REPORTING_ID);
        self.sync_connected_ids();
        // A connected Mongo connection lists its databases through the provider.
        for id in [MONGO_ID, EDGE_ID] {
            self.navigator.enable_provider(id);
        }
        for (id, color) in [(BILLING_ID, "#F59E0B"), (REPORTING_ID, "#FFCC01")] {
            self.sidebar.color_overrides.entry(id.into()).or_insert_with(|| color.into());
        }
        for (id, type_id) in [(MONGO_ID, PRODUCTION), (BILLING_ID, PRODUCTION), (REPORTING_ID, DEVELOPMENT)] {
            self.connection_policy.set_profile(
                id,
                ConnectionProfile { type_id: type_id.into(), ..Default::default() },
            );
        }
        self.seed_tunnel(
            EDGE_ID.into(),
            &crate::ssh_tunnel::SshSettings {
                enabled: true,
                host: "bastion.example.com".into(),
                user: "deploy".into(),
                ..Default::default()
            },
        );
        let postgres_root = NavigatorNodeId::Connection { connection: POSTGRES_ID.into() };
        self.apply_scenario_expansion(&postgres_root);
        let warehouse = NavigatorNodeId::Database {
            connection: POSTGRES_ID.into(),
            database: "warehouse".into(),
        };
        if let Some(request) = self.navigator.toggle_expansion(&warehouse).request {
            let generation = self.navigator.load_generation(&request).unwrap_or_default();
            self.navigator.apply_load(
                &request,
                generation,
                Err("permission denied for database \"warehouse\"".into()),
            );
        }
        self.navigator
            .select_visible(&NavigatorNodeId::Connection { connection: STAGING_ID.into() }, false, false);
        self.navigator_ui.generation = self.navigator.generation();
    }

    /// `connection-overview`: the Postgres fixture's Connection tab. `sample`
    /// is connected (server 16.2, driver 0.3.1, a read-only Production type, an
    /// SSH tunnel, eight databases listed but not expanded in the sidebar); `error` failed to
    /// connect, `disconnected` never connected, `loading` is connecting.
    pub(super) fn seed_connection_overview_scenario(&mut self) {
        use crate::settings::connection::{ConnectionProfile, PRODUCTION};
        use crate::ui_fixture::POSTGRES_ID as ID;
        let Some(fixture) = self
            .scenario_ui
            .service
            .as_ref()
            .map(|service| service.scenario().fixture)
        else {
            return;
        };
        self.connection_policy.set_profile(
            ID,
            ConnectionProfile {
                type_id: PRODUCTION.into(),
                ..Default::default()
            },
        );
        if let Some(ty) = Arc::make_mut(&mut self.settings)
            .connection_types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
        {
            ty.read_only = true;
        }
        let attempt = self.sessions.begin_connect(ID);
        match (fixture, attempt) {
            (FixtureKind::Error, Some(attempt)) => {
                self.sessions.finish_connect(
                    ID,
                    attempt,
                    Err("connection to server at \"fixture.invalid\" (10.0.4.17), port 5432 \
                         failed: Connection timed out. Is the server running on that host and \
                         accepting TCP/IP connections? Check the firewall and the listen \
                         address, then try again"
                        .into()),
                );
            }
            (FixtureKind::Disconnected, _) => {
                self.sessions.disconnect(ID);
            }
            (FixtureKind::Loading, _) => {}
            (_, Some(attempt)) => {
                self.sessions.finish_connect(ID, attempt, Ok(()));
                #[cfg(any(test, feature = "ui-scenarios"))]
                self.seed_tunnel(
                    ID.into(),
                    &crate::ssh_tunnel::SshSettings {
                        enabled: true,
                        host: "bastion.example.com".into(),
                        user: "deploy".into(),
                        ..Default::default()
                    },
                );
                self.driver_runtime.server_versions.insert(ID.into(), 160_200);
                let driver = crate::connections::ConnectionKind::Postgres.driver_id();
                self.driver_ui.records.insert(
                    driver.into(),
                    engine_driver_host::InstalledDriverRecord {
                        id: driver.into(),
                        version: "0.3.1".into(),
                        ..Default::default()
                    },
                );
                let root = NavigatorNodeId::Connection {
                    connection: ID.into(),
                };
                if let Some(request) = self.navigator.begin_background_load(&root) {
                    let generation = self.navigator.load_generation(&request).unwrap_or_default();
                    let children = [
                        "analytics",
                        "billing",
                        "crm",
                        "inventory",
                        "postgres",
                        "reporting",
                        "staging",
                        "warehouse",
                    ]
                    .into_iter()
                    .map(|name| navigator::database_child(ID, name))
                    .collect();
                    self.navigator.apply_load(&request, generation, Ok(children));
                }
            }
            _ => {}
        }
        self.sync_connected_ids();
        self.workspace.open_or_activate_connection_tab(ID, "Postgres fixture");
        self.navigator_ui.generation = self.navigator.generation();
    }

    /// The fixture stands in for the server: applies the same case-insensitive
    /// substring filter and limit the driver's server-side filter would.
    fn scenario_filtered_children(
        &mut self,
        request: &LoadRequest,
        children: Vec<crate::navigator::LoadedChild>,
    ) -> Vec<crate::navigator::LoadedChild> {
        if !matches!(request, LoadRequest::Objects { .. }) {
            return children;
        }
        let Some(filter) = self.group_filter_for(request) else {
            return children;
        };
        let needle = filter.to_lowercase();
        children
            .into_iter()
            .filter(|child| child.label.to_lowercase().contains(&needle))
            .take(crate::navigator::REMOTE_FILTER_LIMIT)
            .collect()
    }

    pub(super) fn apply_scenario_expansion(&mut self, id: &NavigatorNodeId) {
        let request = self.navigator.toggle_expansion(id).request;
        let Some(request) = request else {
            return;
        };
        let loaded = self
            .scenario_ui
            .service
            .as_ref()
            .map(|service| service.load(&request));
        match loaded {
            Some(Ok(crate::ui_fixture::FixtureLoad::Children(children))) => {
                let generation = self.navigator.load_generation(&request).unwrap_or_default();
                let children = self.scenario_filtered_children(&request, children);
                self.navigator
                    .apply_load(&request, generation, Ok(children));
            }
            Some(Ok(crate::ui_fixture::FixtureLoad::Provider(page))) => {
                let generation = self.navigator.load_generation(&request).unwrap_or_default();
                self.navigator
                    .apply_provider_page(&request, generation, Ok(page));
            }
            _ => {}
        }
    }

    pub(super) fn dispatch_scenario_load(&mut self, request: LoadRequest, cx: &mut Context<Self>) {
        let Some(generation) = self.navigator.load_generation(&request) else {
            return;
        };
        if let Some(page) = self.local_aggregation_page(&request) {
            self.navigator
                .apply_provider_page(&request, generation, Ok(page));
            self.navigator_ui.generation = self.navigator.generation();
            self.continue_sidebar_locate(cx);
            return;
        }
        let Some(service) = self.scenario_ui.service.as_ref() else {
            return;
        };
        match service.load(&request) {
            Ok(crate::ui_fixture::FixtureLoad::Children(children)) => {
                let children = self.scenario_filtered_children(&request, children);
                self.apply_navigator_load(&request, generation, Ok(children), cx);
            }
            Ok(crate::ui_fixture::FixtureLoad::Provider(page)) => {
                self.navigator
                    .apply_provider_page(&request, generation, Ok(page));
                self.navigator_ui.generation = self.navigator.generation();
                self.after_provider_collections_loaded(&request, cx);
                self.continue_sidebar_locate(cx);
            }
            Err(error) => self.apply_navigator_load(&request, generation, Err(error), cx),
        }
    }
}

// ---- Redis (the `redis` scenario and the tests that need no server) ----------

impl AppRoot {
    /// A key tab or console run answered by the fixture's in-memory server.
    /// `gate`: the console's read-only / confirm-execute gate has not run yet
    /// (a run the user already confirmed comes through `run_sql`: `false`).
    pub(super) fn run_scenario_redis(&mut self, tab: Tab, gate: bool, cx: &mut Context<Self>) {
        let tab_id = tab.id;
        let db: u32 = tab.database.parse().unwrap_or(0);
        if tab.query_mode.is_redis_key() {
            let Some((run, offset, limit)) = self.workspace.runtime_mut(tab_id).and_then(|runtime| {
                let run = runtime.query.begin()?;
                Some((run, runtime.pagination.offset().max(0) as usize, tab.output_limit.max(1) as usize))
            }) else {
                return;
            };
            let name = crate::redis::decode_key(&tab.collection)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let Some(redis) = self.scenario_ui.service.as_ref().and_then(|service| service.fixture().redis()) else {
                return;
            };
            let page = redis.value_page(db, &name, offset, limit);
            let info = redis.key_info(db, &name);
            match page {
                Ok((batch, total)) => {
                    self.apply_query_result(
                        tab_id,
                        run,
                        Ok(crate::query::QueryResultBatch { batches: vec![Arc::new(batch)], total_count: total as i64 }),
                    );
                    if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                        runtime.redis.info = info.ok();
                        runtime.redis.info_read_at = Some(Instant::now());
                        runtime.redis.info_error = None;
                    }
                    self.after_redis_key_page(tab_id, cx);
                }
                Err(error) => {
                    self.apply_query_result(tab_id, run, Err(error.clone()));
                    if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                        runtime.redis.info = None;
                        runtime.redis.info_error = Some(error);
                    }
                }
            }
            return;
        }
        // The console: the read-only gate first, like the driver route.
        let command = tab.sql.trim().to_owned();
        if command.is_empty() || (gate && self.gate_editor_statement(tab_id, &command, cx)) {
            return;
        }
        let Some(run) = self.workspace.runtime_mut(tab_id).and_then(|runtime| runtime.query.begin()) else {
            return;
        };
        let request = SqlRun {
            tab_id,
            kind: RunKind::RedisCommand,
            connection: tab.connection_id.clone(),
            sql: command,
            database: tab.database.clone(),
            collection: String::new(),
            limit: tab.output_limit,
            offset: 0,
            confirmed: false,
            read_only: false,
        };
        self.run_scenario_redis_console(run, request, cx);
    }

    /// One console command on the fixture server; the confirm class opens the
    /// same dialog the driver route does.
    pub(super) fn run_scenario_redis_console(&mut self, run: crate::query::QueryRunId, request: SqlRun, cx: &mut Context<Self>) {
        use crate::ui_fixture_redis::CommandRefusal;
        let tab_id = request.tab_id;
        let db: u32 = request.database.parse().unwrap_or(0);
        let Some(redis) = self.scenario_ui.service.as_mut().and_then(|service| service.fixture_mut().redis_mut()) else {
            return;
        };
        match redis.command(db, &request.sql, request.confirmed) {
            Ok(batch) => {
                let rows = batch.num_rows() as i64;
                self.apply_query_result(
                    tab_id,
                    run,
                    Ok(crate::query::QueryResultBatch { batches: vec![Arc::new(batch)], total_count: rows }),
                );
                self.record_query_history(&request, rows, cx);
            }
            Err(CommandRefusal::Confirm(reason)) => self.await_sql_confirmation(run, request, reason, cx),
            Err(CommandRefusal::Blocked(message) | CommandRefusal::Failed(message)) => {
                self.apply_query_result(tab_id, run, Err(message));
            }
        }
    }

    /// `redis` scenario: the connection opened, its keys grouped by `:` under
    /// a key filter, and (`TABLEX_SEED_REDIS`) a value tab or the console.
    pub(super) fn seed_redis_scenario(&mut self, cx: &mut Context<Self>) {
        use crate::navigator::redis_tree::DATABASE;
        use crate::ui_fixture::REDIS_ID;
        // Connected, as after a click on the connection.
        if let Some(attempt) = self.sessions.begin_connect(REDIS_ID) {
            self.sessions.finish_connect(REDIS_ID, attempt, Ok(()));
            self.sync_sidebar_connections(cx);
        }
        self.navigator.enable_provider(REDIS_ID);
        let connection = NavigatorNodeId::Connection { connection: REDIS_ID.into() };
        self.apply_scenario_expansion(&connection);
        let db0 = NavigatorNodeId::Provider { connection: REDIS_ID.into(), node: "database:0".into() };
        let _ = DATABASE;
        // The filter shown in the capture: only keys that have a namespace.
        self.navigator.set_redis_filter(
            &db0,
            crate::redis::KeyFilter { pattern: "*:*".into(), key_type: None, group: true },
        );
        self.apply_scenario_expansion(&db0);
        for prefix in ["cache", "queue", "user"] {
            let folder = NavigatorNodeId::Provider { connection: REDIS_ID.into(), node: format!("ns:0:{prefix}") };
            self.navigator.toggle_expansion(&folder);
        }
        let view = std::env::var("TABLEX_SEED_REDIS").unwrap_or_default();
        let key_raw = |name: &str| crate::redis::encode_key(name.as_bytes());
        match view.as_str() {
            "string" => {
                self.seed_redis_key_tab("config:app", &key_raw("config:app"), cx);
            }
            "console" => {
                let tab = self.open_redis_console_tab(REDIS_ID, "0");
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    tab.sql = "HGETALL user:1002".into();
                }
                self.run_tab_query(tab, cx);
            }
            "confirm" => {
                let tab = self.open_redis_console_tab(REDIS_ID, "0");
                if let Some(tab) = self.workspace.tab_mut(tab) {
                    tab.sql = "FLUSHDB".into();
                }
            }
            _ => {
                self.seed_redis_key_tab("user:1001", &key_raw("user:1001"), cx);
            }
        }
        let key = if view == "string" { "config:app" } else { "user:1001" };
        let selected = NavigatorNodeId::Provider {
            connection: REDIS_ID.into(),
            node: format!("key:0:{}", crate::redis::encode_key(key.as_bytes())),
        };
        self.navigator.select_visible(&selected, false, false);
        self.navigator_ui.generation = self.navigator.generation();
    }

    /// `redis` scenario, `confirm` view: the unsafe-command prompt of a FLUSHDB
    /// typed in the console, open over the tab.
    pub(super) fn open_scenario_redis_confirmation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.active_tab().cloned() else {
            return;
        };
        let request = SqlRun {
            tab_id: tab.id,
            kind: RunKind::RedisCommand,
            connection: tab.connection_id.clone(),
            sql: tab.sql.clone(),
            database: tab.database.clone(),
            collection: String::new(),
            limit: tab.output_limit,
            offset: 0,
            confirmed: false,
            read_only: false,
        };
        let reason = format!("Redis command requires confirmation: {}", tab.sql);
        self.open_sql_confirmation(request, reason, window, cx);
    }

    fn seed_redis_key_tab(&mut self, name: &str, key_raw: &str, cx: &mut Context<Self>) {
        let (tab_id, _) = self.workspace.open_or_activate_redis_key_tab(
            crate::ui_fixture::REDIS_ID,
            "0",
            key_raw,
            &crate::redis::tab_label(name, "0"),
        );
        if let Some(tab) = self.workspace.tab(tab_id).cloned() {
            self.run_scenario_redis(tab, true, cx);
        }
    }
}
