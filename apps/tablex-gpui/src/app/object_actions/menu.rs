//! The navigator rows' object-action menu entries: which rows offer which action, and the
//! entries themselves (the same element items the MongoDB menu uses: a stable debug selector per
//! entry and the danger color on the destructive ones).

use gpui::WeakEntity;
use gpui_component::menu::PopupMenu;

use super::super::mongo_admin_view::{entry_item, glyph, Entry};
use super::super::navigator_ui::SidebarGlyph;
use super::super::*;
use super::create_templates::CreateKind;
use crate::navigator::KindOfObject;
use crate::sql_ddl::{dialect_for, NamespaceKind};

/// What a table-like navigator row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ObjectKind {
    Table,
    View,
    MaterializedView,
}

impl ObjectKind {
    pub(in crate::app) fn of(kind: Option<KindOfObject>) -> Option<Self> {
        match kind {
            Some(KindOfObject::Table) | None => Some(Self::Table),
            Some(KindOfObject::View) => Some(Self::View),
            Some(KindOfObject::MaterializedView) => Some(Self::MaterializedView),
            // A MariaDB sequence is dropped and renamed with its own statements.
            Some(KindOfObject::Sequence) => None,
        }
    }

    pub(in crate::app) fn noun(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::MaterializedView => "materialized view",
        }
    }

    /// The navigator group that lists it.
    pub(in crate::app) fn group(self) -> ObjectGroup {
        match self {
            Self::Table => ObjectGroup::Tables,
            Self::View => ObjectGroup::Views,
            Self::MaterializedView => ObjectGroup::MaterializedViews,
        }
    }
}

/// One entry of the object-action part of a row's menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ObjectMenuItem {
    /// Remember the table (the tables selected beside it) for a Paste on another connection.
    CopyObjects,
    /// Open the Data Transfer on the tables copied before, into this connection, database or schema.
    PasteObjects,
    /// The DDL of the two tables or views selected, as a diff.
    CompareDdl,
    /// The DDL of the table or view against another one picked in the Go to Object dialog.
    CompareDdlWith,
    /// Keep the row in the Bookmarks section of the tree.
    AddBookmark,
    NewTable,
    /// Load a file into the table (on a table row) or into a new one (on a namespace).
    ImportData,
    /// Copy the rows of the table (or of the tables of a schema) into tables of a connection.
    TransferData,
    /// Write the rows of the table (or of the tables of a schema) to files.
    ExportData,
    /// Run a `.sql` file on the namespace.
    RunSqlFile,
    /// Back the database up with the server's native tool (a table row: that table).
    Backup,
    /// Restore a backup into the database with the server's native tool.
    Restore,
    NewNamespace(NamespaceKind),
    Truncate,
    Rename(ObjectKind),
    Drop(ObjectKind),
}

impl ObjectMenuItem {
    /// The label on a row that is part of a selection of several (`several`) or not.
    pub(in crate::app) fn label_for(self, several: bool) -> std::borrow::Cow<'static, str> {
        match self {
            Self::CopyObjects if several => t!("menu.copy_tables"),
            _ => self.label(),
        }
    }

    fn label(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::CopyObjects => t!("menu.copy_table"),
            Self::PasteObjects => t!("menu.paste"),
            Self::CompareDdl => t!("menu.compare_ddl"),
            Self::CompareDdlWith => t!("menu.compare_ddl_with"),
            Self::AddBookmark => t!("menu.add_bookmark"),
            Self::NewTable => t!("menu.new_table"),
            Self::ImportData => t!("menu.import_data"),
            Self::TransferData => t!("menu.transfer_data"),
            Self::ExportData => t!("menu.export_data"),
            Self::RunSqlFile => t!("menu.run_sql_file"),
            Self::Backup => t!("menu.backup"),
            Self::Restore => t!("menu.restore"),
            Self::NewNamespace(NamespaceKind::Schema) => t!("menu.new_schema"),
            Self::NewNamespace(NamespaceKind::Database) => t!("menu.new_database"),
            Self::Truncate => t!("menu.truncate_table"),
            Self::Rename(ObjectKind::Table) => t!("menu.rename_table"),
            Self::Rename(ObjectKind::View) => t!("menu.rename_view"),
            Self::Rename(ObjectKind::MaterializedView) => t!("menu.rename_materialized_view"),
            Self::Drop(ObjectKind::Table) => t!("menu.drop_table"),
            Self::Drop(ObjectKind::View) => t!("menu.drop_view"),
            Self::Drop(ObjectKind::MaterializedView) => t!("menu.drop_materialized_view"),
        }
    }

    pub(in crate::app) fn selector(self) -> &'static str {
        match self {
            Self::CopyObjects => "object-menu-copy",
            Self::PasteObjects => "object-menu-paste",
            Self::CompareDdl => "object-menu-compare",
            Self::CompareDdlWith => "object-menu-compare-with",
            Self::AddBookmark => "object-menu-add-bookmark",
            Self::NewTable => "object-menu-new-table",
            Self::ImportData => "object-menu-import",
            Self::TransferData => "object-menu-transfer",
            Self::ExportData => "object-menu-export",
            Self::RunSqlFile => "object-menu-run-file",
            Self::Backup => "object-menu-backup",
            Self::Restore => "object-menu-restore",
            Self::NewNamespace(NamespaceKind::Schema) => "object-menu-new-schema",
            Self::NewNamespace(NamespaceKind::Database) => "object-menu-new-database",
            Self::Truncate => "object-menu-truncate",
            Self::Rename(_) => "object-menu-rename",
            Self::Drop(_) => "object-menu-drop",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::CopyObjects => glyph(SidebarGlyph::Copy),
            Self::PasteObjects => Icon::empty().path("icons/clipboard-paste.svg").size(px(12.0)),
            Self::CompareDdl | Self::CompareDdlWith => Icon::empty().path("icons/git-compare.svg").size(px(12.0)),
            Self::AddBookmark => Icon::empty().path("icons/bookmark.svg").size(px(12.0)),
            Self::NewTable => glyph(SidebarGlyph::Table),
            Self::ImportData => Icon::empty().path("icons/folder-input.svg").size(px(12.0)),
            Self::TransferData => Icon::empty().path("icons/arrow-left-right.svg").size(px(12.0)),
            Self::ExportData => Icon::empty().path("icons/download.svg").size(px(12.0)),
            Self::RunSqlFile => Icon::empty().path("icons/play.svg").size(px(12.0)),
            Self::Backup => Icon::empty().path("icons/download.svg").size(px(12.0)),
            Self::Restore => Icon::empty().path("icons/upload.svg").size(px(12.0)),
            Self::NewNamespace(_) => glyph(SidebarGlyph::Database),
            Self::Truncate => Icon::empty().path("icons/eraser.svg").size(px(12.0)),
            Self::Rename(_) => glyph(SidebarGlyph::Edit),
            Self::Drop(_) => glyph(SidebarGlyph::Trash),
        }
    }

    fn destructive(self) -> bool {
        matches!(self, Self::Truncate | Self::Drop(_))
    }
}

/// The object-action entries of the row `id`, each opening its flow in `root`; after a separator
/// when the menu already has entries. Asked when the menu opens, not when the row is drawn.
pub(in crate::app) fn append_object_items(
    menu: PopupMenu,
    root: WeakEntity<AppRoot>,
    id: &NavigatorNodeId,
    after_entries: bool,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let items = root.update(cx, |this, _| this.object_menu_items(id)).unwrap_or_default();
    let generate = root.update(cx, |this, _| this.generate_sql_kinds(id)).unwrap_or_default();
    let creates = root.update(cx, |this, _| this.create_menu_kinds(id)).unwrap_or_default();
    if items.is_empty() && generate.is_empty() && creates.is_empty() {
        return menu;
    }
    let menu = if after_entries { menu.separator() } else { menu };
    // Generate SQL: a statement for the table (a SELECT for a view) in a new SQL tab.
    let menu = if generate.is_empty() {
        menu
    } else {
        let (generate_root, generate_id) = (root.clone(), id.clone());
        menu.submenu(t!("menu.generate_sql"), window, cx, move |submenu, _, _| {
            generate.iter().fold(submenu, |submenu, &kind| {
                let (root, id) = (generate_root.clone(), generate_id.clone());
                submenu.item(entry_item(
                    Entry { selector: kind.selector(), label: kind.label().into(), icon: kind.icon(), disabled: None, destructive: false },
                    move |_, app| {
                        let _ = root.update(app, |this, cx| this.open_generated_sql(kind, &id, cx));
                    },
                ))
            })
        })
    };
    // Paste is there to be found: greyed out, and saying why, until something was copied.
    let pasteable = root.update(cx, |this, _| this.tree_clipboard_filled()).unwrap_or(false);
    // Copy copies the whole selection when the row is part of one.
    let several = root.update(cx, |this, _| this.navigator.selected_ids().len() > 1 && this.navigator.selected_ids().contains(id)).unwrap_or(false);
    let menu = items.into_iter().fold(menu, |menu, item| {
        let (root, id) = (root.clone(), id.clone());
        let disabled = (item == ObjectMenuItem::PasteObjects && !pasteable).then(|| t!("menu.paste_tables_empty"));
        let label = item.label_for(several);
        menu.item(entry_item(
            Entry { selector: item.selector(), label, icon: item.icon(), disabled, destructive: item.destructive() },
            move |window, app| {
                let _ = root.update(app, |this, cx| this.run_object_menu_item(item, &id, window, cx));
            },
        ))
    });
    // New view, function, index, ...: a template in a new SQL tab. A folder or a table's folder offers
    // its own entry; a schema, a database or a table row the lot, in a submenu.
    if creates.is_empty() {
        return menu;
    }
    let create_item = {
        let (root, id) = (root.clone(), id.clone());
        move |kind: CreateKind| {
            let (root, id) = (root.clone(), id.clone());
            entry_item(Entry { selector: kind.selector(), label: kind.label(), icon: kind.icon(), disabled: None, destructive: false }, move |_, app| {
                let _ = root.update(app, |this, cx| this.open_create_template(kind, &id, cx));
            })
        }
    };
    if matches!(id, NavigatorNodeId::ObjectGroup { .. } | NavigatorNodeId::TableFolder { .. }) {
        return creates.into_iter().fold(menu, |menu, kind| menu.item(create_item(kind)));
    }
    menu.submenu(t!("menu.new_object"), window, cx, move |submenu, _, _| creates.iter().fold(submenu, |submenu, &kind| submenu.item(create_item(kind))))
}

impl AppRoot {
    /// The object actions the row `id` offers, by what its connection's dialect can do. Nothing for
    /// a connection that is not SQL.
    pub(in crate::app) fn object_menu_items(&self, id: &NavigatorNodeId) -> Vec<ObjectMenuItem> {
        let connection = match id {
            NavigatorNodeId::Database { connection, .. }
            | NavigatorNodeId::Schema { connection, .. }
            | NavigatorNodeId::ObjectGroup { connection, .. }
            | NavigatorNodeId::Object { connection, .. } => connection,
            _ => return Vec::new(),
        };
        let Some(dialect) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).and_then(|saved| dialect_for(saved.kind)) else {
            return Vec::new();
        };
        let caps = dialect.object_capabilities();
        let namespaces = caps.create_namespace.iter().copied().map(ObjectMenuItem::NewNamespace);
        // The native tools of PostgreSQL, MySQL and MariaDB.
        let backup = self.backup_supported(connection);
        // Two selected tables or views compare as they are; one is compared with another picked in a dialog.
        let compare = if self.compare_pair(id).is_some() { ObjectMenuItem::CompareDdl } else { ObjectMenuItem::CompareDdlWith };
        match id {
            NavigatorNodeId::Object { .. } => match ObjectKind::of(self.navigator.object_kind(id)) {
                Some(ObjectKind::Table) => [ObjectMenuItem::CopyObjects, compare, ObjectMenuItem::AddBookmark, ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData]
                    .into_iter()
                    .chain(backup.then_some(ObjectMenuItem::Backup))
                    .chain([ObjectMenuItem::Truncate, ObjectMenuItem::Rename(ObjectKind::Table), ObjectMenuItem::Drop(ObjectKind::Table)])
                    .collect(),
                Some(kind) => [ObjectMenuItem::CopyObjects, compare, ObjectMenuItem::AddBookmark]
                    .into_iter()
                    .chain(caps.rename_view.then_some(ObjectMenuItem::Rename(kind)))
                    .chain([ObjectMenuItem::Drop(kind)])
                    .collect(),
                None => Vec::new(),
            },
            NavigatorNodeId::ObjectGroup { group: ObjectGroup::Tables, .. } => [ObjectMenuItem::NewTable, ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData, ObjectMenuItem::PasteObjects, ObjectMenuItem::RunSqlFile]
                .into_iter()
                .chain(backup.then_some([ObjectMenuItem::Backup, ObjectMenuItem::Restore]).into_iter().flatten())
                .collect(),
            NavigatorNodeId::ObjectGroup { .. } => Vec::new(),
            _ => [ObjectMenuItem::AddBookmark, ObjectMenuItem::NewTable, ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData, ObjectMenuItem::PasteObjects, ObjectMenuItem::RunSqlFile]
                .into_iter()
                .chain(backup.then_some([ObjectMenuItem::Backup, ObjectMenuItem::Restore]).into_iter().flatten())
                .chain(namespaces)
                .collect(),
        }
    }
}
