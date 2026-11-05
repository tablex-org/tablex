//! Saved aggregations: Save / Save As / Create New, opening, renaming and
//! deleting from the sidebar group, and keeping the sidebar's "Aggregations"
//! nodes in step with the store.
//!
//! Storage is `engine_storage::aggregate` (the same SQLite table the previous
//! app wrote), reached through an [`AggregateStore`] owned by the app so the
//! scenario fixture can use an in-memory database. All store calls run on the
//! background executor (`smol::unblock`): SQLite may wait on another process
//! (busy timeout), and the UI thread must never.
//!
//! The row's `pipeline_json` is the effective (enabled) pipeline text; the
//! sample size, `maxTimeMS`, collation, `allowDiskUse` and disabled stages
//! travel in `options_json` (see `crate::aggregate`).
use super::*;
use engine_storage::aggregate::{AggregateStore, SavedAggregate};

/// Lazily opened store shared with background jobs (see `store_slot`).
pub(in crate::app) type StoreSlot = Arc<std::sync::OnceLock<Result<Arc<AggregateStore>, String>>>;

/// Sidebar group node (child of a collection) listing its saved pipelines.
pub(in crate::app) const GROUP_NODE_PREFIX: &str = "agg-group:";
/// Sidebar node of one saved pipeline.
pub(in crate::app) const ITEM_NODE_PREFIX: &str = "agg:";
pub(in crate::app) const GROUP_KIND: &str = "aggregations";
pub(in crate::app) const ITEM_KIND: &str = "aggregation";

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or_default()
}

/// Message for a name that cannot be used.
fn validate_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("Enter a name for the aggregation.".into());
    }
    if name.trim() != name {
        return Err("The name must not start or end with whitespace.".into());
    }
    if name.chars().count() > 120 {
        return Err("The name is too long (120 characters at most).".into());
    }
    Ok(())
}

impl AppRoot {
    /// The saved-aggregates store slot. The store is opened inside the first
    /// job that needs it, off the UI thread (SQLite may wait on another
    /// process), and an open failure is remembered rather than retried by
    /// every sidebar load. An `explicit` request (Save, Open, Rename, Delete)
    /// is the user asking again, so a failed slot is replaced and retried.
    fn store_slot(&mut self, explicit: bool) -> StoreSlot {
        if explicit && self.aggregate_store.get().is_some_and(Result::is_err) {
            self.aggregate_store = Arc::new(std::sync::OnceLock::new());
            self.aggregate_store_error_shown = false;
        }
        Arc::clone(&self.aggregate_store)
    }

    /// Runs `job` against the store off the UI thread (`smol::unblock`). The
    /// scenario fixture's in-memory store is instant and the deterministic
    /// test scheduler forbids foreign threads, so there the job runs inline.
    fn spawn_store_job<R: Send + 'static>(
        &self,
        slot: StoreSlot,
        cx: &mut Context<Self>,
        job: impl FnOnce(&AggregateStore) -> Result<R, String> + Send + 'static,
    ) -> Task<Result<R, String>> {
        let scenario = self.scenario_ui.service.is_some();
        let run = move || {
            let store = slot.get_or_init(|| {
                if scenario {
                    AggregateStore::open_in_memory()
                } else {
                    AggregateStore::open_default()
                }
                .map(Arc::new)
                .map_err(|error| format!("Could not open saved aggregations: {error}"))
            });
            match store {
                Ok(store) => job(store),
                Err(message) => Err(message.clone()),
            }
        };
        if scenario {
            return Task::ready(run());
        }
        cx.background_executor()
            .spawn(async move { smol::unblock(run).await })
    }

    // ---- save --------------------------------------------------------------------

    /// Save: updates the saved pipeline, or asks for a name first.
    pub(in crate::app) fn save_aggregate(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        if draft.saved_id.is_some() {
            self.commit_aggregate_save(tab_id, None, false, cx);
        } else {
            self.open_aggregate_save_dialog(tab_id, false, window, cx);
        }
    }

    /// Name prompt for Save (first time) and Save As.
    pub(in crate::app) fn open_aggregate_save_dialog(
        &mut self,
        tab_id: u64,
        as_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        let collection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.collection.clone())
            .unwrap_or_default();
        let initial = draft
            .name
            .clone()
            .map(|name| if as_new { format!("{name} copy") } else { name })
            .unwrap_or_else(|| format!("{collection} pipeline"));
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Aggregation name")
                .default_value(initial)
        });
        let focus_input = input.clone();
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            dialog
                .title(if as_new { "Save Aggregation As" } else { "Save Aggregation" })
                .child(
                    div()
                        .debug_selector(|| "agg-save-name".into())
                        .child(Input::new(&input)),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Save"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    if let Err(message) = validate_name(&name) {
                        window.open_alert_dialog(cx, move |dialog, _, _| {
                            dialog.title("Invalid name").child(message.clone())
                        });
                        return false;
                    }
                    let _ = root.update(cx, |this, cx| {
                        this.commit_aggregate_save(tab_id, Some(name), as_new, cx);
                    });
                    true
                })
        });
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Writes the tab's pipeline. `name = None` keeps the saved name.
    pub(in crate::app) fn commit_aggregate_save(
        &mut self,
        tab_id: u64,
        name: Option<String>,
        as_new: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        let Some((connection, database, collection)) = self.workspace.tab(tab_id).map(|tab| {
            (
                tab.connection_id.clone(),
                tab.database.clone(),
                tab.collection.clone(),
            )
        }) else {
            return;
        };
        let name = name.or_else(|| draft.name.clone()).unwrap_or_default();
        if let Err(message) = validate_name(&name) {
            self.aggregate_ui.tab_mut(tab_id).notice = Some(message);
            cx.notify();
            return;
        }
        let slot = self.store_slot(true);
        let id = if as_new {
            None
        } else {
            draft.saved_id.clone()
        }
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = now_secs();
        let record = draft.to_saved(&id, &connection, &database, &collection, &name, now, now);
        let wanted = name.clone();
        // What was saved is this revision: edits made while the save runs
        // must keep the draft "unsaved".
        let saved_revision = draft.revision;
        let task = self.spawn_store_job(slot, cx, move |store| {
            let existing = store
                .list(&record.conn_uuid, &record.database)
                .map_err(|error| error.to_string())?;
            if existing
                .iter()
                .any(|row| row.id != record.id && row.name.eq_ignore_ascii_case(&wanted))
            {
                return Err(format!(
                    "An aggregation named \"{wanted}\" already exists in this database."
                ));
            }
            store.upsert(&record).map_err(|error| error.to_string())?;
            store
                .list(&record.conn_uuid, &record.database)
                .map_err(|error| error.to_string())
                .map(|rows| (record.id.clone(), rows))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok((saved_id, rows)) => {
                        if let Some(draft) = this.aggregate_draft_mut(tab_id) {
                            draft.name = Some(name.clone());
                            draft.saved_id = Some(saved_id);
                            draft.dirty = draft.revision != saved_revision;
                            let title = crate::workspace::aggregate_tab_title(&collection, draft);
                            if let Some(tab) = this.workspace.tab_mut(tab_id) {
                                tab.title = title;
                            }
                        }
                        this.aggregate_ui.tab_mut(tab_id).notice =
                            Some(format!("Saved \"{name}\""));
                        this.apply_saved_aggregates(&connection, &database, rows, cx);
                    }
                    Err(message) => {
                        this.report_error(Area::Query, Some(&connection), &message);
                        this.aggregate_ui.tab_mut(tab_id).notice = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- open ----------------------------------------------------------------------

    /// Opens a saved pipeline (activating its tab when it is already open).
    pub(in crate::app) fn open_saved_aggregate(
        &mut self,
        connection: &str,
        id: &str,
        cx: &mut Context<Self>,
    ) {
        let open = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| {
                tab.connection_id == connection
                    && tab
                        .aggregate
                        .as_ref()
                        .is_some_and(|draft| draft.saved_id.as_deref() == Some(id))
            })
            .map(|tab| tab.id);
        if let Some(tab_id) = open {
            self.workspace.activate_tab(tab_id);
            cx.notify();
            return;
        }
        let slot = self.store_slot(true);
        let (connection, id) = (connection.to_string(), id.to_string());
        let task = self.spawn_store_job(slot, cx, move |store| store.get(&id).map_err(|error| error.to_string()));
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Some(saved)) => {
                        let draft = AggregateDraft::from_saved(&saved);
                        this.open_aggregate_tab(
                            &connection,
                            &saved.database,
                            &saved.collection,
                            draft,
                            cx,
                        );
                    }
                    Ok(None) => {
                        this.sidebar.notice = Some("That saved aggregation no longer exists.".into())
                    }
                    Err(message) => {
                        this.report_error(Area::Query, Some(&connection), &message);
                        this.sidebar.notice = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- rename / delete (sidebar) ----------------------------------------------------

    pub(in crate::app) fn open_rename_saved_aggregate_dialog(
        &mut self,
        connection: String,
        database: String,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .saved_aggregates
            .get(&(connection.clone(), database.clone()))
            .and_then(|rows| rows.iter().find(|row| row.id == id))
            .map(|row| row.name.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Aggregation name")
                .default_value(current)
        });
        let focus_input = input.clone();
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let root = root.clone();
            let (connection, database, id) = (connection.clone(), database.clone(), id.clone());
            dialog
                .title("Rename Aggregation")
                .child(
                    div()
                        .debug_selector(|| "agg-rename-name".into())
                        .child(Input::new(&input)),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Rename"),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let name = input.read(cx).value().to_string();
                    if let Err(message) = validate_name(&name) {
                        window.open_alert_dialog(cx, move |dialog, _, _| {
                            dialog.title("Invalid name").child(message.clone())
                        });
                        return false;
                    }
                    let (connection, database, id) =
                        (connection.clone(), database.clone(), id.clone());
                    let _ = root.update(cx, |this, cx| {
                        this.rename_saved_aggregate(connection, database, id, name, cx);
                    });
                    true
                })
        });
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }

    fn rename_saved_aggregate(
        &mut self,
        connection: String,
        database: String,
        id: String,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let slot = self.store_slot(true);
        let (store_connection, store_database, store_id, store_name) =
            (connection.clone(), database.clone(), id.clone(), name.clone());
        let task = self.spawn_store_job(slot, cx, move |store| {
            let rows = store
                .list(&store_connection, &store_database)
                .map_err(|error| error.to_string())?;
            if rows
                .iter()
                .any(|row| row.id != store_id && row.name.eq_ignore_ascii_case(&store_name))
            {
                return Err(format!(
                    "An aggregation named \"{store_name}\" already exists in this database."
                ));
            }
            let mut row = store
                .get(&store_id)
                .map_err(|error| error.to_string())?
                .ok_or("That saved aggregation no longer exists.")?;
            row.name = store_name;
            row.updated_at = now_secs();
            store.upsert(&row).map_err(|error| error.to_string())?;
            store
                .list(&store_connection, &store_database)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(rows) => {
                        // Open tabs of that pipeline follow the new name.
                        let renamed: Vec<u64> = this
                            .workspace
                            .tabs()
                            .iter()
                            .filter(|tab| {
                                tab.aggregate
                                    .as_ref()
                                    .is_some_and(|draft| draft.saved_id.as_deref() == Some(id.as_str()))
                            })
                            .map(|tab| tab.id)
                            .collect();
                        for tab_id in renamed {
                            if let Some(draft) = this.aggregate_draft_mut(tab_id) {
                                draft.name = Some(name.clone());
                                let collection = this
                                    .workspace
                                    .tab(tab_id)
                                    .map(|tab| tab.collection.clone())
                                    .unwrap_or_default();
                                let title = crate::workspace::aggregate_tab_title(
                                    &collection,
                                    this.aggregate_draft(tab_id).expect("draft exists"),
                                );
                                if let Some(tab) = this.workspace.tab_mut(tab_id) {
                                    tab.title = title;
                                }
                            }
                        }
                        this.apply_saved_aggregates(&connection, &database, rows, cx);
                    }
                    Err(message) => {
                        this.report_error(Area::Query, Some(&connection), &message);
                        this.sidebar.notice = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::app) fn confirm_delete_saved_aggregate(
        &mut self,
        connection: String,
        database: String,
        id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .saved_aggregates
            .get(&(connection.clone(), database.clone()))
            .and_then(|rows| rows.iter().find(|row| row.id == id))
            .map(|row| row.name.clone())
            .unwrap_or_default();
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let (connection, database, id) = (connection.clone(), database.clone(), id.clone());
            dialog
                .title("Delete saved aggregation?")
                .child(format!("\"{name}\" will be removed. Open tabs keep their pipeline."))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"),
                )
                .confirm()
                .footer(
                    gpui_component::dialog::DialogFooter::new()
                        .child(
                            gpui_component::dialog::DialogClose::new()
                                .trigger(|button| button.label("Cancel")),
                        )
                        .child(
                            div().debug_selector(|| "agg-delete-confirm".into()).child(
                                gpui_component::dialog::DialogAction::new()
                                    .child(Button::new("agg-delete").label("Delete").danger()),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    let (connection, database, id) =
                        (connection.clone(), database.clone(), id.clone());
                    let _ = root.update(cx, |this, cx| {
                        this.delete_saved_aggregate(connection, database, id, cx);
                    });
                    true
                })
        });
        cx.notify();
    }

    fn delete_saved_aggregate(
        &mut self,
        connection: String,
        database: String,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let slot = self.store_slot(true);
        let (store_connection, store_database, store_id) =
            (connection.clone(), database.clone(), id.clone());
        let task = self.spawn_store_job(slot, cx, move |store| {
            store.delete(&store_id).map_err(|error| error.to_string())?;
            store
                .list(&store_connection, &store_database)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(rows) => {
                        // Tabs keep their pipeline but are no longer "saved".
                        let detached: Vec<u64> = this
                            .workspace
                            .tabs()
                            .iter()
                            .filter(|tab| {
                                tab.aggregate
                                    .as_ref()
                                    .is_some_and(|draft| draft.saved_id.as_deref() == Some(id.as_str()))
                            })
                            .map(|tab| tab.id)
                            .collect();
                        for tab_id in detached {
                            if let Some(draft) = this.aggregate_draft_mut(tab_id) {
                                draft.saved_id = None;
                                draft.dirty = true;
                            }
                        }
                        this.apply_saved_aggregates(&connection, &database, rows, cx);
                    }
                    Err(message) => {
                        this.report_error(Area::Query, Some(&connection), &message);
                        this.sidebar.notice = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- sidebar sync --------------------------------------------------------------------

    /// Stores the saved list of one database and updates the sidebar's
    /// collection rows (a collection is expandable only while it has saved
    /// pipelines) and the already-loaded group / item nodes in place.
    pub(in crate::app) fn apply_saved_aggregates(
        &mut self,
        connection: &str,
        database: &str,
        rows: Vec<SavedAggregate>,
        cx: &mut Context<Self>,
    ) {
        let key = (connection.to_string(), database.to_string());
        self.saved_aggregates.insert(key, rows);
        for (id, collection) in self.navigator.provider_collection_nodes(connection, database) {
            let group = self.aggregation_nodes(connection, database, &collection, false);
            let has_saved = !group.is_empty();
            if self.navigator.set_provider_has_children(&id, has_saved) && !has_saved {
                // The flag went off: nothing to show below any more.
                self.navigator.reset_node(&id);
                continue;
            }
            if !self.navigator.has_loaded_children(&id) {
                continue;
            }
            let group_id = group.first().map(|node| NavigatorNodeId::Provider {
                connection: connection.to_string(),
                node: node.id.clone(),
            });
            self.navigator.replace_provider_children(&id, group);
            if let Some(group_id) = group_id {
                if self.navigator.has_loaded_children(&group_id) {
                    let items = self.aggregation_nodes(connection, database, &collection, true);
                    self.navigator.replace_provider_children(&group_id, items);
                }
            }
        }
        self.navigator_ui.generation = self.navigator.generation();
        cx.notify();
    }

    /// Saved pipelines of a database (fetch + decorate), used when the
    /// sidebar loads a database's collections.
    pub(in crate::app) fn saved_for_collection(
        &self,
        connection: &str,
        database: &str,
        collection: &str,
    ) -> Vec<SavedAggregate> {
        self.saved_aggregates
            .get(&(connection.to_string(), database.to_string()))
            .map(|rows| {
                rows.iter()
                    .filter(|row| row.collection == collection)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Sidebar nodes below a collection: the "Aggregations (n)" group
    /// (`items = false`, empty when nothing is saved) or the saved pipelines
    /// themselves (`items = true`).
    fn aggregation_nodes(
        &self,
        connection: &str,
        database: &str,
        collection: &str,
        items: bool,
    ) -> Vec<engine_driver_api::NavigatorNode> {
        let node = |id: String, kind: &str, label: String, has_children: bool, payload| {
            engine_driver_api::NavigatorNode {
                id,
                kind: kind.into(),
                label,
                subtitle: None,
                icon: None,
                has_children,
                payload,
                actions: vec!["open".into()],
            }
        };
        let saved = self.saved_for_collection(connection, database, collection);
        if items {
            return saved
                .into_iter()
                .map(|row| {
                    node(
                        format!("{ITEM_NODE_PREFIX}{}", row.id),
                        ITEM_KIND,
                        row.name.clone(),
                        false,
                        serde_json::json!({
                            "database": database,
                            "collection": collection,
                            "group": GROUP_KIND,
                            "aggregate_id": row.id,
                        }),
                    )
                })
                .collect();
        }
        if saved.is_empty() {
            return Vec::new();
        }
        vec![node(
            format!("{GROUP_NODE_PREFIX}{database}:{collection}"),
            GROUP_KIND,
            format!("Aggregations ({})", saved.len()),
            true,
            serde_json::json!({
                "database": database,
                "collection": collection,
                "group": GROUP_KIND,
            }),
        )]
    }

    /// Local answer to a sidebar load of a collection (-> the Aggregations
    /// group) or of the group (-> the saved pipelines). `None` for every
    /// other request, which goes to the driver as before.
    pub(in crate::app) fn local_aggregation_page(
        &self,
        request: &LoadRequest,
    ) -> Option<engine_driver_api::NavigatorPage> {
        let LoadRequest::Provider {
            connection,
            parent: Some(parent),
            payload: Some(payload),
            ..
        } = request
        else {
            return None;
        };
        if !self.is_mongo_connection(connection) {
            return None;
        }
        let database = payload.get("database")?.as_str()?;
        let collection = payload.get("collection")?.as_str()?;
        let is_group = payload.get("group").and_then(serde_json::Value::as_str) == Some(GROUP_KIND);
        if !is_group && (parent.starts_with(GROUP_NODE_PREFIX) || parent.starts_with(ITEM_NODE_PREFIX)) {
            return None;
        }
        Some(engine_driver_api::NavigatorPage {
            nodes: self.aggregation_nodes(connection, database, collection, is_group),
            next_cursor: None,
        })
    }

    pub(in crate::app) fn is_mongo_connection(&self, connection: &str) -> bool {
        self.saved_connections.iter().any(|saved| {
            saved.matches_id(connection) && saved.kind == crate::connections::ConnectionKind::Mongodb
        })
    }

    /// Loads the saved list of `database` (background) into the cache and the
    /// sidebar. Called after a Mongo database's collections were loaded.
    pub(in crate::app) fn refresh_saved_aggregates(
        &mut self,
        connection: &str,
        database: &str,
        cx: &mut Context<Self>,
    ) {
        let slot = self.store_slot(false);
        let (connection, database) = (connection.to_string(), database.to_string());
        let (store_connection, store_database) = (connection.clone(), database.clone());
        let task = self.spawn_store_job(slot, cx, move |store| {
            store
                .list(&store_connection, &store_database)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(rows) => this.apply_saved_aggregates(&connection, &database, rows, cx),
                Err(message) => {
                    // Implicit load: say so once; Save / Open retry explicitly.
                    if !this.aggregate_store_error_shown {
                        this.aggregate_store_error_shown = true;
                        this.report_error(Area::Query, Some(&connection), &message);
                        this.sidebar.notice = Some(message);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

/// Context menu of a saved-aggregation row: Open, Rename, Delete.
pub(in crate::app) fn build_saved_aggregate_menu(
    menu: gpui_component::menu::PopupMenu,
    root: gpui::WeakEntity<AppRoot>,
    connection: String,
    database: String,
    aggregate_id: String,
) -> gpui_component::menu::PopupMenu {
    let entry = |selector: &'static str, label: &'static str, danger: bool| {
        PopupMenuItem::element(move |_, _| {
            let row = div()
                .id(selector)
                .debug_selector(move || selector.to_string())
                .child(label);
            if danger {
                row.text_color(theme::danger())
            } else {
                row
            }
        })
    };
    let open = {
        let (root, connection, id) = (root.clone(), connection.clone(), aggregate_id.clone());
        entry("agg-menu-open", "Open", false).on_click(move |_, _, app| {
            let _ = root.update(app, |this, cx| this.open_saved_aggregate(&connection, &id, cx));
        })
    };
    let rename = {
        let (root, connection, database, id) = (
            root.clone(),
            connection.clone(),
            database.clone(),
            aggregate_id.clone(),
        );
        entry("agg-menu-rename", "Rename", false).on_click(move |_, window, app| {
            let (connection, database, id) = (connection.clone(), database.clone(), id.clone());
            let _ = root.update(app, |this, cx| {
                this.open_rename_saved_aggregate_dialog(connection, database, id, window, cx)
            });
        })
    };
    let delete = entry("agg-menu-delete", "Delete", true).on_click(move |_, window, app| {
        let (connection, database, id) = (connection.clone(), database.clone(), aggregate_id.clone());
        let _ = root.update(app, |this, cx| {
            this.confirm_delete_saved_aggregate(connection, database, id, window, cx)
        });
    });
    menu.item(open).item(rename).separator().item(delete)
}

impl AppRoot {
    /// `(connection, database, aggregate id)` of a saved-aggregation row.
    pub(in crate::app) fn saved_aggregation_target(
        &self,
        id: &NavigatorNodeId,
    ) -> Option<(String, String, String)> {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return None;
        };
        let node = self.navigator.provider_node(id)?;
        if node.kind != ITEM_KIND {
            return None;
        }
        let text = |key: &str| node.payload.get(key)?.as_str().map(str::to_string);
        Some((connection.clone(), text("database")?, text("aggregate_id")?))
    }

    /// After a Mongo database's collections were loaded: fetch its saved
    /// aggregations so collections that have some become expandable.
    pub(in crate::app) fn after_provider_collections_loaded(
        &mut self,
        request: &LoadRequest,
        cx: &mut Context<Self>,
    ) {
        let LoadRequest::Provider {
            connection,
            parent: Some(_),
            payload: Some(payload),
            ..
        } = request
        else {
            return;
        };
        if !self.is_mongo_connection(connection) || payload.get("collection").is_some() {
            return;
        }
        let Some(database) = payload.get("database").and_then(serde_json::Value::as_str) else {
            return;
        };
        self.refresh_saved_aggregates(connection, database, cx);
    }
}
