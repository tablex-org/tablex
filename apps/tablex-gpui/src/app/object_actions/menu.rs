//! The navigator rows' object-action menu entries: which rows offer which action, and the
//! entries themselves (the same element items the MongoDB menu uses: a stable debug selector per
//! entry and the danger color on the destructive ones).

use gpui::WeakEntity;
use gpui_component::menu::PopupMenu;

use super::super::mongo_admin_view::{entry_item, glyph, Entry};
use super::super::navigator_ui::SidebarGlyph;
use super::super::*;
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
    fn label(self) -> &'static str {
        match self {
            Self::NewTable => "New Table\u{2026}",
            Self::ImportData => "Import Data\u{2026}",
            Self::TransferData => "Transfer Data\u{2026}",
            Self::ExportData => "Export Data\u{2026}",
            Self::RunSqlFile => "Run SQL File\u{2026}",
            Self::Backup => "Backup\u{2026}",
            Self::Restore => "Restore\u{2026}",
            Self::NewNamespace(NamespaceKind::Schema) => "New Schema\u{2026}",
            Self::NewNamespace(NamespaceKind::Database) => "New Database\u{2026}",
            Self::Truncate => "Truncate Table\u{2026}",
            Self::Rename(ObjectKind::Table) => "Rename Table\u{2026}",
            Self::Rename(ObjectKind::View) => "Rename View\u{2026}",
            Self::Rename(ObjectKind::MaterializedView) => "Rename Materialized View\u{2026}",
            Self::Drop(ObjectKind::Table) => "Drop Table\u{2026}",
            Self::Drop(ObjectKind::View) => "Drop View\u{2026}",
            Self::Drop(ObjectKind::MaterializedView) => "Drop Materialized View\u{2026}",
        }
    }

    pub(in crate::app) fn selector(self) -> &'static str {
        match self {
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
    if items.is_empty() && generate.is_empty() {
        return menu;
    }
    let menu = if after_entries { menu.separator() } else { menu };
    // Generate SQL: a statement for the table (a SELECT for a view) in a new SQL tab.
    let menu = if generate.is_empty() {
        menu
    } else {
        let (generate_root, generate_id) = (root.clone(), id.clone());
        menu.submenu("Generate SQL", window, cx, move |submenu, _, _| {
            generate.iter().fold(submenu, |submenu, &kind| {
                let (root, id) = (generate_root.clone(), generate_id.clone());
                submenu.item(entry_item(
                    Entry { selector: kind.selector(), label: kind.label(), icon: kind.icon(), disabled: None, destructive: false },
                    move |_, app| {
                        let _ = root.update(app, |this, cx| this.open_generated_sql(kind, &id, cx));
                    },
                ))
            })
        })
    };
    items.into_iter().fold(menu, |menu, item| {
        let (root, id) = (root.clone(), id.clone());
        menu.item(entry_item(
            Entry { selector: item.selector(), label: item.label(), icon: item.icon(), disabled: None, destructive: item.destructive() },
            move |window, app| {
                let _ = root.update(app, |this, cx| this.run_object_menu_item(item, &id, window, cx));
            },
        ))
    })
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
        match id {
            NavigatorNodeId::Object { .. } => match ObjectKind::of(self.navigator.object_kind(id)) {
                Some(ObjectKind::Table) => [ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData]
                    .into_iter()
                    .chain(backup.then_some(ObjectMenuItem::Backup))
                    .chain([ObjectMenuItem::Truncate, ObjectMenuItem::Rename(ObjectKind::Table), ObjectMenuItem::Drop(ObjectKind::Table)])
                    .collect(),
                Some(kind) => caps
                    .rename_view
                    .then_some(ObjectMenuItem::Rename(kind))
                    .into_iter()
                    .chain([ObjectMenuItem::Drop(kind)])
                    .collect(),
                None => Vec::new(),
            },
            NavigatorNodeId::ObjectGroup { group: ObjectGroup::Tables, .. } => [ObjectMenuItem::NewTable, ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData, ObjectMenuItem::RunSqlFile]
                .into_iter()
                .chain(backup.then_some([ObjectMenuItem::Backup, ObjectMenuItem::Restore]).into_iter().flatten())
                .collect(),
            NavigatorNodeId::ObjectGroup { .. } => Vec::new(),
            _ => [ObjectMenuItem::NewTable, ObjectMenuItem::ExportData, ObjectMenuItem::TransferData, ObjectMenuItem::ImportData, ObjectMenuItem::RunSqlFile]
                .into_iter()
                .chain(backup.then_some([ObjectMenuItem::Backup, ObjectMenuItem::Restore]).into_iter().flatten())
                .chain(namespaces)
                .collect(),
        }
    }
}
