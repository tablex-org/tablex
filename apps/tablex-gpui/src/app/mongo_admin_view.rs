//! MongoDB sidebar context menus and their dialogs.
//!
//! Menus use the official `PopupMenu` (element items so every entry carries a
//! stable debug selector and the destructive entries the danger color).
//! Dialogs use the official `Dialog` through `WindowExt`; each one is an
//! entity that owns its inputs and busy/error state, because the dialog
//! builder runs during Root render and must never touch `AppRoot`.
use super::mongo_admin::MongoNodeTarget;
use super::navigator_ui::SidebarGlyph;
use super::*;
use crate::mongo_admin::{
    typed_name_matches, validate_new_collection_name, IndexKeyType, MongoAdminCaps, MongoAdminOp,
    MongoIndexInfo, NewIndex,
};
use crate::navigator::NavigatorAction;
use gpui::{AnyWindowHandle, Subscription, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;
use gpui_component::menu::PopupMenu;
use gpui_component::select::{Select, SelectState};
use gpui_component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_component::IndexPath;

const UNAVAILABLE: &str = "Not supported by this MongoDB driver version";

pub(super) fn glyph(glyph: SidebarGlyph) -> Icon {
    Icon::new(glyph).size(px(12.0))
}

/// One menu entry. `destructive` paints the label with the danger color;
/// a disabled entry explains itself in a tooltip.
pub(super) struct Entry {
    pub(super) selector: &'static str,
    pub(super) label: &'static str,
    pub(super) icon: Icon,
    pub(super) disabled: Option<&'static str>,
    pub(super) destructive: bool,
}

pub(super) fn entry_item(
    entry: Entry,
    on_click: impl Fn(&mut Window, &mut gpui::App) + 'static,
) -> PopupMenuItem {
    let Entry {
        selector,
        label,
        icon,
        disabled,
        destructive,
    } = entry;
    PopupMenuItem::element(move |_, _| {
        let row = div()
            .id(selector)
            .debug_selector(move || selector.to_string())
            .when(destructive, |this| this.text_color(theme::danger()))
            .child(label);
        match disabled {
            Some(reason) => row
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(reason).build(window, cx)
                })
                .into_any_element(),
            None => row.into_any_element(),
        }
    })
    .icon(icon)
    .disabled(disabled.is_some())
    .on_click(move |_, window, app| on_click(window, app))
}

/// Everything the menu needs, read once at open time.
struct MenuSnapshot {
    caps: MongoAdminCaps,
    can_query: bool,
    can_aggregate: bool,
    default_database: Option<String>,
}

fn snapshot(
    root: &WeakEntity<AppRoot>,
    target: &MongoNodeTarget,
    cx: &mut gpui::App,
) -> MenuSnapshot {
    root.update(cx, |this, _| MenuSnapshot {
        caps: this.mongo_admin_caps(&target.connection),
        can_query: this
            .navigator
            .supports_action(&target.id, false, NavigatorAction::NewQuery),
        can_aggregate: this.mongo_supports_aggregate(&target.connection),
        default_database: this.mongo_default_database(&target.connection),
    })
    .unwrap_or(MenuSnapshot {
        caps: MongoAdminCaps::default(),
        can_query: false,
        can_aggregate: false,
        default_database: None,
    })
}

/// MongoDB collection / database menu. Collection: Copy Name, View Data,
/// View in New Tab, New Query, New Aggregation, Manage Indexes, Rename, Clone
/// Collection, Refresh, Drop Collection. Database: Copy Name, New Query, (Clear) Default
/// Database, Refresh, Drop Database.
pub(super) fn build_mongo_node_menu(
    menu: PopupMenu,
    root: WeakEntity<AppRoot>,
    target: MongoNodeTarget,
    cx: &mut gpui::App,
) -> PopupMenu {
    let snapshot = snapshot(&root, &target, cx);
    let gate = |allowed: bool| (!allowed).then_some(UNAVAILABLE);
    let copy_root = root.clone();
    let copy_id = target.id.clone();
    // Copy Name carries the real Cmd-C action for its hint; the click handler
    // wins on click, key presses land on the sidebar-level listener.
    let copy = PopupMenuItem::element(|_, _| {
        div()
            .id("mongo-menu-copy-name")
            .debug_selector(|| "mongo-menu-copy-name".into())
            .child("Copy Name")
    })
    .icon(glyph(SidebarGlyph::Copy))
    .action(Box::new(CopyConnectionName))
    .on_click(move |_, _, app| {
        let _ = copy_root.update(app, |this, cx| this.copy_mongo_node_name(&copy_id, cx));
    });
    let menu = menu.item(copy).separator();

    let query = {
        let (root, id) = (root.clone(), target.id.clone());
        entry_item(
            Entry {
                selector: "mongo-menu-new-query",
                label: "New Query",
                icon: Icon::new(IconName::SquareTerminal).size(px(12.0)),
                disabled: gate(snapshot.can_query),
                destructive: false,
            },
            move |_, app| {
                let _ = root.update(app, |this, cx| {
                    this.new_query_for_sidebar_node(&id, cx);
                });
            },
        )
    };
    let refresh = {
        let (root, target) = (root.clone(), target.clone());
        entry_item(
            Entry {
                selector: "mongo-menu-refresh",
                label: "Refresh",
                icon: glyph(SidebarGlyph::Refresh),
                disabled: None,
                destructive: false,
            },
            move |_, app| {
                let _ = root.update(app, |this, cx| this.refresh_mongo_node(&target, cx));
            },
        )
    };

    if target.collection.is_some() {
        let open = |new_tab: bool, selector, label, icon| {
            let (root, target) = (root.clone(), target.clone());
            entry_item(
                Entry {
                    selector,
                    label,
                    icon,
                    disabled: None,
                    destructive: false,
                },
                move |_, app| {
                    let _ = root.update(app, |this, cx| {
                        this.open_mongo_collection_data(&target, new_tab, cx)
                    });
                },
            )
        };
        let view_data = open(
            false,
            "mongo-menu-view-data",
            "View Data",
            glyph(SidebarGlyph::TableProperties),
        );
        let new_tab = open(
            true,
            "mongo-menu-view-new-tab",
            "View in New Tab",
            glyph(SidebarGlyph::CopyPlus),
        );
        let dialog_item = |selector,
                           label,
                           icon,
                           destructive,
                           allowed: bool,
                           open: fn(
            &mut AppRoot,
            MongoNodeTarget,
            &mut Window,
            &mut Context<AppRoot>,
        )| {
            let (root, target) = (root.clone(), target.clone());
            entry_item(
                Entry {
                    selector,
                    label,
                    icon,
                    disabled: gate(allowed),
                    destructive,
                },
                move |window, app| {
                    let target = target.clone();
                    let _ = root.update(app, |this, cx| open(this, target, window, cx));
                },
            )
        };
        let indexes = dialog_item(
            "mongo-menu-manage-indexes",
            "Manage Indexes",
            glyph(SidebarGlyph::PencilRuler),
            false,
            snapshot.caps.indexes,
            AppRoot::open_mongo_index_manager,
        );
        let rename = dialog_item(
            "mongo-menu-rename",
            "Rename",
            glyph(SidebarGlyph::Edit),
            false,
            snapshot.caps.collection,
            AppRoot::open_mongo_rename_dialog,
        );
        let clone = dialog_item(
            "mongo-menu-clone",
            "Clone Collection",
            glyph(SidebarGlyph::CopyPlus),
            false,
            snapshot.caps.collection,
            AppRoot::open_mongo_clone_dialog,
        );
        let drop = dialog_item(
            "mongo-menu-drop-collection",
            "Drop Collection",
            glyph(SidebarGlyph::Trash),
            true,
            snapshot.caps.collection,
            AppRoot::open_mongo_drop_dialog,
        );
        // "New Aggregation" opens a Compass-style Aggregate tab; it needs a
        // driver that can run `mongo_aggregate`.
        let aggregation = {
            let (root, target) = (root.clone(), target.clone());
            entry_item(
                Entry {
                    selector: "mongo-menu-new-aggregation",
                    label: "New Aggregation",
                    icon: Icon::empty().path("icons/list-ordered.svg").size(px(12.0)),
                    disabled: gate(snapshot.can_aggregate),
                    destructive: false,
                },
                move |_, app| {
                    let _ = root.update(app, |this, cx| this.new_aggregation_for_target(&target, cx));
                },
            )
        };
        menu.item(view_data)
            .item(new_tab)
            .item(query)
            .item(aggregation)
            .item(indexes)
            .item(rename)
            .item(clone)
            .item(refresh)
            .separator()
            .item(drop)
    } else {
        let default_item = {
            let (root, connection, database) = (
                root.clone(),
                target.connection.clone(),
                target.database.clone(),
            );
            let is_default = snapshot.default_database.as_deref() == Some(target.database.as_str());
            entry_item(
                Entry {
                    selector: if is_default {
                        "mongo-menu-clear-default-database"
                    } else {
                        "mongo-menu-set-default-database"
                    },
                    label: if is_default {
                        "Clear Default Database"
                    } else {
                        "Set as Default Database"
                    },
                    icon: glyph(SidebarGlyph::Database),
                    disabled: None,
                    destructive: false,
                },
                move |_, app| {
                    let _ = root.update(app, |this, cx| {
                        let next = (!is_default).then_some(database.as_str());
                        if let Err(error) = this.set_mongo_default_database(&connection, next, cx) {
                            this.sidebar.notice =
                                Some(format!("Could not save default database: {error}"));
                            cx.notify();
                        }
                    });
                },
            )
        };
        let drop = {
            let (root, target) = (root.clone(), target.clone());
            entry_item(
                Entry {
                    selector: "mongo-menu-drop-database",
                    label: "Drop Database",
                    icon: glyph(SidebarGlyph::Trash),
                    disabled: if crate::mongo_admin::is_protected_database(&target.database) {
                        Some("System databases cannot be dropped")
                    } else {
                        gate(snapshot.caps.database)
                    },
                    destructive: true,
                },
                move |window, app| {
                    let target = target.clone();
                    let _ = root.update(app, |this, cx| {
                        this.open_mongo_drop_dialog(target, window, cx)
                    });
                },
            )
        };
        menu.item(query)
            .item(default_item)
            .item(refresh)
            .separator()
            .item(drop)
    }
}

// ---------------------------------------------------------------------------
// Rename / Clone
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum NameMode {
    Rename,
    Clone,
}

struct NameDialog {
    root: WeakEntity<AppRoot>,
    window: AnyWindowHandle,
    target: MongoNodeTarget,
    collection: String,
    mode: NameMode,
    input: gpui::Entity<InputState>,
    include_indexes: bool,
    error: Option<String>,
    busy: bool,
    closed: bool,
    _subscription: Subscription,
}

impl NameDialog {
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let name = self.input.read(cx).value().to_string();
        if let Err(error) = validate_new_collection_name(&self.target.database, &name) {
            self.error = Some(error);
            cx.notify();
            return;
        }
        if name == self.collection {
            self.error = Some("The new name must differ from the current name.".into());
            cx.notify();
            return;
        }
        let op = match self.mode {
            NameMode::Rename => MongoAdminOp::RenameCollection {
                from: self.collection.clone(),
                to: name,
            },
            NameMode::Clone => MongoAdminOp::CloneCollection {
                from: self.collection.clone(),
                to: name,
                include_indexes: self.include_indexes,
            },
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let view = cx.entity().downgrade();
        let handle = self.window;
        let (connection, database) = (self.target.connection.clone(), self.target.database.clone());
        let _ = self.root.update(cx, |root, cx| {
            root.run_mongo_admin(
                &connection,
                &database,
                op,
                Box::new(move |_, result, cx| {
                    // The completion may run inside this view's own update;
                    // deferring keeps the entity lease free.
                    cx.defer(move |app| {
                        let _ = view.update(app, |this, cx| this.finished(result, handle, cx));
                    });
                }),
                cx,
            );
        });
    }

    fn finished(
        &mut self,
        result: Result<u64, String>,
        handle: AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        self.busy = false;
        match result {
            Ok(_) => {
                if !self.closed {
                    let _ = cx.update_window(handle, |_, window, cx| window.close_dialog(cx));
                }
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

impl Render for NameDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let description = match self.mode {
            NameMode::Rename => format!(
                "Rename \"{}\" in database \"{}\". An existing collection is never overwritten.",
                self.collection, self.target.database
            ),
            NameMode::Clone => format!(
                "Copy the documents of \"{}\" into a new collection in \"{}\". Collection options such as validators are not copied.",
                self.collection, self.target.database
            ),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(description),
            )
            .child(
                div()
                    .debug_selector(|| "mongo-name-input".into())
                    .child(Input::new(&self.input)),
            )
            .when(self.mode == NameMode::Clone, |this| {
                this.child(
                    div()
                        .self_start()
                        .debug_selector(|| "mongo-clone-include-indexes".into())
                        .child(
                            Checkbox::new("mongo-clone-include-indexes-box")
                                .label("Include indexes")
                                .checked(self.include_indexes)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.include_indexes = *checked;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-dialog-error".into())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
    }
}

fn dialog_cancel(id: &'static str) -> Button {
    Button::new(id)
        .label("Cancel")
        .compact()
        .ghost()
        .debug_selector(move || id.to_string())
        .on_click(|_, window, cx| window.close_dialog(cx))
}

impl AppRoot {
    pub(super) fn open_mongo_rename_dialog(
        &mut self,
        target: MongoNodeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_mongo_name_dialog(target, NameMode::Rename, window, cx);
    }

    pub(super) fn open_mongo_clone_dialog(
        &mut self,
        target: MongoNodeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_mongo_name_dialog(target, NameMode::Clone, window, cx);
    }

    fn open_mongo_name_dialog(
        &mut self,
        target: MongoNodeTarget,
        mode: NameMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(collection) = target.collection.clone() else {
            return;
        };
        if !self.mongo_admin_caps(&target.connection).collection {
            self.sidebar.notice = Some(format!("{UNAVAILABLE}."));
            cx.notify();
            return;
        }
        let initial = match mode {
            NameMode::Rename => collection.clone(),
            NameMode::Clone => format!("{collection}_copy"),
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let focus_input = input.clone();
        let handle = window.window_handle();
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| {
            // Enter reaches the dialog's own `Confirm` path (`on_ok` below),
            // so only edits are observed here.
            let subscription = cx.subscribe(
                &input,
                |this: &mut NameDialog, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                },
            );
            NameDialog {
                root,
                window: handle,
                target,
                collection,
                mode,
                input,
                include_indexes: true,
                error: None,
                busy: false,
                closed: false,
                _subscription: subscription,
            }
        });
        let (title, ok) = match mode {
            NameMode::Rename => ("Rename Collection", "Rename"),
            NameMode::Clone => ("Clone as New Collection", "Clone"),
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let busy = view.read(cx).busy;
            let submit_view = view.clone();
            let enter_view = view.clone();
            let close_view = view.clone();
            dialog
                .title(title)
                .w(px(440.0))
                .child(view.clone())
                // Enter confirms through the dialog; never closes by itself.
                .on_ok(move |_, _, cx| {
                    enter_view.update(cx, |this, cx| this.submit(cx));
                    false
                })
                .footer(
                    DialogFooter::new()
                        .child(dialog_cancel("mongo-name-cancel"))
                        .child(
                            Button::new("mongo-name-submit")
                                .label(ok)
                                .compact()
                                .primary()
                                .disabled(busy)
                                .debug_selector(|| "mongo-name-submit".into())
                                .on_click(move |_, _, cx| {
                                    submit_view.update(cx, |this, cx| this.submit(cx));
                                }),
                        ),
                )
                .on_close(move |_, _, cx| {
                    close_view.update(cx, |this, _| this.closed = true);
                })
        });
        // Opening a dialog moves focus to the dialog itself; the name field
        // takes it back so typing and Enter work immediately.
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }
}

// ---------------------------------------------------------------------------
// Drop (typed-name confirmation)
// ---------------------------------------------------------------------------

struct DropDialog {
    root: WeakEntity<AppRoot>,
    window: AnyWindowHandle,
    target: MongoNodeTarget,
    /// The name the user has to type.
    expected: String,
    input: gpui::Entity<InputState>,
    error: Option<String>,
    busy: bool,
    closed: bool,
    _subscription: Subscription,
}

impl DropDialog {
    fn confirmed(&self, cx: &gpui::App) -> bool {
        typed_name_matches(&self.expected, self.input.read(cx).value().as_ref())
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.confirmed(cx) {
            return;
        }
        let op = match &self.target.collection {
            Some(collection) => MongoAdminOp::DropCollection {
                collection: collection.clone(),
            },
            None => MongoAdminOp::DropDatabase,
        };
        self.busy = true;
        self.error = None;
        cx.notify();
        let view = cx.entity().downgrade();
        let handle = self.window;
        let (connection, database) = (self.target.connection.clone(), self.target.database.clone());
        let _ = self.root.update(cx, |root, cx| {
            root.run_mongo_admin(
                &connection,
                &database,
                op,
                Box::new(move |_, result, cx| {
                    cx.defer(move |app| {
                        let _ = view.update(app, |this, cx| {
                            this.busy = false;
                            match result {
                                Ok(_) => {
                                    if !this.closed {
                                        let _ = cx.update_window(handle, |_, window, cx| {
                                            window.close_dialog(cx)
                                        });
                                    }
                                }
                                Err(error) => this.error = Some(error),
                            }
                            cx.notify();
                        });
                    });
                }),
                cx,
            );
        });
    }
}

impl Render for DropDialog {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let (noun, consequence) = match &self.target.collection {
            Some(_) => ("collection", "the collection and all its documents"),
            None => ("database", "the database and all its data"),
        };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .child(format!(
                        "Are you sure you want to drop {noun} \"{}\"? This will permanently delete {consequence}.",
                        self.expected
                    )),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(format!("Type the {noun} name to confirm.")),
            )
            .child(
                div()
                    .debug_selector(|| "mongo-drop-confirm-input".into())
                    .child(Input::new(&self.input)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-dialog-error".into())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
    }
}

impl AppRoot {
    pub(super) fn open_mongo_drop_dialog(
        &mut self,
        target: MongoNodeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let caps = self.mongo_admin_caps(&target.connection);
        let (allowed, expected, label) = match &target.collection {
            Some(collection) => (caps.collection, collection.clone(), "Drop Collection"),
            None => (
                caps.database && !crate::mongo_admin::is_protected_database(&target.database),
                target.database.clone(),
                "Drop Database",
            ),
        };
        if !allowed {
            self.sidebar.notice = Some(format!("{UNAVAILABLE}."));
            cx.notify();
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(expected.clone()));
        let focus_input = input.clone();
        let handle = window.window_handle();
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let subscription =
                cx.subscribe(&input, |_: &mut DropDialog, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                });
            DropDialog {
                root,
                window: handle,
                target,
                expected,
                input,
                error: None,
                busy: false,
                closed: false,
                _subscription: subscription,
            }
        });
        window.open_dialog(cx, move |dialog, _, cx| {
            let enabled = {
                let state = view.read(cx);
                !state.busy && state.confirmed(cx)
            };
            let submit_view = view.clone();
            let enter_view = view.clone();
            let close_view = view.clone();
            dialog
                .title(label)
                .w(px(460.0))
                .child(view.clone())
                .on_ok(move |_, _, cx| {
                    enter_view.update(cx, |this, cx| this.submit(cx));
                    false
                })
                .footer(
                    DialogFooter::new()
                        .child(dialog_cancel("mongo-drop-cancel"))
                        .child(
                            Button::new("mongo-drop-submit")
                                .label(label)
                                .compact()
                                .danger()
                                .disabled(!enabled)
                                .debug_selector(|| "mongo-drop-submit".into())
                                .on_click(move |_, _, cx| {
                                    submit_view.update(cx, |this, cx| this.submit(cx));
                                }),
                        ),
                )
                .on_close(move |_, _, cx| {
                    close_view.update(cx, |this, _| this.closed = true);
                })
        });
        focus_input.update(cx, |state, cx| state.focus(window, cx));
    }
}

// ---------------------------------------------------------------------------
// Index manager
// ---------------------------------------------------------------------------

struct IndexDelegate {
    rows: Vec<MongoIndexInfo>,
}

impl TableDelegate for IndexDelegate {
    fn columns_count(&self, _: &gpui::App) -> usize {
        5
    }

    fn rows_count(&self, _: &gpui::App) -> usize {
        self.rows.len()
    }

    fn column(&self, ix: usize, _: &gpui::App) -> Column {
        let (key, name, width) = [
            ("name", "Name", 170.0),
            ("keys", "Keys", 210.0),
            ("unique", "Unique", 60.0),
            ("sparse", "Sparse", 60.0),
            ("ttl", "TTL (s)", 70.0),
        ][ix];
        Column::new(key, name)
            .width(px(width))
            .resizable(false)
            .movable(false)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        let text = match col_ix {
            0 => row.name.clone(),
            1 => row.keys_label(),
            2 => if row.unique { "Yes" } else { "" }.to_string(),
            3 => if row.sparse { "Yes" } else { "" }.to_string(),
            _ => row
                .ttl_seconds
                .map(|ttl| ttl.to_string())
                .unwrap_or_default(),
        };
        div()
            .size_full()
            .text_size(px(12.0))
            .debug_selector(move || format!("mongo-index-cell-{row_ix}-{col_ix}"))
            .child(text)
    }
}

struct KeyRow {
    field: gpui::Entity<InputState>,
    key_type: gpui::Entity<SelectState<Vec<String>>>,
}

struct IndexManager {
    root: WeakEntity<AppRoot>,
    target: MongoNodeTarget,
    collection: String,
    table: gpui::Entity<TableState<IndexDelegate>>,
    selected: Option<String>,
    loading: bool,
    error: Option<String>,
    creating: bool,
    create_busy: bool,
    form_error: Option<String>,
    keys: Vec<KeyRow>,
    name: gpui::Entity<InputState>,
    ttl: gpui::Entity<InputState>,
    unique: bool,
    sparse: bool,
    _subscriptions: Vec<Subscription>,
}

fn key_type_labels() -> Vec<String> {
    IndexKeyType::ALL
        .iter()
        .map(|key_type| key_type.label().to_string())
        .collect()
}

impl IndexManager {
    fn new_key_row(window: &mut Window, cx: &mut Context<Self>) -> KeyRow {
        KeyRow {
            field: cx.new(|cx| InputState::new(window, cx).placeholder("Field name")),
            key_type: cx.new(|cx| {
                SelectState::new(
                    key_type_labels(),
                    Some(IndexPath::default().row(0)),
                    window,
                    cx,
                )
            }),
        }
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
        let view = cx.entity().downgrade();
        let (connection, database, collection) = (
            self.target.connection.clone(),
            self.target.database.clone(),
            self.collection.clone(),
        );
        let _ = self.root.update(cx, |root, cx| {
            root.list_mongo_indexes(
                &connection,
                &database,
                &collection,
                Box::new(move |_, result, cx| {
                    cx.defer(move |app| {
                        let _ = view.update(app, |this, cx| this.loaded(result, cx));
                    });
                }),
                cx,
            );
        });
    }

    fn loaded(&mut self, result: Result<Vec<MongoIndexInfo>, String>, cx: &mut Context<Self>) {
        self.loading = false;
        let rows = match result {
            Ok(rows) => rows,
            Err(error) => {
                self.error = Some(error);
                Vec::new()
            }
        };
        if self
            .selected
            .as_ref()
            .is_some_and(|name| !rows.iter().any(|row| &row.name == name))
        {
            self.selected = None;
        }
        self.table.update(cx, |table, cx| {
            table.delegate_mut().rows = rows;
            table.refresh(cx);
            cx.notify();
        });
        cx.notify();
    }

    fn run(&mut self, op: MongoAdminOp, create: bool, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let (connection, database) = (self.target.connection.clone(), self.target.database.clone());
        let _ = self.root.update(cx, |root, cx| {
            root.run_mongo_admin(
                &connection,
                &database,
                op,
                Box::new(move |_, result, cx| {
                    cx.defer(move |app| {
                        let _ =
                            view.update(app, |this, cx| this.operation_done(create, result, cx));
                    });
                }),
                cx,
            );
        });
    }

    fn operation_done(
        &mut self,
        create: bool,
        result: Result<u64, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(_) => {
                if create {
                    self.creating = false;
                    self.create_busy = false;
                    self.form_error = None;
                }
                self.reload(cx);
            }
            Err(error) => {
                if create {
                    self.create_busy = false;
                    self.form_error = Some(error);
                } else {
                    self.error = Some(error);
                }
            }
        }
        cx.notify();
    }

    fn submit_create(&mut self, cx: &mut Context<Self>) {
        if self.create_busy {
            return;
        }
        let keys: Vec<(String, IndexKeyType)> = self
            .keys
            .iter()
            .map(|row| {
                let label = row.key_type.read(cx).selected_value().cloned();
                let key_type = IndexKeyType::ALL
                    .into_iter()
                    .find(|key_type| Some(key_type.label()) == label.as_deref())
                    .unwrap_or(IndexKeyType::Ascending);
                (row.field.read(cx).value().to_string(), key_type)
            })
            .collect();
        let index = match NewIndex::from_form(
            &keys,
            self.name.read(cx).value().as_ref(),
            self.unique,
            self.sparse,
            self.ttl.read(cx).value().as_ref(),
        ) {
            Ok(index) => index,
            Err(error) => {
                self.form_error = Some(error);
                cx.notify();
                return;
            }
        };
        self.create_busy = true;
        self.form_error = None;
        cx.notify();
        let collection = self.collection.clone();
        self.run(MongoAdminOp::CreateIndex { collection, index }, true, cx);
    }

    fn open_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.creating = true;
        self.form_error = None;
        if self.keys.is_empty() {
            let row = Self::new_key_row(window, cx);
            row.field.update(cx, |state, cx| state.focus(window, cx));
            self.keys.push(row);
        }
        cx.notify();
    }

    fn confirm_drop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.selected.clone().filter(|name| name != "_id_") else {
            return;
        };
        let view = cx.entity().downgrade();
        let collection = self.collection.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            let name = name.clone();
            let collection = collection.clone();
            dialog
                .title("Drop Index")
                .child(format!(
                    "Drop index \"{name}\" from \"{collection}\"? Queries that rely on it may become slower."
                ))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Drop Index"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let (name, collection) = (name.clone(), collection.clone());
                    let _ = view.update(cx, |this, cx| {
                        this.run(MongoAdminOp::DropIndex { collection, name }, false, cx)
                    });
                    true
                })
        });
    }
}

impl Render for IndexManager {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let can_drop = self.selected.as_ref().is_some_and(|name| name != "_id_");
        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(
                Button::new("mongo-index-create")
                    .label("Create Index")
                    .small()
                    .outline()
                    .disabled(self.creating || self.loading)
                    .debug_selector(|| "mongo-index-create".into())
                    .on_click(cx.listener(|this, _, window, cx| this.open_form(window, cx))),
            )
            .child(
                Button::new("mongo-index-drop")
                    .label("Drop Index")
                    .small()
                    .outline()
                    .disabled(!can_drop || self.creating || self.loading)
                    .debug_selector(|| "mongo-index-drop".into())
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_drop(window, cx))),
            )
            .child(
                div().ml_auto().child(
                    Button::new("mongo-index-refresh")
                        .label("Refresh")
                        .small()
                        .ghost()
                        .disabled(self.loading || self.creating)
                        .debug_selector(|| "mongo-index-refresh".into())
                        .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                ),
            );
        let empty =
            !self.loading && self.error.is_none() && self.table.read(cx).delegate().rows.is_empty();
        let list = div()
            .h(px(190.0))
            .w_full()
            .debug_selector(|| "mongo-index-table".into())
            .child(DataTable::new(&self.table).bordered(true))
            .when(empty, |this| {
                this.child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child("This collection has no indexes yet."),
                )
            });
        let status = self
            .loading
            .then(|| "Loading indexes…".to_string())
            .or_else(|| {
                self.selected
                    .as_ref()
                    .map(|name| format!("Selected: {name}"))
            });
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(format!("{} / {}", self.target.database, self.collection)),
            )
            .child(toolbar)
            .child(list)
            .when_some(status, |this, status| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-index-status".into())
                        .text_size(px(11.0))
                        .text_color(theme::text_muted())
                        .child(status),
                )
            })
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-dialog-error".into())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            });
        if self.creating {
            content = content.child(self.render_form(cx));
        }
        content
    }
}

impl IndexManager {
    fn render_form(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let removable = self.keys.len() > 1;
        let rows = self.keys.iter().enumerate().map(|(index, row)| {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_1()
                        .debug_selector(move || format!("mongo-index-field-{index}"))
                        .child(Input::new(&row.field)),
                )
                .child(
                    div()
                        .w(px(150.0))
                        .debug_selector(move || format!("mongo-index-type-{index}"))
                        .child(Select::new(&row.key_type)),
                )
                .child(
                    Button::new(("mongo-index-remove-field", index))
                        .icon(Icon::new(SidebarGlyph::Trash).size(px(12.0)))
                        .small()
                        .ghost()
                        .disabled(!removable)
                        .debug_selector(move || format!("mongo-index-remove-field-{index}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.keys.len() > 1 && index < this.keys.len() {
                                this.keys.remove(index);
                                cx.notify();
                            }
                        })),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .border_1()
            .border_color(theme::border())
            .rounded(px(6.0))
            .debug_selector(|| "mongo-index-form".into())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(12.0)).child("Index fields"))
                    .child(
                        Button::new("mongo-index-add-field")
                            .label("Add field")
                            .small()
                            .outline()
                            .debug_selector(|| "mongo-index-add-field".into())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let row = Self::new_key_row(window, cx);
                                this.keys.push(row);
                                cx.notify();
                            })),
                    ),
            )
            .children(rows)
            .child(
                div()
                    .debug_selector(|| "mongo-index-name".into())
                    .child(Input::new(&self.name)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(14.0))
                    .child(
                        div().debug_selector(|| "mongo-index-unique".into()).child(
                            Checkbox::new("mongo-index-unique-box")
                                .label("Unique")
                                .checked(self.unique)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.unique = *checked;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div().debug_selector(|| "mongo-index-sparse".into()).child(
                            Checkbox::new("mongo-index-sparse-box")
                                .label("Sparse")
                                .checked(self.sparse)
                                .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                    this.sparse = *checked;
                                    cx.notify();
                                })),
                        ),
                    )
                    .child(
                        div()
                            .w(px(190.0))
                            .debug_selector(|| "mongo-index-ttl".into())
                            .child(Input::new(&self.ttl)),
                    ),
            )
            .when_some(self.form_error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-index-form-error".into())
                        .text_size(px(11.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .justify_end()
                    .gap(px(6.0))
                    .child(
                        Button::new("mongo-index-form-cancel")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .disabled(self.create_busy)
                            .debug_selector(|| "mongo-index-form-cancel".into())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.creating = false;
                                this.form_error = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("mongo-index-form-submit")
                            .label("Create")
                            .small()
                            .primary()
                            .disabled(self.create_busy)
                            .debug_selector(|| "mongo-index-form-submit".into())
                            .on_click(cx.listener(|this, _, _, cx| this.submit_create(cx))),
                    ),
            )
    }
}

impl AppRoot {
    pub(super) fn open_mongo_index_manager(
        &mut self,
        target: MongoNodeTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(collection) = target.collection.clone() else {
            return;
        };
        if !self.mongo_admin_caps(&target.connection).indexes {
            self.sidebar.notice = Some(format!("{UNAVAILABLE}."));
            cx.notify();
            return;
        }
        let root = cx.entity().downgrade();
        let view = cx.new(|cx| {
            let table = cx.new(|cx| {
                TableState::new(IndexDelegate { rows: Vec::new() }, window, cx)
                    .col_resizable(false)
                    .col_movable(false)
                    .sortable(false)
            });
            let select_subscription = cx.subscribe(
                &table,
                |this: &mut IndexManager, table, event: &TableEvent, cx| {
                    if let TableEvent::SelectRow(row) = event {
                        this.selected = table
                            .read(cx)
                            .delegate()
                            .rows
                            .get(*row)
                            .map(|row| row.name.clone());
                        cx.notify();
                    }
                },
            );
            let name =
                cx.new(|cx| InputState::new(window, cx).placeholder("Name (generated if blank)"));
            let ttl =
                cx.new(|cx| InputState::new(window, cx).placeholder("Expire after (seconds)"));
            IndexManager {
                root,
                target,
                collection,
                table,
                selected: None,
                loading: false,
                error: None,
                creating: false,
                create_busy: false,
                form_error: None,
                keys: Vec::new(),
                name,
                ttl,
                unique: false,
                sparse: false,
                _subscriptions: vec![select_subscription],
            }
        });
        // The initial load goes through `self` (AppRoot is being updated, so
        // the view must not re-enter it through its weak handle).
        let weak = view.downgrade();
        view.update(cx, |this, _| this.loading = true);
        let (connection, database) = {
            let state = view.read(cx);
            (
                state.target.connection.clone(),
                state.target.database.clone(),
            )
        };
        let collection_name = view.read(cx).collection.clone();
        self.list_mongo_indexes(
            &connection,
            &database,
            &collection_name,
            Box::new(move |_, result, cx| {
                cx.defer(move |app| {
                    let _ = weak.update(app, |this, cx| this.loaded(result, cx));
                });
            }),
            cx,
        );
        let title = format!("Indexes: \"{}\"", view.read(cx).collection);
        window.open_dialog(cx, move |dialog, _, _| {
            let enter_view = view.clone();
            dialog
                .title(title.clone())
                .w(px(640.0))
                .child(view.clone())
                // Enter submits an open create form and never closes the manager.
                .on_ok(move |_, _, cx| {
                    enter_view.update(cx, |this, cx| {
                        if this.creating {
                            this.submit_create(cx);
                        }
                    });
                    false
                })
                .footer(
                    DialogFooter::new().child(dialog_cancel("mongo-index-close").label("Close")),
                )
        });
    }
}
