//! "Go to object" (Cmd+Shift+D, Ctrl+Shift+D elsewhere): a quick-open dialog for the objects of
//! the connected connections.
//!
//! It is the palette's own list and ranker (a `PaletteDelegate` in goto mode) over one segment:
//! what the navigator has loaded (databases, schemas, tables, views, functions, procedures,
//! MongoDB collections), with the connection / database / schema path of each result. Typing
//! also starts the sidebar's remote search for the query (`remote_search`): debounced, at most
//! `MAX_IN_FLIGHT` loads at a time, the databases per connection and the matches per group
//! capped, never connecting a connection. Every catalog load that lands refreshes the results;
//! a new query, choosing a result and closing the dialog cancel the search and restore the
//! groups it narrowed. Choosing a table, view or collection reveals it in the tree and opens it
//! as Enter on the row does; a function or procedure is revealed and its source opened; a
//! database or schema is revealed.

use super::delegate::PaletteDelegate;
use super::index::{Index, Segment};
use super::*;
use crate::app::remote_search::{remote_search_wanted, DEBOUNCE, MAX_IN_FLIGHT};
use crate::navigator::sql_driver;

#[derive(Default)]
pub(super) struct GotoState {
    list: Option<gpui::Entity<ListState<PaletteDelegate>>>,
    catalog: Option<CatalogCache>,
    /// The dialog is open.
    open: bool,
    /// Compare DDL With…: the first table or view; the object chosen in the dialog is the second one.
    compare_from: Option<NavigatorNodeId>,
    /// The catalog the dialog's remote search loads into (the sidebar's own is never touched);
    /// made when the first search starts, dropped with the dialog.
    nav: Option<NavigatorState>,
    /// Counts the dialog's openings and closings: an answer of an earlier one is ignored.
    epoch: u64,
    /// Bumped by each query: the debounce of an earlier query does nothing.
    generation: u64,
    /// The query the remote search looks for, once its debounce is over.
    filter: Option<String>,
    loading: bool,
    pumping: bool,
    repump: bool,
}

impl AppRoot {
    /// Cmd+Shift+D: opens the dialog, or closes it when it is already open. Never stacks on
    /// another dialog.
    pub(in crate::app) fn open_goto_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_goto(None, window, cx);
    }

    /// The dialog; with `compare_from` it picks the second object of a DDL compare instead of revealing the one chosen.
    pub(in crate::app) fn open_goto(&mut self, compare_from: Option<NavigatorNodeId>, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.goto.open && window.has_active_dialog(cx) {
            window.close_dialog(cx);
            self.end_goto_search(cx);
            return;
        }
        if window.has_active_dialog(cx) {
            return;
        }
        let index = self.goto_index();
        let list = match self.palette.goto.list.clone() {
            Some(list) => list,
            None => {
                let root = cx.entity().downgrade();
                let list = cx.new(|cx| {
                    ListState::new(PaletteDelegate::new_goto(root), window, cx).searchable(true)
                });
                self.palette.goto.list = Some(list.clone());
                list
            }
        };
        self.palette.goto.open = true;
        let comparing = compare_from.is_some();
        self.palette.goto.compare_from = compare_from;
        self.palette.goto.epoch += 1;
        self.palette.goto.catalog = None;
        list.update(cx, |state, cx| {
            state.delegate_mut().load(index, Vec::new());
            state.set_query("", window, cx);
        });
        let dialog_list = list.clone();
        let root = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let list = dialog_list.clone();
            let root = root.clone();
            dialog
                .p_0()
                .w(px(DIALOG_WIDTH))
                .close_button(false)
                .on_close(move |_, _, cx| {
                    let _ = root.update(cx, |app, cx| app.end_goto_search(cx));
                })
                .content(move |content, _, cx| {
                    let height = list.read(cx).delegate().panel_height();
                    content.child(
                        div()
                            .debug_selector(|| "goto-list".into())
                            .w_full()
                            .h(px(height))
                            .child(
                                List::new(&list)
                                    .search_placeholder(if comparing {
                                        t!("menu.compare_pick_placeholder")
                                    } else {
                                        std::borrow::Cow::Borrowed("Go to table, view, function, collection…")
                                    }),
                            ),
                    )
                })
        });
        list.update(cx, |state, cx| state.focus(window, cx));
    }

    /// The searchable snapshot of the dialog: the catalog with the routines, rebuilt only when the
    /// navigator's catalog (or the connection list) changed.
    fn goto_index(&mut self) -> Arc<Index> {
        let revision = self.navigator.catalog_revision()
            + self.palette.goto.nav.as_ref().map_or(0, NavigatorState::catalog_revision);
        let connections = self.connections_signature();
        let catalog: Arc<Segment> = match &self.palette.goto.catalog {
            Some(cache) if cache.revision == revision && cache.connections == connections => {
                cache.segment.clone()
            }
            _ => {
                let own = self.palette.goto.nav.as_ref();
                let navs: Vec<&NavigatorState> = std::iter::once(&*self.navigator).chain(own).collect();
                let segment = Arc::new(self.catalog_segment_of(&navs, true));
                self.palette.goto.catalog = Some(CatalogCache {
                    revision,
                    connections,
                    segment: segment.clone(),
                });
                segment
            }
        };
        // The delegate addresses segments by the palette's `SEG_*` positions.
        Arc::new(Index {
            segments: vec![
                Arc::default(),
                Arc::default(),
                Arc::default(),
                catalog,
            ],
        })
    }

    /// The query of the dialog changed: a remote search for it starts after the debounce (a new
    /// query cancels the one before; under two characters there is none).
    pub(in crate::app::command_palette) fn goto_query_changed(&mut self, query: String, cx: &mut Context<Self>) {
        if !self.palette.goto.open {
            return;
        }
        let goto = &mut self.palette.goto;
        goto.generation += 1;
        goto.loading = false;
        let generation = goto.generation;
        if !remote_search_wanted(&query, false, false) {
            goto.filter = None;
            return;
        }
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                if !this.palette.goto.open || this.palette.goto.generation != generation {
                    return;
                }
                if this.palette.goto.nav.is_none() {
                    let nav = this.goto_new_catalog();
                    this.palette.goto.nav = Some(nav);
                }
                this.palette.goto.filter = Some(query);
                this.palette.goto.loading = true;
                this.goto_pump(cx);
                this.refresh_goto_object(cx);
            });
        })
        .detach();
    }

    /// An empty catalog set up like the sidebar's (its connections, visibility filters, database
    /// scopes and what each connected driver lists): the dialog's search loads into it.
    fn goto_new_catalog(&self) -> NavigatorState {
        let mut nav = NavigatorState::new(self.saved_connections.clone());
        let _ = nav.set_preferences(self.navigator.preferences().clone());
        nav.set_database_scopes(self.connection_policy.database_scopes(&self.saved_connections));
        nav.set_settings_defaults(
            !self.settings.navigation.show_system_objects,
            u64::from(self.settings.navigation.navigator_page_size),
        );
        for saved in self.saved_connections.iter() {
            let id = saved.id.to_string();
            if !self.sessions.is_connected(&id) {
                continue;
            }
            let capabilities = self.driver_capabilities(&id, saved.kind.driver_id());
            nav.set_driver_kinds(&id, capabilities.map(|capabilities| capabilities.navigator_kinds.clone()));
            nav.enable_provider(&id);
        }
        nav
    }

    /// Starts as many of the search's loads as the budget allows (never connecting: only the
    /// connections with a live session are searched), like `pump_remote_search` does for the sidebar.
    fn goto_pump(&mut self, cx: &mut Context<Self>) {
        if !self.palette.goto.loading || !self.palette.goto.open {
            return;
        }
        if self.palette.goto.pumping {
            // A synchronous (fixture) load finished inside the loop below.
            self.palette.goto.repump = true;
            return;
        }
        self.palette.goto.pumping = true;
        loop {
            self.palette.goto.repump = false;
            let scenario = self.scenario_ui.service.is_some();
            let connected: HashSet<String> = self
                .sessions
                .connected()
                .filter(|id| scenario || self.driver_runtime.sessions.contains_key(*id))
                .map(str::to_string)
                .collect();
            let goto = &mut self.palette.goto;
            let (Some(filter), Some(nav)) = (goto.filter.clone(), goto.nav.as_mut()) else {
                break;
            };
            let budget = MAX_IN_FLIGHT.saturating_sub(nav.loading_count());
            let step = nav.filter_step(&connected, &filter, budget);
            if step.finished() {
                goto.loading = false;
            }
            for request in step.requests {
                self.goto_dispatch(request, cx);
            }
            if !self.palette.goto.repump || !self.palette.goto.loading {
                break;
            }
        }
        self.palette.goto.pumping = false;
    }

    /// One load of the search, into the dialog's catalog (the loaders of the sidebar's tree, the
    /// answer applied elsewhere).
    fn goto_dispatch(&mut self, request: LoadRequest, cx: &mut Context<Self>) {
        let epoch = self.palette.goto.epoch;
        let Some(nav) = self.palette.goto.nav.as_mut() else {
            return;
        };
        let Some(generation) = nav.load_generation(&request) else {
            return;
        };
        let filter = nav.request_filter(&request);
        let connection = request.connection_id();
        if let Some(service) = self.scenario_ui.service.as_ref() {
            // The fixture stands in for the server, filter and limit included.
            let loaded = service.load(&request);
            match loaded {
                Ok(crate::ui_fixture::FixtureLoad::Children(children)) => {
                    let children = match (&request, filter) {
                        (LoadRequest::Objects { .. }, Some(filter)) => {
                            let needle = filter.to_lowercase();
                            children
                                .into_iter()
                                .filter(|child| child.label.to_lowercase().contains(&needle))
                                .take(crate::navigator::REMOTE_FILTER_LIMIT)
                                .collect()
                        }
                        _ => children,
                    };
                    nav.apply_load(&request, generation, Ok(children));
                }
                Ok(crate::ui_fixture::FixtureLoad::Provider(page)) => {
                    nav.apply_provider_page(&request, generation, Ok(page));
                }
                Err(error) => {
                    nav.apply_load(&request, generation, Err(error));
                }
            }
            self.goto_loaded(cx);
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(&connection).cloned() else {
            nav.apply_load(&request, generation, Err("driver session is not connected".into()));
            self.goto_loaded(cx);
            return;
        };
        if matches!(request, LoadRequest::Provider { .. }) {
            let driver_request = navigator_ui::navigator_request_from_load(&request);
            let task = cx.background_executor().spawn(async move {
                smol::unblock(move || navigator_ui::list_driver_nav_nodes(&session, &driver_request)).await
            });
            cx.spawn(async move |this, cx| {
                let result = task.await.map_err(|error| crate::sessions::redact_secrets(&error.to_string()));
                let _ = this.update(cx, |this, cx| {
                    if this.palette.goto.epoch == epoch {
                        if let Some(nav) = this.palette.goto.nav.as_mut() {
                            nav.apply_provider_page(&request, generation, result);
                        }
                        this.goto_loaded(cx);
                    }
                });
            })
            .detach();
            return;
        }
        let supports_schemas = self.sql_supports_schemas(&connection);
        let Some(driver_request) = sql_driver::catalog_request(&request, supports_schemas, filter.as_deref()) else {
            // A driver without a schema level: its one schema is the database.
            let children = match &request {
                LoadRequest::Schemas { connection, database } => {
                    vec![crate::navigator::schema_child(connection, database, database)]
                }
                _ => Vec::new(),
            };
            if let Some(nav) = self.palette.goto.nav.as_mut() {
                nav.apply_load(&request, generation, Ok(children));
            }
            self.goto_loaded(cx);
            return;
        };
        let single_page = filter.is_some();
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                sql_driver::collect_pages(driver_request, single_page, |request| {
                    navigator_ui::list_driver_nav_nodes(&session, request)
                })
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.palette.goto.epoch != epoch {
                    return;
                }
                let children = result
                    .map(|nodes| sql_driver::catalog_children(&request, &nodes))
                    .map_err(|error| crate::sessions::redact_secrets(&error.to_string()));
                if let Some(nav) = this.palette.goto.nav.as_mut() {
                    nav.apply_load(&request, generation, children);
                }
                this.goto_loaded(cx);
            });
        })
        .detach();
    }

    /// A load of the search landed: the next ones start and the results are ranked again.
    fn goto_loaded(&mut self, cx: &mut Context<Self>) {
        self.goto_pump(cx);
        self.refresh_goto_object(cx);
    }

    /// The catalog changed (a load landed): the open dialog ranks the larger catalog.
    pub(in crate::app) fn refresh_goto_object(&mut self, cx: &mut Context<Self>) {
        if !self.palette.goto.open {
            return;
        }
        let Some(list) = self.palette.goto.list.clone() else {
            return;
        };
        let index = self.goto_index();
        self.with_ui_window(cx, move |window, cx| {
            list.update(cx, |state, cx| {
                state.delegate_mut().replace_index(index);
                if state.selected_index().is_none() && state.delegate().shows_rows() {
                    state.set_selected_index(Some(gpui_component::IndexPath::default()), window, cx);
                }
                cx.notify();
            });
        });
    }

    /// The dialog is gone: its search ends and its catalog is dropped (answers still on their way
    /// find the epoch changed and do nothing).
    pub(in crate::app) fn end_goto_search(&mut self, _cx: &mut Context<Self>) {
        let goto = &mut self.palette.goto;
        if !std::mem::take(&mut goto.open) {
            return;
        }
        goto.epoch += 1;
        goto.generation += 1;
        goto.nav = None;
        goto.catalog = None;
        goto.compare_from = None;
        goto.filter = None;
        goto.loading = false;
    }

    /// Reveals the chosen object in the tree and opens it.
    pub(in crate::app) fn goto_object_node(&mut self, node: &NavigatorNodeId, cx: &mut Context<Self>) {
        // A MongoDB node may be known to the dialog's catalog only: read it before that is dropped.
        let provider = self
            .palette
            .goto
            .nav
            .as_ref()
            .and_then(|nav| nav.provider_node(node))
            .or_else(|| self.navigator.provider_node(node))
            .cloned();
        // Compare DDL With…: the choice is the second object, whatever is revealed or opened otherwise.
        if let Some(first) = self.palette.goto.compare_from.take() {
            self.end_goto_search(cx);
            if matches!(node, NavigatorNodeId::Object { .. }) {
                self.open_ddl_compare(first, node.clone(), cx);
            } else {
                self.sidebar.notice = Some(t!("menu.compare_pick_table").into_owned());
                cx.notify();
            }
            return;
        }
        self.end_goto_search(cx);
        let connection = |id: &str| NavigatorNodeId::Connection { connection: id.to_string() };
        let result = match node {
            NavigatorNodeId::Object { connection: c, database, schema, .. } => {
                let path = vec![
                    connection(c),
                    NavigatorNodeId::Database { connection: c.clone(), database: database.clone() },
                    NavigatorNodeId::Schema {
                        connection: c.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                    },
                    NavigatorNodeId::ObjectGroup {
                        connection: c.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                        group: crate::navigator::ObjectGroup::Tables,
                    },
                    node.clone(),
                ];
                self.locate_and_open_in_sidebar(path, None, cx)
            }
            NavigatorNodeId::Provider { connection: c, .. } => {
                let target = provider.as_ref().and_then(|provider| {
                    let text = |key: &str| provider.payload.get(key)?.as_str().map(str::to_string);
                    match provider.kind.as_str() {
                        "collection" => Some((text("database")?, Some(text("collection")?))),
                        "database" => Some((text("database")?, None)),
                        _ => None,
                    }
                });
                match target {
                    Some((database, Some(collection))) => {
                        self.locate_and_open_in_sidebar(vec![connection(c)], Some((database, collection)), cx)
                    }
                    // A database: revealed and opened as a click on its row does.
                    Some((database, None)) => {
                        self.locate_and_open_in_sidebar(vec![connection(c)], Some((database, String::new())), cx)
                    }
                    None => Ok(()),
                }
            }
            NavigatorNodeId::SchemaObject { connection: c, database, schema, group, .. } => {
                let path = vec![
                    connection(c),
                    NavigatorNodeId::Database { connection: c.clone(), database: database.clone() },
                    NavigatorNodeId::Schema {
                        connection: c.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                    },
                    NavigatorNodeId::ObjectGroup {
                        connection: c.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                        group: *group,
                    },
                    node.clone(),
                ];
                self.shell.sidebar_collapsed = false;
                self.reveal_path_in_sidebar(path, cx);
                let label = self.navigator.label(node).unwrap_or_default().to_string();
                self.open_schema_object_source(node, &label, cx);
                Ok(())
            }
            NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. } => {
                self.reveal_node_in_sidebar(node, cx);
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.sidebar.locate_error = Some(error);
            self.notify_chrome(cx);
        }
    }
}

#[cfg(test)]
impl AppRoot {
    /// `(badge, title, path)` of the rows the open dialog shows.
    pub(in crate::app) fn goto_compare_pending_for_test(&self) -> bool {
        self.palette.goto.compare_from.is_some()
    }

    pub(in crate::app) fn goto_filter_for_test(&self) -> Option<String> {
        self.palette.goto.filter.clone()
    }

    pub(in crate::app) fn goto_rows_for_test(&self, cx: &gpui::App) -> Vec<(&'static str, String, String)> {
        self.palette
            .goto
            .list
            .as_ref()
            .map(|list| list.read(cx).delegate().rows_detailed())
            .unwrap_or_default()
    }
}
