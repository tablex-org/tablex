//! The pending structure changes (D3) as they would look: the declared graph
//! with a [`Draft`] applied (no GPUI).
//!
//! The overlay never removes a column or a table: a dropped one stays, marked,
//! so the indices relations and unique keys hold stay valid and the user sees
//! what is going away. A renamed table keeps its catalog identity
//! (`DiagramEntity::table`: positions, colors, groups and logical relationships
//! are keyed by it) and shows its new name as the label. Changes are replayed
//! in execution order ([`Change::phase`]), the order `sql_ddl::plan` runs them.

use std::collections::HashMap;

use super::model::{
    ColumnState, DiagramColumn, DiagramEntity, DiagramGraph, DiagramIndex, DiagramRelation, EntityId, EntityState,
    RelationOrigin, TableRef, TypeGlyph,
};
use crate::app::structure_edit::EditScope;
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::schema::{LiveSchema, LiveTable};
use crate::sql_ddl::{Change, ColumnSpec, PrimaryKeyRef, TableId};

impl EditScope {
    /// The table as a change names it.
    pub(in crate::app) fn id(&self, table: &TableRef) -> TableId {
        self.table_id(&table.database, table.schema.as_deref(), &table.table)
    }

    /// The entity identity of a table the draft creates.
    pub(in crate::app) fn table_ref(&self, id: &TableId) -> TableRef {
        TableRef { database: self.database.clone(), schema: self.schema.clone(), table: id.name.clone() }
    }

    /// The id a new table of this diagram gets.
    pub(in crate::app) fn new_table(&self, name: &str) -> TableId {
        self.id(&self.table_ref(&TableId { schema: None, name: name.to_owned() }))
    }
}

/// Where a table stands against the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum TableState {
    #[default]
    Clean,
    /// A pending change touches it.
    Changed,
    /// Created by the draft.
    New,
    /// Dropped by the draft.
    Dropped,
}

impl TableState {
    pub(in crate::app) fn pending(self) -> bool {
        self != Self::Clean
    }
}

/// A column the way a new `DiagramColumn` shows it.
fn column_of(spec: &ColumnSpec, primary_key: bool, pending: ColumnState) -> DiagramColumn {
    DiagramColumn {
        name: spec.name.clone(),
        data_type: spec.data_type.clone(),
        glyph: TypeGlyph::for_type(&spec.data_type),
        primary_key,
        foreign_key: false,
        nullable: spec.nullable && !primary_key,
        default: spec.default.clone(),
        comment: spec.comment.clone(),
        auto_increment: spec.auto_increment,
        generated: spec.generated,
        attributes: spec.attributes.clone(),
        pending,
    }
}

impl DiagramEntity {
    /// The editable view of a column (what an `AlterColumn` starts from).
    pub(in crate::app) fn column_spec(&self, index: usize) -> Option<ColumnSpec> {
        let column = self.columns.get(index)?;
        Some(ColumnSpec {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            nullable: column.nullable,
            default: column.default.clone().filter(|default| !default.is_empty()),
            comment: column.comment.clone().filter(|comment| !comment.is_empty()),
            auto_increment: column.auto_increment,
            generated: column.generated,
            attributes: column.attributes.clone(),
        })
    }

    /// What the catalog holds of this (declared, not drafted) table, for the
    /// staleness checks. `foreign_keys` are the names of its foreign keys.
    pub(in crate::app) fn live_table(&self, foreign_keys: Vec<String>) -> LiveTable {
        let key: Vec<String> = self.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
        LiveTable {
            columns: (0..self.columns.len()).filter_map(|index| self.column_spec(index)).collect(),
            primary_key: (!key.is_empty()).then(|| PrimaryKeyRef { name: self.primary_key_name.clone(), columns: key }),
            indexes: self.indexes.iter().map(|index| index.name.clone()).collect(),
            foreign_keys,
        }
    }

    fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column.name == name)
    }

    fn set_primary_key(&mut self, names: &[String]) {
        let old: Vec<usize> =
            self.columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(index, _)| index).collect();
        for column in &mut self.columns {
            column.primary_key = names.contains(&column.name);
            // A key column holds no NULL.
            column.nullable &= !column.primary_key;
        }
        let new: Vec<usize> =
            self.columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(index, _)| index).collect();
        self.unique_keys.retain(|key| *key != old);
        if !new.is_empty() && !self.unique_keys.contains(&new) {
            self.unique_keys.push(new);
            self.unique_keys.sort();
        }
    }
}

impl DiagramGraph {
    /// This graph with `draft` applied, and the state of every entity of the
    /// result (the declared ones first, in their order, then the created ones).
    pub(in crate::app) fn with_draft(&self, draft: &Draft, scope: &EditScope) -> (DiagramGraph, Vec<TableState>) {
        let mut graph = self.clone();
        let mut states = vec![TableState::Clean; graph.entities.len()];
        // Entities by the name the draft uses for them.
        let mut by_name: HashMap<TableId, usize> =
            graph.entities.iter().enumerate().map(|(index, entity)| (scope.id(&entity.table), index)).collect();

        let mut order: Vec<&Change> = draft.changes.iter().collect();
        order.sort_by_key(|change| change.phase());
        for change in order {
            match change {
                Change::RenameTable { table, to } => {
                    let Some(index) = by_name.remove(table) else { continue };
                    let target = TableId { schema: table.schema.clone(), name: to.clone() };
                    let entity = &mut graph.entities[index];
                    entity.label = match entity.label.strip_suffix(table.name.as_str()) {
                        Some(prefix) => format!("{prefix}{to}"),
                        None => to.clone(),
                    };
                    by_name.insert(target, index);
                    states[index] = TableState::Changed;
                }
                Change::CreateTable { table, columns, primary_key, .. } => {
                    let columns: Vec<DiagramColumn> = columns
                        .iter()
                        .map(|spec| column_of(spec, primary_key.contains(&spec.name), ColumnState::Added))
                        .collect();
                    let key: Vec<usize> =
                        columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(index, _)| index).collect();
                    by_name.insert(table.clone(), graph.entities.len());
                    graph.entities.push(DiagramEntity {
                        table: scope.table_ref(table),
                        label: table.name.clone(),
                        columns,
                        state: EntityState::Ready,
                        focus: false,
                        unique_keys: if key.is_empty() { Vec::new() } else { vec![key] },
                        indexes: Vec::new(),
                        primary_key_name: None,
                    });
                    states.push(TableState::New);
                }
                other => {
                    let Some(&index) = other.table().and_then(|table| by_name.get(table)) else { continue };
                    graph.apply_one(index, other, &by_name);
                    states[index] = match (other, states[index]) {
                        (Change::DropTable { .. }, _) => TableState::Dropped,
                        (_, TableState::New) => TableState::New,
                        _ => TableState::Changed,
                    };
                }
            }
        }
        (graph, states)
    }

    fn apply_one(&mut self, index: usize, change: &Change, by_name: &HashMap<TableId, usize>) {
        let entity = &mut self.entities[index];
        match change {
            Change::AddColumn { column, .. } => {
                entity.columns.push(column_of(column, false, ColumnState::Added));
            }
            Change::DropColumn { column, .. } => {
                if let Some(at) = entity.column_index(column) {
                    entity.columns[at].pending = ColumnState::Dropped;
                }
            }
            Change::RenameColumn { from, to, .. } => {
                if let Some(at) = entity.column_index(from) {
                    entity.columns[at].name = to.clone();
                    entity.columns[at].pending = ColumnState::Changed;
                }
            }
            Change::AlterColumn { to, .. } => {
                if let Some(at) = entity.column_index(&to.name) {
                    let column = &mut entity.columns[at];
                    column.data_type = to.data_type.clone();
                    column.glyph = TypeGlyph::for_type(&to.data_type);
                    column.nullable = to.nullable;
                    column.default = to.default.clone();
                    column.comment = to.comment.clone();
                    column.pending = ColumnState::Changed;
                }
            }
            Change::SetPrimaryKey { columns, .. } => entity.set_primary_key(columns),
            Change::CreateIndex { index: spec, .. } => entity.indexes.push(DiagramIndex {
                name: spec.name.clone(),
                columns: spec.columns.clone(),
                unique: spec.unique,
                primary: false,
                method: None,
            }),
            Change::DropIndex { name, .. } => entity.indexes.retain(|index| index.name != *name),
            Change::AddForeignKey { foreign_key, .. } => {
                let Some(&parent) = by_name.get(&foreign_key.ref_table) else { return };
                let indices = |entity: &DiagramEntity, names: &[String]| -> Vec<usize> {
                    names.iter().filter_map(|name| entity.column_index(name)).collect()
                };
                let child_columns = indices(&self.entities[index], &foreign_key.columns);
                let parent_columns = indices(&self.entities[parent], &foreign_key.ref_columns);
                let child = &self.entities[index];
                let optional = child_columns.iter().any(|&at| child.columns[at].nullable);
                let unique = !child_columns.is_empty()
                    && child.unique_keys.iter().any(|key| !key.is_empty() && key.iter().all(|c| child_columns.contains(c)));
                // The draft key replaces the logical relationship it promotes.
                self.relations.retain(|relation| {
                    !(matches!(relation.origin, RelationOrigin::Logical(_))
                        && relation.child == EntityId(index)
                        && relation.parent == EntityId(parent)
                        && relation.child_columns == child_columns
                        && relation.parent_columns == parent_columns)
                });
                for at in &child_columns {
                    self.entities[index].columns[*at].foreign_key = true;
                }
                self.relations.push(DiagramRelation {
                    name: foreign_key.name.clone(),
                    child: EntityId(index),
                    child_columns,
                    parent: EntityId(parent),
                    parent_columns,
                    optional,
                    unique,
                    origin: RelationOrigin::Draft,
                });
            }
            Change::DropForeignKey { name, .. } => {
                self.relations.retain(|relation| {
                    !(relation.child == EntityId(index) && relation.name == *name && relation.origin == RelationOrigin::Declared)
                });
            }
            // Whole-table changes are the overlay's own business; the diagram has no view or namespace
            // actions (the navigator's object actions never reach it).
            Change::DropTable { .. }
            | Change::RenameTable { .. }
            | Change::CreateTable { .. }
            | Change::DropView { .. }
            | Change::RenameView { .. }
            | Change::TruncateTable { .. }
            | Change::CreateNamespace { .. }
            | Change::Account(_) => {}
        }
    }
}

impl DiagramGraph {
    /// What this graph's tables hold, for the staleness checks. `complete`: every
    /// table of the namespace is in the graph (a whole-schema diagram that has
    /// loaded everything), so a table that is missing is known to be absent.
    /// Tables still loading are left out (unknown).
    pub(in crate::app) fn live_schema(&self, scope: &EditScope, complete: bool) -> LiveSchema {
        let mut live = LiveSchema::new(complete);
        for (index, entity) in self.entities.iter().enumerate() {
            if entity.state != EntityState::Ready {
                continue;
            }
            let foreign_keys = self
                .relations
                .iter()
                .filter(|r| r.child == EntityId(index) && r.origin == RelationOrigin::Declared)
                .map(|r| r.name.clone())
                .collect();
            live.present(scope.id(&entity.table), entity.live_table(foreign_keys));
        }
        live
    }
}
