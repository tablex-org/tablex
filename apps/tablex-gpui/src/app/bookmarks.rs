//! Bookmarks: "Add Bookmark…" on a database, schema, table, view, function or MongoDB collection row keeps it in the Bookmarks
//! section at the top of the sidebar tree (hidden while there is none, collapsible, kept in the sidebar preferences so it survives
//! a restart). They are not pins: a pin only moves a row to the top of its own level.
//!
//! Opening a bookmark (double-click, Enter, the row's menu, or "Go to Bookmark" in the palette) reveals its object in the tree and
//! opens it the way Go to Object does (`goto_object_node`: connecting first, loading the levels on the way), so it behaves like a
//! choice there. A bookmark whose connection was deleted is dropped with the connection (`SidebarPreferences::remove_connection`);
//! one whose object is gone leaves the "not found" message of the locate in the sidebar and stays until removed.

use gpui::WeakEntity;
use gpui_component::dialog::DialogFooter;
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::menu::PopupMenu;
use gpui_component::IndexPath;

use super::mongo_admin_view::{entry_item, Entry};
use super::*;
use crate::sidebar_preferences::{Bookmark, BookmarkTarget};

/// What the name dialog is for.
#[derive(Clone)]
enum Flow {
    Add(BookmarkTarget),
    Rename(String),
}

impl AppRoot {
    /// What a bookmark on the row `id` would point at, and the name it gets by default (the object's own).
    pub(super) fn bookmark_target_of(&self, id: &NavigatorNodeId) -> Option<(BookmarkTarget, String)> {
        Some(match id {
            NavigatorNodeId::Database { connection, database } => {
                (BookmarkTarget::Database { connection: connection.clone(), database: database.clone() }, database.clone())
            }
            NavigatorNodeId::Schema { connection, database, schema } => (
                BookmarkTarget::Schema { connection: connection.clone(), database: database.clone(), schema: schema.clone() },
                schema.clone(),
            ),
            NavigatorNodeId::Object { connection, database, schema, object } => (
                BookmarkTarget::Object { connection: connection.clone(), database: database.clone(), schema: schema.clone(), object: object.clone() },
                object.clone(),
            ),
            NavigatorNodeId::SchemaObject { connection, database, schema, group, object } => {
                let name = self.navigator.schema_object(id).map_or_else(|| object.clone(), |found| found.name.clone());
                (
                    BookmarkTarget::Routine { connection: connection.clone(), database: database.clone(), schema: schema.clone(), group: *group, object: object.clone() },
                    name,
                )
            }
            NavigatorNodeId::Provider { .. } => {
                let target = self.mongo_node_target(id)?;
                match target.collection {
                    Some(collection) => (
                        BookmarkTarget::Collection { connection: target.connection, database: target.database, collection: collection.clone() },
                        collection,
                    ),
                    None => (BookmarkTarget::Database { connection: target.connection, database: target.database.clone() }, target.database),
                }
            }
            _ => return None,
        })
    }

    /// "Add Bookmark…": asks for a name (the object's own is filled in) and keeps the bookmark.
    pub(super) fn open_add_bookmark_dialog(&mut self, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((target, name)) = self.bookmark_target_of(id) {
            self.open_bookmark_name_dialog(Flow::Add(target), name, window, cx);
        }
    }

    /// Rename… of a bookmark.
    pub(super) fn open_rename_bookmark_dialog(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.navigator.preferences().bookmark(id).map(|bookmark| bookmark.name.clone()) {
            self.open_bookmark_name_dialog(Flow::Rename(id.to_owned()), name, window, cx);
        }
    }

    fn open_bookmark_name_dialog(&mut self, flow: Flow, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(name).placeholder(t!("menu.bookmark_name_placeholder")));
        let focus = input.clone();
        let root = cx.entity().downgrade();
        let title = match flow {
            Flow::Add(_) => t!("menu.add_bookmark_title"),
            Flow::Rename(_) => t!("menu.rename_bookmark_title"),
        };
        let submit = move |root: &WeakEntity<AppRoot>, flow: &Flow, input: &gpui::Entity<InputState>, cx: &mut gpui::App| {
            let name = input.read(cx).value().to_string();
            let _ = root.update(cx, |this, cx| this.save_bookmark_name(flow, &name, cx));
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let (enter_root, enter_flow, enter_input) = (root.clone(), flow.clone(), input.clone());
            let (save_root, save_flow, save_input) = (root.clone(), flow.clone(), input.clone());
            dialog
                .title(title.clone())
                .w(px(380.0))
                .child(div().debug_selector(|| "bookmark-name".into()).child(Input::new(&input)))
                // Enter saves and closes.
                .on_ok(move |_, _, cx| {
                    submit(&enter_root, &enter_flow, &enter_input, cx);
                    true
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("bookmark-cancel")
                                .label(t!("common.cancel"))
                                .compact()
                                .ghost()
                                .debug_selector(|| "bookmark-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("bookmark-save")
                                .label(t!("menu.bookmark_save"))
                                .compact()
                                .primary()
                                .debug_selector(|| "bookmark-save".into())
                                .on_click(move |_, window, cx| {
                                    submit(&save_root, &save_flow, &save_input, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
        focus.update(cx, |state, cx| state.focus(window, cx));
    }

    fn save_bookmark_name(&mut self, flow: &Flow, name: &str, cx: &mut Context<Self>) {
        match flow {
            Flow::Add(target) => {
                let _ = self.add_bookmark(target.clone(), name, cx);
            }
            Flow::Rename(id) => {
                let _ = self.change_sidebar_preferences(|prefs| prefs.rename_bookmark(id, name), cx);
            }
        }
    }

    /// Keeps a bookmark of `target`; an empty `name` is the object's own. Returns its id. A failed save says so once, in the sidebar's
    /// error line.
    pub(super) fn add_bookmark(&mut self, target: BookmarkTarget, name: &str, cx: &mut Context<Self>) -> Result<String, String> {
        let name = Some(name.trim()).filter(|name| !name.is_empty()).map(str::to_owned).unwrap_or_else(|| bookmark_default_name(&target));
        let id = uuid::Uuid::new_v4().to_string();
        let bookmark = Bookmark { id: id.clone(), name, target };
        self.change_sidebar_preferences(|prefs| prefs.add_bookmark(bookmark), cx)?;
        self.navigator_ui.generation = self.navigator.generation();
        Ok(id)
    }

    pub(super) fn remove_bookmark(&mut self, id: &str, cx: &mut Context<Self>) {
        let _ = self.change_sidebar_preferences(
            |prefs| {
                prefs.remove_bookmark(id);
                Ok(())
            },
            cx,
        );
    }

    /// Reveals the bookmarked object in the tree and opens it (connecting first when its connection is not open).
    pub(super) fn activate_bookmark(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(bookmark) = self.navigator.preferences().bookmark(id).cloned() else { return };
        let connection = bookmark.target.connection().to_owned();
        let Some(kind) = self.saved_connections.iter().find(|saved| saved.matches_id(&connection)).map(|saved| saved.kind) else {
            self.sidebar.notice = Some(t!("sidebar.bookmark_unavailable").into_owned());
            return cx.notify();
        };
        let on_connection = || NavigatorNodeId::Connection { connection: connection.clone() };
        match bookmark.target {
            BookmarkTarget::Database { database, .. } if kind == ConnectionKind::Mongodb => {
                if let Err(error) = self.locate_and_open_in_sidebar(vec![on_connection()], Some((database, String::new())), cx) {
                    self.sidebar.locate_error = Some(error);
                }
            }
            BookmarkTarget::Collection { database, collection, .. } => {
                if let Err(error) = self.locate_and_open_in_sidebar(vec![on_connection()], Some((database, collection)), cx) {
                    self.sidebar.locate_error = Some(error);
                }
            }
            BookmarkTarget::Database { connection, database } => self.goto_object_node(&NavigatorNodeId::Database { connection, database }, cx),
            BookmarkTarget::Schema { connection, database, schema } => self.goto_object_node(&NavigatorNodeId::Schema { connection, database, schema }, cx),
            BookmarkTarget::Object { connection, database, schema, object } => self.goto_object_node(&NavigatorNodeId::Object { connection, database, schema, object }, cx),
            BookmarkTarget::Routine { connection, database, schema, group, object } => {
                self.goto_object_node(&NavigatorNodeId::SchemaObject { connection, database, schema, group, object }, cx)
            }
        }
        self.notify_chrome(cx);
    }

    /// "Go to Bookmark" of the palette: a list of the bookmarks to filter by typing; Enter opens the one chosen.
    pub(super) fn open_go_to_bookmark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let known: std::collections::HashSet<String> = self.saved_connections.iter().map(|saved| saved.id.to_string()).collect();
        let items: Vec<PickItem> = self
            .navigator
            .preferences()
            .bookmarks
            .iter()
            .filter(|bookmark| known.contains(bookmark.target.connection()))
            .map(|bookmark| PickItem {
                id: bookmark.id.clone(),
                name: bookmark.name.clone().into(),
                detail: self.bookmark_detail(&bookmark.target).into(),
                lower: bookmark.name.to_lowercase(),
            })
            .collect();
        if items.is_empty() {
            self.sidebar.notice = Some(t!("sidebar.no_bookmarks").into_owned());
            return cx.notify();
        }
        let root = cx.entity().downgrade();
        let list = cx.new(|cx| ListState::new(BookmarkPicker::new(root, items), window, cx).searchable(true));
        let dialog_list = list.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let list = dialog_list.clone();
            dialog.p_0().w(px(480.0)).close_button(false).content(move |content, _, cx| {
                let height = list.read(cx).delegate().panel_height();
                content.child(
                    div()
                        .debug_selector(|| "bookmark-pick".into())
                        .w_full()
                        .h(px(height))
                        .child(gpui_component::list::List::new(&list).search_placeholder(t!("menu.go_to_bookmark_placeholder"))),
                )
            })
        });
        list.update(cx, |state, cx| {
            state.set_selected_index(Some(IndexPath::default()), window, cx);
            state.focus(window, cx);
        });
    }

    /// `connection · path`: what the picker shows beside a bookmark's name.
    fn bookmark_detail(&self, target: &BookmarkTarget) -> String {
        let connection = target.connection();
        let name = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map_or(connection, |saved| saved.name.as_str());
        let path = match target {
            BookmarkTarget::Database { database, .. } => database.clone(),
            BookmarkTarget::Schema { database, schema, .. } => format!("{database}.{schema}"),
            BookmarkTarget::Object { database, schema, object, .. } => format!("{database}.{schema}.{object}"),
            BookmarkTarget::Routine { database, schema, .. } => format!("{database}.{schema}"),
            BookmarkTarget::Collection { database, collection, .. } => format!("{database}.{collection}"),
        };
        format!("{name} \u{00B7} {path}")
    }
}

/// The name a bookmark gets when none is typed.
fn bookmark_default_name(target: &BookmarkTarget) -> String {
    match target {
        BookmarkTarget::Database { database, .. } => database.clone(),
        BookmarkTarget::Schema { schema, .. } => schema.clone(),
        BookmarkTarget::Object { object, .. } | BookmarkTarget::Routine { object, .. } => object.clone(),
        BookmarkTarget::Collection { collection, .. } => collection.clone(),
    }
}

/// "Add Bookmark…" in the menu of the row `id`, when it can be bookmarked (a function's, a MongoDB collection's or database's menu).
pub(super) fn append_bookmark_item(menu: PopupMenu, root: WeakEntity<AppRoot>, id: &NavigatorNodeId, cx: &mut gpui::App) -> PopupMenu {
    if !root.update(cx, |this, _| this.bookmark_target_of(id).is_some()).unwrap_or(false) {
        return menu;
    }
    let id = id.clone();
    menu.item(entry_item(
        Entry {
            selector: "sidebar-menu-add-bookmark",
            label: t!("menu.add_bookmark"),
            icon: Icon::empty().path("icons/bookmark.svg").size(px(12.0)),
            disabled: None,
            destructive: false,
        },
        move |window, app| {
            let _ = root.update(app, |this, cx| this.open_add_bookmark_dialog(&id, window, cx));
        },
    ))
}

/// The menu of a bookmark row: Open, Rename…, Remove.
pub(super) fn build_bookmark_menu(menu: PopupMenu, root: WeakEntity<AppRoot>, id: String) -> PopupMenu {
    let entry = |selector: &'static str, label: std::borrow::Cow<'static, str>, icon: &'static str, destructive: bool| Entry {
        selector,
        label,
        icon: Icon::empty().path(icon).size(px(12.0)),
        disabled: None,
        destructive,
    };
    let (open_root, open_id) = (root.clone(), id.clone());
    let (rename_root, rename_id) = (root.clone(), id.clone());
    menu.item(entry_item(entry("bookmark-menu-open", t!("sidebar.open"), "icons/bookmark.svg", false), move |_, app| {
        let _ = open_root.update(app, |this, cx| this.activate_bookmark(&open_id, cx));
    }))
    .item(entry_item(entry("bookmark-menu-rename", t!("menu.rename_bookmark"), "icons/pencil.svg", false), move |window, app| {
        let _ = rename_root.update(app, |this, cx| this.open_rename_bookmark_dialog(&rename_id, window, cx));
    }))
    .separator()
    .item(entry_item(entry("bookmark-menu-remove", t!("menu.remove_bookmark"), "icons/delete.svg", true), move |_, app| {
        let _ = root.update(app, |this, cx| this.remove_bookmark(&id, cx));
    }))
}

/// One row of the "Go to Bookmark" list.
struct PickItem {
    id: String,
    name: gpui::SharedString,
    detail: gpui::SharedString,
    /// The name, lowercase: what the typed text is looked for in.
    lower: String,
}

/// The list "Go to Bookmark" shows: the bookmarks whose name holds what was typed, in the order they were made.
struct BookmarkPicker {
    root: WeakEntity<AppRoot>,
    items: Vec<PickItem>,
    shown: Vec<usize>,
    selected: Option<IndexPath>,
}

const PICK_ROW_HEIGHT: f32 = 32.0;
const PICK_INPUT_HEIGHT: f32 = 40.0;
const PICK_MAX_ROWS: usize = 10;

impl BookmarkPicker {
    fn new(root: WeakEntity<AppRoot>, items: Vec<PickItem>) -> Self {
        let shown = (0..items.len()).collect();
        Self { root, items, shown, selected: Some(IndexPath::default()) }
    }

    fn panel_height(&self) -> f32 {
        PICK_INPUT_HEIGHT + PICK_ROW_HEIGHT * self.shown.len().clamp(1, PICK_MAX_ROWS) as f32 + 8.0
    }
}

impl ListDelegate for BookmarkPicker {
    type Item = ListItem;

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut Context<ListState<Self>>) -> gpui::Task<()> {
        let needle = query.trim().to_lowercase();
        self.shown = (0..self.items.len()).filter(|&index| needle.is_empty() || self.items[index].lower.contains(&needle)).collect();
        self.selected = (!self.shown.is_empty()).then(IndexPath::default);
        gpui::Task::ready(())
    }

    fn items_count(&self, _: usize, _: &gpui::App) -> usize {
        self.shown.len()
    }

    fn render_item(&mut self, ix: IndexPath, _: &mut Window, _: &mut Context<ListState<Self>>) -> Option<ListItem> {
        let item = self.items.get(*self.shown.get(ix.row)?)?;
        let row = ix.row;
        Some(
            ListItem::new(("bookmark-pick-row", row as u64))
                .debug_selector(move || format!("bookmark-pick-row-{row}"))
                .h(px(PICK_ROW_HEIGHT))
                .py_0()
                .px_3()
                .rounded(px(6.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(13.0))
                        .text_color(theme::text())
                        .child(Icon::empty().path("icons/bookmark.svg").size(px(14.0)).text_color(theme::text_muted()))
                        .child(div().flex_none().max_w(px(220.0)).overflow_hidden().whitespace_nowrap().text_ellipsis().child(item.name.clone()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(item.detail.clone()),
                        ),
                ),
        )
    }

    fn render_empty(&mut self, _: &mut Window, _: &mut Context<ListState<Self>>) -> impl IntoElement {
        div().w_full().h(px(PICK_ROW_HEIGHT)).flex().items_center().justify_center().text_size(px(13.0)).text_color(theme::text_muted()).child(t!("sidebar.no_bookmarks_match"))
    }

    fn set_selected_index(&mut self, ix: Option<IndexPath>, _: &mut Window, _: &mut Context<ListState<Self>>) {
        self.selected = ix;
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let Some(id) = self.selected.and_then(|ix| self.shown.get(ix.row)).and_then(|index| self.items.get(*index)).map(|item| item.id.clone()) else { return };
        let root = self.root.clone();
        window.close_dialog(cx);
        window.defer(cx, move |_, cx| {
            let _ = root.update(cx, |app, cx| app.activate_bookmark(&id, cx));
        });
    }
}
