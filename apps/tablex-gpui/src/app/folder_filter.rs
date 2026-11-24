//! Per-folder filters of the connection tree: "Filter..." on a connection (its databases), a
//! database (its schemas) and an object group (Tables, Views, Functions, ...) opens a small dialog
//! with Include and Exclude patterns (comma-separated names with `*` and `?`, case-insensitive).
//! The filter narrows that folder only, over the children the navigator has loaded (no driver call,
//! "Load More" and the folder's expansion are untouched), is kept in the sidebar preferences
//! (`SidebarPreferences::folder_filters`, so it survives a restart), and the folder's row says
//! "(filtered 3/12)". "Clear filter" is in the dialog and in the folder's menu while a filter is set.

use gpui::WeakEntity;
use gpui_component::dialog::DialogFooter;
use gpui_component::menu::PopupMenu;

use super::mongo_admin_view::{entry_item, glyph, Entry};
use super::navigator_ui::SidebarGlyph;
use super::*;
use crate::connections::ConnectionKind;
use crate::sidebar_preferences::{parse_folder_patterns, FolderFilter, FolderKey, FolderKind};

/// What a folder's menu shows for its filter.
struct FilterMenu {
    key: FolderKey,
    label: std::borrow::Cow<'static, str>,
    active: bool,
}

/// The filter entries of the folder row `id`: Filter... and, while one is set, Clear filter; after a
/// separator when the menu has entries already.
pub(super) fn append_filter_items(
    menu: PopupMenu,
    root: WeakEntity<AppRoot>,
    id: &NavigatorNodeId,
    after_entries: bool,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(FilterMenu { key, label, active }) = root.update(cx, |this, _| this.folder_filter_menu(id)).ok().flatten() else {
        return menu;
    };
    let menu = if after_entries { menu.separator() } else { menu };
    let open = {
        let (root, key) = (root.clone(), key.clone());
        entry_item(
            Entry { selector: "folder-menu-filter", label, icon: glyph(SidebarGlyph::Filter), disabled: None, destructive: false },
            move |window, app| {
                let _ = root.update(app, |this, cx| this.open_folder_filter_dialog(key.clone(), window, cx));
            },
        )
    };
    let menu = menu.item(open);
    if !active {
        return menu;
    }
    menu.item(entry_item(
        Entry { selector: "folder-menu-clear-filter", label: t!("sidebar.clear_filter"), icon: glyph(SidebarGlyph::Filter), disabled: None, destructive: false },
        move |_, app| {
            let _ = root.update(app, |this, cx| this.clear_folder_filter(&key, cx));
        },
    ))
}

fn folder_name(folder: FolderKind) -> String {
    match folder {
        FolderKind::Databases => "Databases".to_owned(),
        FolderKind::Schemas => "Schemas".to_owned(),
        FolderKind::Objects(group) => group.title().to_owned(),
    }
}

impl AppRoot {
    /// The filter entries the folder row `id` offers (`None`: it is no folder that can be filtered).
    fn folder_filter_menu(&self, id: &NavigatorNodeId) -> Option<FilterMenu> {
        let key = navigator::folder_key(id)?;
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(&key.connection))?;
        let offered = match key.folder {
            // SQLite lists the one database of its file; Redis has no databases to name.
            FolderKind::Databases => !saved.disabled && !matches!(saved.kind, ConnectionKind::Sqlite | ConnectionKind::Redis),
            FolderKind::Schemas => crate::navigator::sql_driver::is_sql_kind(saved.kind) && self.sql_supports_schemas(&key.connection),
            FolderKind::Objects(_) => crate::navigator::sql_driver::is_sql_kind(saved.kind),
        };
        if !offered {
            return None;
        }
        let label = match key.folder {
            FolderKind::Databases => t!("menu.filter_databases"),
            FolderKind::Schemas => t!("menu.filter_schemas"),
            FolderKind::Objects(_) => t!("menu.filter_folder"),
        };
        let active = self.navigator.preferences().folder_filter(&key).is_some();
        Some(FilterMenu { key, label, active })
    }

    /// Whether the menu of the row `id` has entries of its own before the filter ones.
    pub(super) fn has_object_entries(&self, id: &NavigatorNodeId) -> bool {
        !self.object_menu_items(id).is_empty() || !self.create_menu_kinds(id).is_empty()
    }

    pub(super) fn apply_folder_filter(&mut self, filter: FolderFilter, cx: &mut Context<Self>) {
        // A failed save is reported once, by the sidebar's own error line.
        let _ = self.change_sidebar_preferences(|prefs| prefs.set_folder_filter(filter), cx);
    }

    pub(super) fn clear_folder_filter(&mut self, key: &FolderKey, cx: &mut Context<Self>) {
        let _ = self.change_sidebar_preferences(
            |prefs| {
                prefs.clear_folder_filter(key);
                Ok(())
            },
            cx,
        );
    }

    /// The Filter dialog of a folder: the two pattern fields (filled with what is set), Clear filter,
    /// Cancel and Apply. Apply with both fields empty clears the filter.
    pub(super) fn open_folder_filter_dialog(&mut self, key: FolderKey, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.navigator.preferences().folder_filter(&key).cloned();
        let field = |patterns: Option<&Vec<String>>, placeholder: std::borrow::Cow<'static, str>, window: &mut Window, cx: &mut Context<Self>| {
            let text = patterns.map(|patterns| patterns.join(", ")).unwrap_or_default();
            cx.new(|cx| InputState::new(window, cx).default_value(text).placeholder(placeholder))
        };
        let include = field(current.as_ref().map(|filter| &filter.include), t!("menu.folder_filter_include_placeholder"), window, cx);
        let exclude = field(current.as_ref().map(|filter| &filter.exclude), t!("menu.folder_filter_exclude_placeholder"), window, cx);
        let focus_include = include.clone();
        let root = cx.entity().downgrade();
        let title = t!("menu.folder_filter_title", folder = folder_name(key.folder)).into_owned();
        let submit = move |root: &WeakEntity<AppRoot>, key: &FolderKey, include: &gpui::Entity<InputState>, exclude: &gpui::Entity<InputState>, cx: &mut gpui::App| {
            let filter = FolderFilter {
                key: key.clone(),
                include: parse_folder_patterns(include.read(cx).value().as_ref()),
                exclude: parse_folder_patterns(exclude.read(cx).value().as_ref()),
            };
            let _ = root.update(cx, |this, cx| this.apply_folder_filter(filter, cx));
        };
        window.open_dialog(cx, move |dialog, _, _| {
            let (enter_root, enter_key, enter_include, enter_exclude) = (root.clone(), key.clone(), include.clone(), exclude.clone());
            let (apply_root, apply_key, apply_include, apply_exclude) = (root.clone(), key.clone(), include.clone(), exclude.clone());
            let (clear_root, clear_key) = (root.clone(), key.clone());
            let labelled = |text: std::borrow::Cow<'static, str>, selector: &'static str, input: &gpui::Entity<InputState>| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_size(px(12.0)).child(text))
                    .child(div().debug_selector(move || selector.into()).child(Input::new(input)))
            };
            dialog
                .title(title.clone())
                .w(px(440.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(labelled(t!("menu.folder_filter_include"), "folder-filter-include", &include))
                        .child(labelled(t!("menu.folder_filter_exclude"), "folder-filter-exclude", &exclude))
                        .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(t!("menu.folder_filter_hint"))),
                )
                // Enter applies and closes.
                .on_ok(move |_, _, cx| {
                    submit(&enter_root, &enter_key, &enter_include, &enter_exclude, cx);
                    true
                })
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("folder-filter-clear")
                                .label(t!("sidebar.clear_filter"))
                                .compact()
                                .ghost()
                                .debug_selector(|| "folder-filter-clear".into())
                                .on_click(move |_, window, cx| {
                                    let key = clear_key.clone();
                                    let _ = clear_root.update(cx, |this, cx| this.clear_folder_filter(&key, cx));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("folder-filter-cancel")
                                .label(t!("common.cancel"))
                                .compact()
                                .ghost()
                                .debug_selector(|| "folder-filter-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("folder-filter-apply")
                                .label(t!("menu.folder_filter_apply"))
                                .compact()
                                .primary()
                                .debug_selector(|| "folder-filter-apply".into())
                                .on_click(move |_, window, cx| {
                                    submit(&apply_root, &apply_key, &apply_include, &apply_exclude, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
        focus_include.update(cx, |state, cx| state.focus(window, cx));
    }
}
