//! The pending change-set of one diagram (no GPUI): [`Draft::push`] adds an
//! edit and folds it into what is already pending, so the list stays minimal
//! and valid however the user edits.
//!
//! * Edits of a table created in the draft are folded into its `CreateTable`.
//! * Editing a column twice is one `AlterColumn` (the first `from`, the last
//!   `to`); undoing the edit removes the change.
//! * Adding then dropping, or creating then dropping, leaves nothing.
//! * A rename rewrites the names every other pending change uses, and renaming
//!   back removes the rename.
//!
//! Every change names its table by the *latest* name (see [`Change`]), which is
//! what lets [`super::plan`] run all table renames first.

use serde::{Deserialize, Serialize};

use super::{Change, ColumnSpec, ForeignKeySpec, IndexSpec, PrimaryKeyRef, TableId};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Draft {
    pub changes: Vec<Change>,
}

impl Draft {
    pub fn from_changes(changes: Vec<Change>) -> Self {
        Self { changes }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// The `CreateTable` of `table`, if the draft creates it.
    pub fn creation(&self, table: &TableId) -> Option<usize> {
        self.changes
            .iter()
            .position(|change| matches!(change, Change::CreateTable { table: created, .. } if created == table))
    }

    /// Whether any pending change is about `table`.
    pub fn touches(&self, table: &TableId) -> bool {
        self.changes.iter().any(|change| change.table() == Some(table))
    }

    /// The draft without `applied` (each removed once, wherever it now is):
    /// what stays is what was added or edited while those were running.
    pub fn without_changes(&self, applied: &[Change]) -> Self {
        let mut changes = self.changes.clone();
        for change in applied {
            if let Some(at) = changes.iter().position(|pending| pending == change) {
                changes.remove(at);
            }
        }
        Self { changes }
    }

    // ---- push ----

    /// Adds `change`, folding it into the pending ones.
    pub fn push(&mut self, change: Change) {
        match change {
            Change::CreateTable { ref table, .. } => {
                let table = table.clone();
                self.changes.retain(|existing| !matches!(existing, Change::CreateTable { table: t, .. } if *t == table));
                self.changes.push(change);
            }
            Change::DropTable { table, cascade } => self.push_drop_table(table, cascade),
            Change::RenameTable { table, to } => self.push_rename_table(table, to),
            // Object actions of the navigator: one action is one change, asking twice is asking once.
            Change::DropView { .. } | Change::RenameView { .. } | Change::TruncateTable { .. } | Change::CreateNamespace { .. } | Change::Account(_) => {
                if !self.changes.contains(&change) {
                    self.changes.push(change);
                }
            }
            Change::AddColumn { table, column } => self.push_add_column(table, column),
            Change::DropColumn { table, column } => self.push_drop_column(table, column),
            Change::RenameColumn { table, from, to } => self.push_rename_column(table, from, to),
            Change::AlterColumn { table, from, to } => self.push_alter_column(table, from, to),
            Change::SetPrimaryKey { table, columns, previous } => self.push_primary_key(table, columns, previous),
            Change::CreateIndex { table, index } => self.push_create_index(table, index),
            Change::DropIndex { table, name } => self.push_drop_index(table, name),
            Change::AddForeignKey { table, foreign_key } => self.push_add_foreign_key(table, foreign_key),
            Change::DropForeignKey { table, name } => self.push_drop_foreign_key(table, name),
        }
    }

    fn push_drop_table(&mut self, table: TableId, cascade: bool) {
        let created = self.creation(&table).is_some();
        // A table renamed earlier is dropped under the name it has in the database.
        let original = match self.renamed_from(&table) {
            Some((at, original)) if !created => {
                self.changes.remove(at);
                original
            }
            _ => table.clone(),
        };
        // A `RenameTable` names the *old* table: when this one is a new table that reuses
        // that name, the rename is about another table and stays.
        self.changes.retain(|change| {
            !(change.table() == Some(&table) && !(created && matches!(change, Change::RenameTable { .. })))
                && !refers_to_table(change, &table)
        });
        if !created {
            self.changes.push(Change::DropTable { table: original, cascade });
        }
    }

    fn push_rename_table(&mut self, table: TableId, to: String) {
        let target = TableId { schema: table.schema.clone(), name: to };
        if target == table {
            return;
        }
        if self.creation(&table).is_some() {
            self.rewrite_table(&table, &target);
            return;
        }
        match self.renamed_from(&table) {
            Some((at, original)) => {
                self.rewrite_table(&table, &target);
                if original == target {
                    self.changes.remove(at);
                } else if let Change::RenameTable { to, .. } = &mut self.changes[at] {
                    *to = target.name;
                }
            }
            None => {
                self.rewrite_table(&table, &target);
                self.changes.push(Change::RenameTable { table, to: target.name });
            }
        }
    }

    /// `(index, original table)` of the pending rename that gave `table` its name.
    fn renamed_from(&self, table: &TableId) -> Option<(usize, TableId)> {
        self.changes.iter().enumerate().find_map(|(at, change)| match change {
            Change::RenameTable { table: original, to } if original.schema == table.schema && *to == table.name => {
                Some((at, original.clone()))
            }
            _ => None,
        })
    }

    fn push_add_column(&mut self, table: TableId, column: ColumnSpec) {
        if let Some(at) = self.creation(&table) {
            if let Change::CreateTable { columns, .. } = &mut self.changes[at] {
                match columns.iter_mut().find(|existing| existing.name == column.name) {
                    Some(existing) => *existing = column,
                    None => columns.push(column),
                }
            }
            return;
        }
        match self.changes.iter_mut().find_map(|change| match change {
            Change::AddColumn { table: t, column: c } if *t == table && c.name == column.name => Some(c),
            _ => None,
        }) {
            Some(existing) => *existing = column,
            None => self.changes.push(Change::AddColumn { table, column }),
        }
    }

    fn push_drop_column(&mut self, table: TableId, column: String) {
        if let Some(at) = self.creation(&table) {
            if let Change::CreateTable { columns, primary_key, .. } = &mut self.changes[at] {
                columns.retain(|existing| existing.name != column);
                primary_key.retain(|name| *name != column);
            }
            self.forget_column_uses(&table, &column);
            return;
        }
        if let Some(at) = self
            .changes
            .iter()
            .position(|change| matches!(change, Change::AddColumn { table: t, column: c } if *t == table && c.name == column))
        {
            self.changes.remove(at);
            self.forget_column_uses(&table, &column);
            return;
        }
        // An existing column, maybe renamed: it is dropped under its database name.
        let original = match self.changes.iter().position(
            |change| matches!(change, Change::RenameColumn { table: t, to, .. } if *t == table && *to == column),
        ) {
            Some(at) => match self.changes.remove(at) {
                Change::RenameColumn { from, .. } => from,
                _ => column.clone(),
            },
            None => column.clone(),
        };
        self.changes
            .retain(|change| !matches!(change, Change::AlterColumn { table: t, to, .. } if *t == table && to.name == column));
        self.forget_column_uses(&table, &column);
        self.changes.push(Change::DropColumn { table, column: original });
    }

    /// Removes the pending indexes and foreign keys that use `column` of
    /// `table`, and takes it out of a pending primary key.
    fn forget_column_uses(&mut self, table: &TableId, column: &str) {
        self.changes.retain(|change| match change {
            Change::CreateIndex { table: t, index } => !(t == table && index.columns.iter().any(|c| c == column)),
            Change::AddForeignKey { table: t, foreign_key } => {
                !((t == table && foreign_key.columns.iter().any(|c| c == column))
                    || (foreign_key.ref_table == *table && foreign_key.ref_columns.iter().any(|c| c == column)))
            }
            _ => true,
        });
        // A primary key that loses its last pending column is not "drop the key": the
        // change goes (the key stays as it is in the database). Nor does a key that is back to
        // the one the database has stay: it would replace the key with itself.
        self.changes.retain_mut(|change| match change {
            Change::SetPrimaryKey { table: t, columns, previous } if t == table => {
                let before = columns.len();
                columns.retain(|c| c != column);
                let emptied = columns.is_empty() && before > 0;
                let restored = columns.len() != before && previous.as_ref().is_some_and(|key| key.columns == *columns);
                !(emptied || restored)
            }
            _ => true,
        });
    }

    fn push_rename_column(&mut self, table: TableId, from: String, to: String) {
        if from == to {
            return;
        }
        if let Some(at) = self.creation(&table) {
            if let Change::CreateTable { columns, primary_key, .. } = &mut self.changes[at] {
                for column in columns.iter_mut().filter(|column| column.name == from) {
                    column.name = to.clone();
                }
                for key in primary_key.iter_mut().filter(|key| **key == from) {
                    *key = to.clone();
                }
            }
            self.rewrite_column(&table, &from, &to);
            return;
        }
        if let Some(Change::AddColumn { column, .. }) = self
            .changes
            .iter_mut()
            .find(|change| matches!(change, Change::AddColumn { table: t, column: c } if *t == table && c.name == from))
        {
            column.name = to.clone();
            self.rewrite_column(&table, &from, &to);
            return;
        }
        let earlier = self.changes.iter().position(
            |change| matches!(change, Change::RenameColumn { table: t, to: t_to, .. } if *t == table && *t_to == from),
        );
        self.rewrite_column(&table, &from, &to);
        match earlier {
            Some(at) => {
                if let Change::RenameColumn { from: original, to: renamed, .. } = &mut self.changes[at] {
                    if *original == to {
                        self.changes.remove(at);
                    } else {
                        *renamed = to;
                    }
                }
            }
            None => self.changes.push(Change::RenameColumn { table, from, to }),
        }
    }

    fn push_alter_column(&mut self, table: TableId, from: ColumnSpec, to: ColumnSpec) {
        if from == to {
            return;
        }
        if let Some(at) = self.creation(&table) {
            if let Change::CreateTable { columns, .. } = &mut self.changes[at] {
                if let Some(existing) = columns.iter_mut().find(|column| column.name == to.name) {
                    *existing = to;
                }
            }
            return;
        }
        if let Some(Change::AddColumn { column, .. }) = self
            .changes
            .iter_mut()
            .find(|change| matches!(change, Change::AddColumn { table: t, column: c } if *t == table && c.name == to.name))
        {
            *column = to;
            return;
        }
        let earlier = self.changes.iter().position(
            |change| matches!(change, Change::AlterColumn { table: t, to: t_to, .. } if *t == table && t_to.name == to.name),
        );
        match earlier {
            Some(at) => {
                let Change::AlterColumn { from: original, .. } = &self.changes[at] else {
                    return;
                };
                let original = original.clone();
                if original == to {
                    self.changes.remove(at);
                } else {
                    self.changes[at] = Change::AlterColumn { table, from: original, to };
                }
            }
            None => self.changes.push(Change::AlterColumn { table, from, to }),
        }
    }

    fn push_primary_key(&mut self, table: TableId, columns: Vec<String>, previous: Option<PrimaryKeyRef>) {
        if let Some(at) = self.creation(&table) {
            if let Change::CreateTable { primary_key, .. } = &mut self.changes[at] {
                *primary_key = columns;
            }
            return;
        }
        let earlier = self.changes.iter().position(|change| matches!(change, Change::SetPrimaryKey { table: t, .. } if *t == table));
        let previous = match earlier {
            Some(at) => match self.changes.remove(at) {
                Change::SetPrimaryKey { previous, .. } => previous,
                _ => previous,
            },
            None => previous,
        };
        let unchanged = previous.as_ref().map_or(columns.is_empty(), |previous| previous.columns == columns);
        if !unchanged {
            self.changes.push(Change::SetPrimaryKey { table, columns, previous });
        }
    }

    fn push_create_index(&mut self, table: TableId, index: IndexSpec) {
        self.changes.retain(|change| !matches!(change, Change::CreateIndex { table: t, index: i } if *t == table && i.name == index.name));
        self.changes.push(Change::CreateIndex { table, index });
    }

    fn push_drop_index(&mut self, table: TableId, name: String) {
        let pending = self
            .changes
            .iter()
            .position(|change| matches!(change, Change::CreateIndex { table: t, index } if *t == table && index.name == name));
        match pending {
            Some(at) => {
                self.changes.remove(at);
            }
            None => {
                if !self.changes.iter().any(|change| matches!(change, Change::DropIndex { table: t, name: n } if *t == table && *n == name)) {
                    self.changes.push(Change::DropIndex { table, name });
                }
            }
        }
    }

    fn push_add_foreign_key(&mut self, table: TableId, foreign_key: ForeignKeySpec) {
        self.changes.retain(
            |change| !matches!(change, Change::AddForeignKey { table: t, foreign_key: f } if *t == table && f.name == foreign_key.name),
        );
        self.changes.push(Change::AddForeignKey { table, foreign_key });
    }

    fn push_drop_foreign_key(&mut self, table: TableId, name: String) {
        let pending = self
            .changes
            .iter()
            .position(|change| matches!(change, Change::AddForeignKey { table: t, foreign_key } if *t == table && foreign_key.name == name));
        match pending {
            Some(at) => {
                self.changes.remove(at);
            }
            None => {
                if !self.changes.iter().any(|change| matches!(change, Change::DropForeignKey { table: t, name: n } if *t == table && *n == name)) {
                    self.changes.push(Change::DropForeignKey { table, name });
                }
            }
        }
    }

    // ---- discard ----

    /// Removes the change at `index` with whatever only made sense with it.
    /// Returns `false` when there is no such change.
    pub fn discard(&mut self, index: usize) -> bool {
        let Some(change) = self.changes.get(index).cloned() else {
            return false;
        };
        match change {
            Change::CreateTable { table, .. } => self.push_drop_table(table, false),
            Change::RenameTable { table, to } => {
                let renamed = TableId { schema: table.schema.clone(), name: to };
                self.changes.remove(index);
                self.rewrite_table(&renamed, &table);
            }
            Change::RenameColumn { table, from, to } => {
                self.changes.remove(index);
                self.rewrite_column(&table, &to, &from);
            }
            Change::AddColumn { table, column } => {
                self.changes.remove(index);
                self.forget_column_uses(&table, &column.name);
            }
            _ => {
                self.changes.remove(index);
            }
        }
        true
    }

    // ---- rewrites ----

    /// Every change about `from` (and every foreign key that points at it) uses `to`.
    fn rewrite_table(&mut self, from: &TableId, to: &TableId) {
        for change in &mut self.changes {
            if change.table() == Some(from) && !matches!(change, Change::RenameTable { .. }) {
                if let Some(table) = change.table_mut() {
                    *table = to.clone();
                }
            }
            if let Change::AddForeignKey { foreign_key, .. } = change {
                if foreign_key.ref_table == *from {
                    foreign_key.ref_table = to.clone();
                }
            }
        }
    }

    /// Every pending use of column `from` of `table` uses `to`.
    fn rewrite_column(&mut self, table: &TableId, from: &str, to: &str) {
        let rename = |name: &mut String| {
            if name == from {
                *name = to.to_owned();
            }
        };
        for change in &mut self.changes {
            match change {
                Change::AlterColumn { table: t, from: before, to: after } if t == table => {
                    rename(&mut before.name);
                    rename(&mut after.name);
                }
                // `previous` names the key's columns as the replay sees them at this step, after the
                // renames of the change-set, so it follows the rename too.
                Change::SetPrimaryKey { table: t, columns, previous } if t == table => {
                    columns.iter_mut().for_each(rename);
                    if let Some(previous) = previous {
                        previous.columns.iter_mut().for_each(rename);
                    }
                }
                Change::CreateIndex { table: t, index } if t == table => index.columns.iter_mut().for_each(rename),
                Change::AddForeignKey { table: t, foreign_key } => {
                    if t == table {
                        foreign_key.columns.iter_mut().for_each(rename);
                    }
                    if foreign_key.ref_table == *table {
                        foreign_key.ref_columns.iter_mut().for_each(rename);
                    }
                }
                _ => {}
            }
        }
    }
}

/// A foreign key (of another table) that points at `table`.
fn refers_to_table(change: &Change, table: &TableId) -> bool {
    matches!(change, Change::AddForeignKey { foreign_key, .. } if foreign_key.ref_table == *table)
}
