//! Editor-area panes: each pane is a tab strip over the content of its
//! active tab (editor / connection page / driver manager / diagram + cached
//! result view). The pane tree (`crate::panes`) decides the layout; this file
//! renders it, routes focus and dropped tabs, and implements the palette
//! actions. Result state is per tab (`TabView`), so panes share nothing and a
//! pane renders only its own active tab.

use super::*;
use crate::panes::{Edge, PaneId};
use crate::panes_view::{
    drop_overlay, drop_zone_at, neighbor_in_direction, render_tree, DropHover, ResizeFn,
};
use gpui_component::ElementExt as _;
use std::cell::RefCell;
use std::rc::Rc;

/// Painted bounds of every pane, captured at prepaint (shared with the
/// pane's `on_prepaint` closure) and read by directional focus.
pub(super) type PaneBounds = Rc<RefCell<Vec<(PaneId, gpui::Bounds<gpui::Pixels>)>>>;

#[cfg(test)]
thread_local! {
    /// How many times each pane view rendered (tests only).
    pub(super) static PANE_RENDERS: RefCell<std::collections::HashMap<u64, u64>> =
        RefCell::new(std::collections::HashMap::new());
}

/// One pane as a cached view: a notification of another pane (drag overlay,
/// strip hover, a tab changing there) or of the chrome replays this pane's
/// previous frame instead of rebuilding its strip and content. It holds no
/// state; `render` calls back into `AppRoot::render_pane` like `TabResults`.
pub(super) struct PaneView {
    root: gpui::WeakEntity<AppRoot>,
    pane: PaneId,
}

impl Render for PaneView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        PANE_RENDERS.with(|renders| *renders.borrow_mut().entry(self.pane.get()).or_default() += 1);
        match self.root.upgrade() {
            Some(root) => root.update(cx, |root, cx| root.render_pane(self.pane, window, cx)),
            None => div().into_any_element(),
        }
    }
}

impl AppRoot {
    /// The whole editor area: the pane tree with splitters. A maximized pane
    /// is rendered alone.
    pub(super) fn render_panes(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use gpui::Styled as _;
        self.workspace.normalize();
        self.measure_tab_texts(window, cx);
        let tree = self.workspace.panes();
        let ids: Vec<PaneId> = match tree.maximized() {
            Some(pane) => vec![pane],
            None => tree.panes().map(|pane| pane.id()).collect(),
        };
        // Views of removed panes go away; the bounds of panes that are not
        // drawn (maximize) are unknown.
        self.pane_views
            .retain(|pane, _| self.workspace.panes().pane(*pane).is_some());
        self.shell
            .pane_bounds
            .borrow_mut()
            .retain(|(pane, _)| ids.contains(pane));
        // Each pane is a cached view, handed to the tree layout by id.
        let mut rendered: Vec<(PaneId, Option<AnyElement>)> = ids
            .into_iter()
            .map(|pane| {
                let root = cx.entity().downgrade();
                let view = self
                    .pane_views
                    .entry(pane)
                    .or_insert_with(|| cx.new(|_| PaneView { root, pane }))
                    .clone();
                let element = view
                    .cached(
                        gpui::StyleRefinement::default()
                            .size_full()
                            .min_w_0()
                            .min_h_0(),
                    )
                    .into_any_element();
                (pane, Some(element))
            })
            .collect();
        let root = cx.entity().downgrade();
        let on_resize: ResizeFn = Rc::new(move |split, ratios, _, app| {
            let _ = root.update(app, |this, cx| {
                // Only the stored shares change; the panes keep their frame.
                if this.workspace.set_split_ratios(split, &ratios) {
                    this.notify_chrome(cx);
                }
            });
        });
        render_tree(
            self.workspace.panes(),
            &on_resize,
            window,
            cx,
            &mut |pane| {
                rendered
                    .iter_mut()
                    .find(|(id, _)| *id == pane.id())
                    .and_then(|(_, element)| element.take())
                    .unwrap_or_else(|| div().into_any_element())
            },
        )
    }

    /// Re-renders every pane view (strip hover while dragging, anything that
    /// is per pane) without touching the tabs' result views.
    pub(super) fn notify_pane_views(&mut self, cx: &mut Context<Self>) {
        let app: &mut gpui::App = cx;
        for view in self.pane_views.values() {
            app.notify(view.entity_id());
        }
    }

    /// Focus moves to the nearest pane in `direction` (no-op at the edge).
    pub(super) fn focus_pane_direction(&mut self, direction: Edge, cx: &mut Context<Self>) {
        let target = {
            let bounds = self.shell.pane_bounds.borrow();
            neighbor_in_direction(&bounds, self.workspace.focused_pane(), direction)
        };
        if let Some(target) = target {
            self.focus_pane_ui(target, cx);
        }
    }

    /// One pane: its tab strip above its active tab's content. A mouse press
    /// anywhere inside focuses the pane (capture phase, before the content
    /// handles it), and a dragged tab over the content shows the drop zone.
    pub(super) fn render_pane(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.workspace.normalize();
        let strip = self.render_tab_bar(pane, window, cx);
        let content = self
            .workspace
            .pane_active_tab(pane)
            .map(|tab| tab.id)
            .map(|tab_id| self.render_tab_content(tab_id, window, cx));
        let hover = self
            .shell
            .tab_bar
            .pane_drop
            .filter(|hover| hover.pane == pane);
        let strip_height = px(self.tab_strip_height(pane));
        let mut bounds = self.shell.tab_bar.pane_drop_bounds;
        if hover.is_some() && bounds.size.width <= px(0.0) {
            // A pinned zone (scenario) before any drag event: size it from the
            // pane's painted bounds, minus its strip.
            if let Some((_, painted)) = self
                .shell
                .pane_bounds
                .borrow()
                .iter()
                .find(|(id, _)| *id == pane)
            {
                bounds = *painted;
                bounds.origin.y += strip_height;
                bounds.size.height -= strip_height;
            }
        }
        let body = div()
            .id(("pane-content", pane.get()))
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .on_drag_move(cx.listener(
                move |this, event: &gpui::DragMoveEvent<tab_bar::TabDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx).id;
                    this.set_pane_drag_hover(pane, dragged, event.bounds, event.event.position, cx);
                },
            ))
            .on_drop(cx.listener(|this, drag: &tab_bar::TabDrag, _, cx| {
                this.drop_on_pane(drag.id, cx);
            }))
            .children(content)
            .when_some(hover, |body, hover| {
                body.child(drop_overlay(bounds, hover.zone, cx))
            });
        div()
            .id(("pane", pane.get()))
            .debug_selector(move || format!("pane-{}", pane.get()))
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(move |this, _, _, cx| {
                this.focus_pane_ui(pane, cx);
            }))
            .on_prepaint({
                let store = self.shell.pane_bounds.clone();
                move |bounds, _, _| {
                    let mut store = store.borrow_mut();
                    match store.iter_mut().find(|(id, _)| *id == pane) {
                        Some(entry) => entry.1 = bounds,
                        None => store.push((pane, bounds)),
                    }
                }
            })
            .child(strip)
            .child(body)
            .into_any_element()
    }

    /// Records the zone of `pane`'s content a dragged tab is over (only when
    /// dropping there would change something).
    fn set_pane_drag_hover(
        &mut self,
        pane: PaneId,
        dragged: u64,
        bounds: gpui::Bounds<gpui::Pixels>,
        position: gpui::Point<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hover = DropHover {
            pane,
            zone: drop_zone_at(bounds, position),
        };
        let next = self
            .workspace
            .can_drop_tab(dragged, hover.to_drop())
            .then_some(hover);
        let ui = &mut self.shell.tab_bar;
        // Only a change of zone re-renders (moving inside one zone is free).
        if ui.pane_drop != next || ui.hover.is_some() {
            ui.pane_drop = next;
            ui.pane_drop_bounds = bounds;
            ui.hover = None;
            self.notify_pane_views(cx);
        }
    }

    /// The content of a tab: SQL / script editor, connection page, driver
    /// manager, diagram, and the tab's cached result view.
    pub(super) fn render_tab_content(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return div().into_any_element();
        };
        let is_query = tab.kind == TabKind::Query;
        let is_mongo = tab.query_mode.is_mongo_find();
        let is_sql_table = is_query && tab.query_mode.is_table_data();
        let is_mongo_script = tab.query_mode.is_mongo_script();
        let is_aggregate = tab.query_mode.is_mongo_aggregate();
        let is_redis_key = tab.query_mode.is_redis_key();
        let is_redis_console = tab.query_mode.is_redis_command();
        let is_driver_manager = tab.kind == TabKind::DriverManager;
        let is_settings = tab.kind == TabKind::Settings;
        let is_diagram = tab.kind == TabKind::Diagram;
        let is_source = tab.kind == TabKind::Source;
        let import = (tab.kind == TabKind::Import).then(|| self.imports.get(&tab_id).cloned()).flatten();
        let transfer = (tab.kind == TabKind::Transfer).then(|| self.transfers.get(&tab_id).cloned()).flatten();
        let file_run = (tab.kind == TabKind::SqlFile).then(|| self.file_runs.get(&tab_id).cloned()).flatten();
        let backup = matches!(tab.kind, TabKind::Backup | TabKind::Restore).then(|| self.backups.get(&tab_id).cloned()).flatten();
        let accounts = (tab.kind == TabKind::Accounts).then(|| self.accounts.get(&tab_id).cloned()).flatten();
        let connection_tab = (tab.kind == TabKind::Connection).then(|| tab.clone());
        let layout = self.results_layout(tab_id);
        let editor_only = layout == results::surface::ResultsLayout::EditorOnly;

        let result_content = match layout {
            // An import tab draws its own preview grid inside the wizard.
            results::surface::ResultsLayout::None | results::surface::ResultsLayout::Preview => div().into_any_element(),
            // Only the (normally closed) history panel; no Results header,
            // body or footer before the first run.
            results::surface::ResultsLayout::EditorOnly => self.render_history_panel(tab_id, cx),
            _ => self.embed_tab_results(tab_id, cx),
        };

        // The SQL editor, created lazily and kept alive across tab switches.
        // The Save button opens the native save-name prompt wired to the real
        // saved-SQL product layer, so Query History can actually populate;
        // Cmd-S everywhere does the same.
        let has_editor = is_query && !is_mongo && !is_sql_table && !is_aggregate && !is_redis_key;
        let is_sql_editor_tab = has_editor && !is_mongo_script && !is_redis_console;
        let editor = has_editor.then(|| self.editor_for_tab(tab_id, window, cx));
        let mut script_toolbar = (is_mongo_script && editor.is_some())
            .then(|| self.render_mongo_script_toolbar(tab_id, window, cx));
        let editor_area = editor.map(|editor| {
            div()
                .flex()
                .flex_col()
                .bg(theme::background())
                .debug_selector(|| "query-editor-area".into())
                .when(editor_only, |this| this.flex_1().min_h_0())
                .children(script_toolbar.take())
                .when(!is_mongo_script, |this| {
                    this.child(
                        div()
                            .h(px(theme::HEADER_HEIGHT))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .bg(theme::chrome())
                            .border_b_1()
                            .border_color(theme::border())
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(div().flex_1().child(if is_redis_console {
                                "Redis Console"
                            } else {
                                "SQL Editor"
                            }))
                            .children(self.render_transaction_controls(tab_id, cx))
                            .child(self.render_editor_run_controls(tab_id, cx))
                            .when(!is_redis_console, |this| this.child(
                                Button::new(("run-sql-file", tab_id))
                                    .debug_selector(|| "run-sql-file".into())
                                    .icon(Icon::empty().path("icons/file-play.svg").size(px(12.0)))
                                    .compact()
                                    .ghost()
                                    .accessibility_label("Run SQL File")
                                    .tooltip("Run a .sql file of any size on this connection")
                                    .on_click(cx.listener(move |this, _, window, cx| this.run_sql_file_from_tab(Some(tab_id), window, cx))),
                            ).child(
                                Button::new("save-sql-script")
                                    .debug_selector(|| "save-sql-script".into())
                                    .label("Save")
                                    .compact()
                                    .ghost()
                                    .tooltip(shortcuts::with_label(
                                        "Save SQL script",
                                        &self.settings.shortcuts,
                                        "editor.save_script",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_active_sql(window, cx);
                                    })),
                            )),
                    )
                })
                .child(
                    // 160px beside the results; fills the panel until the
                    // first run. The same entity either way.
                    gpui_component::input::Editor::new(&editor)
                        .when(editor_only, |this| this.flex_1().min_h_0())
                        .when(!editor_only, |this| this.h(px(160.0)))
                        .appearance(true)
                        // The right-click menu of a SQL editor carries the assistant's actions
                        // (a native menu, built when it is opened: what the tab offers then).
                        .when(is_sql_editor_tab, |this| {
                            let (root, editor) = (cx.entity().downgrade(), editor.clone());
                            this.context_menu(move |_, _, cx| {
                                let asks = root
                                    .read_with(cx, |this, _| this.assistant_editor_asks())
                                    .unwrap_or_default();
                                // The component's own rules for its default menu.
                                let capabilities = editor.read(cx).context_menu_capabilities();
                                let editable =
                                    !capabilities.is_disabled() && !capabilities.is_readonly();
                                let editing = assistant::Editing {
                                    cut: editable && capabilities.is_copyable(),
                                    copy: capabilities.is_copyable(),
                                    paste: editable,
                                };
                                assistant::native_menu(assistant::editor_menu_rows(editing, asks))
                            })
                        }),
                )
                .into_any_element()
        });
        let connection = match connection_tab {
            Some(tab) => {
                self.sync_connection_overview(&tab, window, cx);
                self.render_connection_tab(&tab, cx)
            }
            None => div().into_any_element(),
        };
        let diagram = is_diagram.then(|| {
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(self.render_diagram_kind(
                    tab_id,
                    results::diagram::view::DiagramKind::Schema,
                    cx,
                ))
        });
        let source = is_source.then(|| self.render_source_tab(tab_id, window, cx));
        let import = import.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let transfer = transfer.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let file_run = file_run.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let backup = backup.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let accounts = accounts.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let driver_manager = if is_driver_manager {
            self.render_driver_manager(window, cx)
        } else {
            div().into_any_element()
        };
        let settings_page = if is_settings {
            self.render_settings(window, cx)
        } else {
            div().into_any_element()
        };
        // A `.sql` file dragged in from the OS: the container listens, not the editor or its rows.
        let drop_highlight = (self.file_drop_tab == Some(tab_id)).then(|| file_run::file_drop::highlight("Drop to open this SQL file", cx));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .when(is_sql_editor_tab, |this| {
                this.relative()
                    .on_drag_move(cx.listener(move |this, event: &gpui::DragMoveEvent<gpui::ExternalPaths>, _, cx| this.file_drag_over_tab(tab_id, event, cx)))
                    .on_file_drop_exit(cx.listener(|this, _, _, cx| this.file_drag_left(cx)))
                    .on_drop(cx.listener(move |this, paths: &gpui::ExternalPaths, window, cx| this.drop_files_on_tab(tab_id, paths, window, cx)))
            })
            .children(editor_area)
            .child(connection)
            .child(driver_manager)
            .child(settings_page)
            .children(diagram)
            .children(source)
            .children(import)
            .children(transfer)
            .children(file_run)
            .children(backup)
            .children(accounts)
            .child(result_content)
            .children(drop_highlight)
            .into_any_element()
    }

    // ---- palette actions ----------------------------------------------------

    /// "Split Editor Right / Down": moves the active tab into a new pane. A
    /// pane with a single tab cannot be split (it would leave an empty pane).
    pub(super) fn split_editor(
        &mut self,
        edge: crate::panes::Edge,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) else {
            return;
        };
        let can_split = self
            .workspace
            .panes()
            .focused_pane()
            .is_some_and(|pane| pane.tabs().len() > 1);
        if !can_split {
            window.push_notification("Open a second tab in this pane to split it", cx);
            return;
        }
        self.split_tab_ui(tab_id, edge, cx);
    }

    /// "Focus Next Pane".
    pub(super) fn focus_next_pane(&mut self, cx: &mut Context<Self>) {
        if self.workspace.focus_next_pane(1) {
            self.after_focus_change(cx);
            cx.notify();
        }
    }
}

impl AppRoot {
    /// Run / Cancel and History of a plain SQL editor tab, in its own header
    /// (they used to live in the window title bar and act on the active tab).
    fn render_editor_run_controls(&self, tab_id: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let can_run = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.query.can_run());
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                Button::new(("run-query", tab_id))
                    .debug_selector(|| "run-query".into())
                    .icon(Icon::empty().path("icons/play.svg").size(px(12.0)))
                    .label(if can_run { "Run" } else { "Cancel" })
                    .compact()
                    .ghost()
                    .tooltip(if can_run { "Run the statement" } else { "Cancel the running query" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if can_run {
                            this.run_tab_query_explicit(tab_id, cx);
                        } else {
                            this.cancel_tab_query(tab_id, cx);
                            cx.notify();
                        }
                    })),
            )
            .children(self.render_explain_button(tab_id, cx))
            .child(
                Button::new(("history", tab_id))
                    .debug_selector(|| "query-history".into())
                    .label("History")
                    .compact()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_history(tab_id, cx))),
            )
    }
}
