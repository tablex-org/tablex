//! Editor-area panes: each pane is a tab strip over the content of its
//! active tab (editor / connection page / driver manager / diagram + cached
//! result view). The pane tree (`crate::panes`) decides the layout; this file
//! renders it, routes focus and dropped tabs, and implements the palette
//! actions. Result state is per tab (`TabView`), so panes share nothing and a
//! pane renders only its own active tab.

use super::*;
use crate::panes::{Edge, PaneId};
use super::transaction::TxEnd;
use crate::transaction::CommitMode;
use crate::panes_view::{
    drop_overlay, drop_zone_at, neighbor_in_direction, render_tree, DropHover, ResizeFn,
};
use engine_plan::ExplainMode;
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
    /// How many times a SQL editor header was built (a test checks that an idle window builds none).
    pub(super) static HEADER_RENDERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        let is_error_log = tab.kind == TabKind::ErrorLog;
        let is_diagram = tab.kind == TabKind::Diagram;
        let is_source = tab.kind == TabKind::Source;
        let import = (tab.kind == TabKind::Import).then(|| self.imports.get(&tab_id).cloned()).flatten();
        let transfer = (tab.kind == TabKind::Transfer).then(|| self.transfers.get(&tab_id).cloned()).flatten();
        let file_run = (tab.kind == TabKind::SqlFile).then(|| self.file_runs.get(&tab_id).cloned()).flatten();
        let backup = matches!(tab.kind, TabKind::Backup | TabKind::Restore).then(|| self.backups.get(&tab_id).cloned()).flatten();
        let accounts = (tab.kind == TabKind::Accounts).then(|| self.accounts.get(&tab_id).cloned()).flatten();
        let process_list = (tab.kind == TabKind::ProcessList).then(|| self.process_lists.get(&tab_id).cloned()).flatten();
        let connection_tab = (tab.kind == TabKind::Connection).then(|| tab.clone());
        let layout = self.results_layout(tab_id);
        // An aggregate tab has no output before its first run; any tab can hide its results.
        let hidden = self.results_hidden(tab_id);
        let editor_only = layout == results::surface::ResultsLayout::EditorOnly
            || hidden
            || (is_aggregate && !self.aggregate_ran(tab_id));

        // Where the results go (see `results::placement`): beside the editor, or moved to a
        // Results tab / window, in which case this tab shows a note in their place.
        let place = if self.results_movable(tab_id) {
            self.results_place(tab_id)
        } else {
            results::placement::ResultsPlace::Below
        };
        // Whatever the layout: the results (and the history strip under them) show in one place only.
        let moved = !place.is_beside();
        let result_content = match layout {
            // A Results tab shows the results of its query tab.
            _ if tab.kind == TabKind::Results => match self.results_source(tab_id) {
                Some(source) => self.embed_tab_results(source, cx),
                None => div().into_any_element(),
            },
            // An import tab draws its own preview grid inside the wizard.
            results::surface::ResultsLayout::None | results::surface::ResultsLayout::Preview => div().into_any_element(),
            _ if moved => self.render_moved_results_note(tab_id, place, cx),
            // Hidden beside the editor: a strip says so and brings them back (the history panel stays).
            _ if hidden => div()
                .flex()
                .flex_col()
                .child(self.render_hidden_results_note(tab_id, cx))
                .child(self.render_history_panel(tab_id, cx))
                .into_any_element(),
            _ if is_aggregate && editor_only => div().into_any_element(),
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
        if has_editor {
            self.vim_sync(tab_id);
        }
        let mut script_toolbar = (is_mongo_script && editor.is_some())
            .then(|| self.render_mongo_script_toolbar(tab_id, window, cx));
        let mut header = (editor.is_some() && !is_mongo_script).then(|| self.render_sql_editor_header(tab_id, is_redis_console, cx));
        let status_language = editor_status::EditorLanguage::of(self.workspace.tab(tab_id).and_then(|tab| tab.query_mode.as_ref()));
        // Outside Insert mode a Vim editor refuses text that does not come as key presses (IME, dead keys).
        let read_only = self.vim_readonly(tab_id);
        if has_editor {
            self.vim_rendered(tab_id);
        }
        let editor_area = editor.map(|editor| {
            div()
                .flex()
                .flex_col()
                .bg(theme::background())
                .debug_selector(|| "query-editor-area".into())
                .flex_1()
                .min_h_0()
                .children(script_toolbar.take())
                .when(!is_mongo_script, |this| this.child(header.take().expect("built for a SQL editor")))
                .child(
                    // The editor box: the code and, inside the same frame, its status line.
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .bg(editor_status::editor_background(cx))
                        .rounded(gpui_component::theme::Theme::global(cx).radius)
                        .border_1()
                        .border_color(theme::border())
                        // Object rows dragged from the sidebar insert their names at the drop point
                        // (SQL and MongoDB script editors; a Redis console takes commands, not names).
                        .when(!is_redis_console, |this| {
                            this.drag_over::<navigator_ui::ObjectDrag>(|style, _, _, _| style.border_color(theme::accent()))
                                .on_drop(cx.listener(move |this, drag: &navigator_ui::ObjectDrag, window, cx| {
                                    let at = window.mouse_position();
                                    this.drop_objects_on_editor(tab_id, &drag.ids, at, window, cx);
                                }))
                        })
                        .child(
                    // Fills its area: the whole panel until the first run, then the upper part
                    // of the editor / results split. The same entity either way.
                    self.vim_wrap(tab_id, gpui_component::input::Editor::new(&editor)
                        .flex_1()
                        .min_h_0()
                        .appearance(false)
                        .readonly(read_only)
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
                        })
                        .into_any_element(), cx),
                )
                        .child(self.render_editor_status(tab_id, &editor, status_language, true, cx)),
                )
                .into_any_element()
        });
        // The aggregate tab's editor is its pipeline; its output is the result view.
        let editor_area = editor_area.or_else(|| is_aggregate.then(|| self.render_aggregate_editor(tab_id, window, cx)));
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
        let process_list = process_list.map(|view| div().flex_1().min_h_0().min_w_0().child(view));
        let error_log = is_error_log.then(|| self.render_error_log_tab(window, cx));
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
        // Beside the results, the editor and the results share the height through a splitter
        // (as the aggregate tab's do): the editor starts at its old 160px, and a short result
        // can give the editor the room it does not need.
        // Results moved to a tab or a window leave the editor the whole panel, with the note under
        // it. Results to the right of the editor split the panel side by side.
        let (editor_area, split, result_content) = match editor_area {
            Some(editor_area) if !editor_only && !moved => {
                let fill = |content: AnyElement| div().size_full().flex().flex_col().child(content);
                // The aggregate editor (header + pipeline) starts taller than a SQL editor.
                let (below_id, right_id, start, least) = if is_aggregate {
                    ("agg-split", "agg-split-right", 430.0, 260.0)
                } else {
                    ("editor-results-split", "editor-results-split-right", theme::HEADER_HEIGHT + 160.0, theme::HEADER_HEIGHT + 60.0)
                };
                let split = if place == results::placement::ResultsPlace::Right {
                    gpui_component::resizable::h_resizable((right_id, tab_id))
                        .child(
                            // Neither side sized: they start at half the width each.
                            gpui_component::resizable::resizable_panel()
                                .size_range(px(280.0)..gpui::Pixels::MAX)
                                .child(fill(editor_area)),
                        )
                        .child(
                            gpui_component::resizable::resizable_panel()
                                .size_range(px(320.0)..gpui::Pixels::MAX)
                                .child(fill(result_content)),
                        )
                } else {
                    gpui_component::resizable::v_resizable((below_id, tab_id))
                        .child(
                            gpui_component::resizable::resizable_panel()
                                .size(px(start))
                                .size_range(px(least)..gpui::Pixels::MAX)
                                .child(fill(editor_area)),
                        )
                        .child(
                            gpui_component::resizable::resizable_panel()
                                .size_range(px(120.0)..gpui::Pixels::MAX)
                                .child(fill(result_content)),
                        )
                };
                let split = div()
                    .debug_selector(|| "editor-results-split".into())
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .child(split)
                    .into_any_element();
                (None, Some(split), None)
            }
            editor_area => (editor_area, None, Some(result_content)),
        };
        // A `.sql` file dragged in from the OS: the container listens, not the editor or its rows.
        let drop_highlight = (self.file_drop_tab == Some(tab_id)).then(|| file_run::file_drop::highlight(t!("editor.drop_to_open_this_sql_file"), cx));
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
            .children(split)
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
            .children(process_list)
            .children(error_log)
            .children(result_content)
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
            window.push_notification(t!("editor.open_a_second_tab_in_this_pane_to_split_it"), cx);
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

/// How much of a SQL editor's header fits in the width its panel has: the controls are drawn in
/// full while they fit; below that the secondary ones move into a "..." menu (Run stays). It is
/// worked out from what was painted: the header's width, and what its controls need in full (it
/// depends on the language, the commit mode and the Run / Cancel label, so it is measured, not guessed). The transaction's
/// state text gives way before anything moves: it is not counted in what the controls need, and
/// is given the room that is left (`HeaderFit::room`). Shared with the paint callbacks of the header,
/// which draw the panes again, in a frame of their own, when the fit changes (`AppRoot::header_probe`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct HeaderFit {
    /// The header's painted width.
    width: f32,
    /// The controls row's painted width, and the parts of it that give way (the state text of a
    /// transaction and its hint): what is left is what the controls need.
    row: f32,
    status: f32,
    hint: f32,
    /// What the controls need drawn in full (the row without the parts that give way).
    natural: Option<f32>,
    /// The same for a header drawn collapsed (the controls that stay): the state text is given what is left of it.
    natural_collapsed: Option<f32>,
    /// What `natural` was measured for (language, commit mode, whether a statement runs): a change measures again.
    key: u64,
    /// Whether the header was last built with the secondary controls in the menu.
    drawn_collapsed: bool,
    pub(super) collapsed: bool,
}

pub(super) type HeaderFits = std::rc::Rc<RefCell<HashMap<u64, HeaderFit>>>;

impl HeaderFit {
    /// The room the transaction's state text and its hint have in a header drawn in full, when it is
    /// known: what the fixed controls leave of the width. (A collapsed header has the state text only.)
    pub(super) fn status_width(&self) -> f32 {
        self.status
    }

    pub(super) fn room(&self) -> Option<f32> {
        if self.width <= 0.0 {
            return None;
        }
        let natural = if self.collapsed { self.natural_collapsed? } else { self.natural? };
        Some((self.width - Self::ROOM - Self::TITLE - natural).max(0.0))
    }
}

/// What a paint callback of the header measures.
#[derive(Clone, Copy)]
pub(super) enum HeaderPart {
    Header,
    Row,
    Status,
    Hint,
}

impl HeaderFit {
    /// The header's padding and the gap between its title and its controls.
    const ROOM: f32 = 12.0 + 12.0 + 8.0;
    /// What the title is given before the controls collapse or the state text is cut (a few characters and an ellipsis:
    /// "Trình s…" beside the commit-mode button at 800 px).
    const TITLE: f32 = 48.0;
    /// The least it keeps: nothing. In a header too tight for it the controls (the commit mode, Run and the
    /// "..." menu) are whole and the title goes.
    const TITLE_MIN: f32 = 0.0;

    /// Records a measure; whether the header has to be drawn again: when what is collapsed changed, or the
    /// room the state text is given did (by more than half a pixel: steady state asks for nothing).
    fn measured(fits: &HeaderFits, tab_id: u64, part: HeaderPart, value: f32) -> bool {
        let mut fits = fits.borrow_mut();
        let fit = fits.entry(tab_id).or_default();
        let (before, room_before) = (fit.collapsed, fit.room());
        match part {
            // The state text is painted before the row that holds it: these only note their width.
            HeaderPart::Status => {
                fit.status = value;
                return false;
            }
            HeaderPart::Hint => {
                fit.hint = value;
                return false;
            }
            HeaderPart::Row => {
                fit.row = value;
                // Only a row drawn in full says what the full controls need.
                let needs = Some((fit.row - fit.status - fit.hint).max(0.0));
                if fit.drawn_collapsed {
                    fit.natural_collapsed = needs;
                } else {
                    fit.natural = needs;
                }
            }
            HeaderPart::Header => fit.width = value,
        }
        fit.collapsed = fit.natural.is_some_and(|natural| fit.width > 0.0 && fit.width < natural + Self::ROOM + Self::TITLE);
        // Only a header that draws the state text has a use for its room.
        let room_changed = fit.status > 0.0
            && match (room_before, fit.room()) {
                (Some(before), Some(now)) => (before - now).abs() > 0.5,
                (before, now) => before.is_some() != now.is_some(),
            };
        fit.collapsed != before || room_changed
    }
}

impl AppRoot {
    /// A paint callback that records `part` of the header of `tab_id` and draws the panes again
    /// when that changes how much of the header fits. A notification made while painting is not a
    /// request for a frame (the window is not dirty afterwards, so a real window waits for the next
    /// input), so the panes are notified from a next-frame callback, which does ask for one.
    pub(super) fn header_probe(&self, tab_id: u64, part: HeaderPart, cx: &mut Context<Self>) -> impl FnOnce(gpui::Bounds<gpui::Pixels>, &mut Window, &mut gpui::App) + 'static {
        let (fits, root) = (self.query_editors.header_fit.clone(), cx.entity().downgrade());
        move |bounds, window, _| {
            if HeaderFit::measured(&fits, tab_id, part, f32::from(bounds.size.width)) {
                window.on_next_frame(move |_, app| {
                    let _ = root.update(app, |this, cx| this.notify_pane_views(cx));
                });
            }
        }
    }

    /// The header of a SQL editor / Redis console: title, commit mode, Run and the secondary
    /// buttons. It never draws outside its panel: the title gives way first (it is cut), then the
    /// secondary buttons move into a "..." menu.
    pub(super) fn render_sql_editor_header(&mut self, tab_id: u64, redis: bool, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        HEADER_RENDERS.with(|renders| renders.set(renders.get() + 1));
        // The Run button reads "Cancel" while a statement runs: another width, so another measure.
        let running = !self.workspace.runtime(tab_id).is_some_and(|runtime| runtime.query.can_run());
        let key = u64::from(self.is_transactional_tab(tab_id) && self.commit_mode(tab_id) == CommitMode::Manual)
            | (u64::from(crate::i18n::current() == crate::i18n::Locale::Vi) << 1)
            | (u64::from(running) << 2);
        let collapsed = {
            let mut fits = self.query_editors.header_fit.borrow_mut();
            let fit = fits.entry(tab_id).or_default();
            if fit.key != key {
                *fit = HeaderFit { key, width: fit.width, ..HeaderFit::default() };
            }
            // What is painted this frame is measured against this layout; the state text is measured again.
            fit.drawn_collapsed = fit.collapsed;
            (fit.status, fit.hint) = (0.0, 0.0);
            fit.collapsed
        };
        let title = if redis { t!("editor.redis_console") } else { t!("editor.sql_editor") };
        div()
            .debug_selector(|| "editor-header".into())
            .relative()
            .h(px(theme::HEADER_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .overflow_hidden()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            // The title gives way first when the header is tight; the spacer keeps the controls right.
            .child(
                div()
                    .debug_selector(|| "editor-header-title".into())
                    .flex_1()
                    .min_w(px(HeaderFit::TITLE_MIN))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                // Drawn in full the controls keep their size (that is what is measured) and the
                // transaction's state text is given the room that is left.
                div()
                    .debug_selector(|| "editor-header-controls".into())
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    // Collapsed, the row is clipped to the header and the state text gives way in it (its
                    // Commit and Roll Back are in the menu too).
                    .map(|row| if collapsed { row.flex_shrink(1.0).min_w_0().overflow_hidden() } else { row.flex_shrink_0() })
                    .children(self.render_transaction_controls(tab_id, collapsed, cx))
                    .child(self.render_editor_run_controls(tab_id, collapsed, cx))
                    .when(!redis && !collapsed, |this| {
                        this.child(
                            Button::new(("run-sql-file", tab_id))
                                .small()
                                .flex_shrink_0()
                                .debug_selector(|| "run-sql-file".into())
                                .icon(Icon::empty().path("icons/file-play.svg").size(px(12.0)))
                                .compact()
                                .ghost()
                                .accessibility_label(t!("editor.run_sql_file"))
                                .tooltip(t!("editor.run_a_sql_file_of_any_size_on_this_connection"))
                                .on_click(cx.listener(move |this, _, window, cx| this.run_sql_file_from_tab(Some(tab_id), window, cx))),
                        )
                        .child(
                            Button::new("save-sql-script")
                                .small()
                                .flex_shrink_0()
                                .debug_selector(|| "save-sql-script".into())
                                .label(t!("editor.save"))
                                .compact()
                                .ghost()
                                .tooltip(shortcuts::with_label(
                                    &t!("editor.save_sql_script"),
                                    &self.settings.shortcuts,
                                    "editor.save_script",
                                ))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.save_active_sql(window, cx);
                                })),
                        )
                    })
                    .when(collapsed, |this| this.child(self.render_editor_more_menu(tab_id, redis, cx)))
                    // Last of the row's children: painted after the state text it counts.
                    .on_prepaint(self.header_probe(tab_id, HeaderPart::Row, cx)),
            )
            // Last of the header's children: painted after the row.
            .on_prepaint(self.header_probe(tab_id, HeaderPart::Header, cx))
            .into_any_element()
    }

    /// The "..." menu of a collapsed header: what the buttons that no longer fit do.
    fn render_editor_more_menu(&self, tab_id: u64, redis: bool, cx: &mut Context<Self>) -> AnyElement {
        let can_run = self.workspace.runtime(tab_id).is_some_and(|runtime| runtime.query.can_run());
        let explain = self.explain_dialect_of(tab_id).is_some();
        // In the commit mode: whether a transaction is open, and the tab can end it now.
        let manual = (self.is_transactional_tab(tab_id) && self.commit_mode(tab_id) == CommitMode::Manual)
            .then(|| (self.tab_tx(tab_id).is_some_and(|tx| tx.is_open()), !self.tab_busy(tab_id)));
        let root = cx.entity().downgrade();
        Button::new(("editor-more", tab_id))
            .small()
            .debug_selector(|| "editor-more".into())
            .icon(Icon::new(IconName::Ellipsis).size(px(12.0)))
            .compact()
            .ghost()
            .tooltip(t!("editor.more_actions"))
            .dropdown_menu(move |menu, _, cx| {
                let item = |selector: &'static str, label: String| {
                    PopupMenuItem::element(move |_, _| div().id(selector).debug_selector(move || selector.to_string()).child(label.clone()))
                };
                let mut menu = menu;
                if explain {
                    let block = root.read_with(&*cx, |this, cx| this.explain_analyze_block(tab_id, cx)).ok().flatten();
                    let (estimate, analyze) = (root.clone(), root.clone());
                    menu = menu
                        .item(item("more-explain", t!("editor.explain").into_owned()).disabled(!can_run).on_click(move |_, _, app| {
                            let _ = estimate.update(app, |this, cx| this.explain_tab_query(tab_id, ExplainMode::Estimate, cx));
                        }))
                        .item(
                            item("more-explain-analyze", t!("editor.explain_analyze").into_owned())
                                .disabled(!can_run || block.is_some())
                                .on_click(move |_, _, app| {
                                    let _ = analyze.update(app, |this, cx| this.explain_tab_query(tab_id, ExplainMode::Analyze, cx));
                                }),
                        );
                }
                // The Commit and Roll Back buttons of the commit mode may be clipped in a narrow header.
                if let Some((open, idle)) = manual {
                    for (selector, label, end) in [
                        ("more-commit", t!("editor.commit").into_owned(), TxEnd::Commit),
                        ("more-rollback", t!("editor.roll_back").into_owned(), TxEnd::Rollback),
                    ] {
                        let root = root.clone();
                        menu = menu.item(item(selector, label).disabled(!open || !idle).on_click(move |_, window, app| {
                            let _ = root.update(app, |this, cx| this.end_tab_transaction(tab_id, end, window, cx));
                        }));
                    }
                }
                let history = root.clone();
                menu = menu.item(item("more-history", t!("editor.history").into_owned()).on_click(move |_, _, app| {
                    let _ = history.update(app, |this, cx| this.toggle_history(tab_id, cx));
                }));
                if !redis {
                    let (file, save) = (root.clone(), root.clone());
                    menu = menu
                        .item(item("more-run-sql-file", t!("editor.run_sql_file").into_owned()).on_click(move |_, window, app| {
                            let _ = file.update(app, |this, cx| this.run_sql_file_from_tab(Some(tab_id), window, cx));
                        }))
                        .item(item("more-save", t!("editor.save").into_owned()).on_click(move |_, window, app| {
                            let _ = save.update(app, |this, cx| this.save_active_sql(window, cx));
                        }));
                }
                menu
            })
            .into_any_element()
    }

    /// Run / Cancel (and, when the header has the room, Explain and History) of a plain SQL
    /// editor tab, in its own header.
    fn render_editor_run_controls(&self, tab_id: u64, collapsed: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let can_run = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.query.can_run());
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.0))
            .child(
                Button::new(("run-query", tab_id))
                    .small()
                    .debug_selector(|| "run-query".into())
                    .icon(Icon::empty().path("icons/play.svg").size(px(12.0)))
                    .label(if can_run { t!("editor.run") } else { t!("common.cancel") })
                    .compact()
                    .ghost()
                    .tooltip(if can_run { t!("editor.run_the_statement") } else { t!("editor.cancel_the_running_query") })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if can_run {
                            this.run_tab_query_explicit(tab_id, cx);
                        } else {
                            this.cancel_tab_query(tab_id, cx);
                            cx.notify();
                        }
                    })),
            )
            .when(!collapsed, |this| {
                this.children(self.render_explain_button(tab_id, cx)).child(
                    Button::new(("history", tab_id))
                        .small()
                        .debug_selector(|| "query-history".into())
                        .label(t!("editor.history"))
                        .compact()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_history(tab_id, cx))),
                )
            })
    }
}
