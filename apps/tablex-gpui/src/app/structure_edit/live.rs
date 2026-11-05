//! A described table (the driver's `TableProperties`) in the forms the structure editors compare
//! and edit: what the staleness checks read, and the column spec an `AlterColumn` starts from.

use crate::catalog::{CatalogColumn, CatalogConstraintKind, TableProperties};
use crate::sql_ddl::schema::LiveTable;
use crate::sql_ddl::{ColumnAttributes, ColumnSpec, PrimaryKeyRef};

/// The editable view of a catalog column (what an `AlterColumn` starts from).
pub(in crate::app) fn column_spec(column: &CatalogColumn) -> ColumnSpec {
    ColumnSpec {
        name: column.name.clone(),
        data_type: column.data_type.clone(),
        nullable: column.nullable,
        default: column.default_expression.clone().filter(|default| !default.is_empty()),
        comment: column.comment.clone().filter(|comment| !comment.is_empty()),
        auto_increment: column.auto_increment == Some(true),
        generated: column.is_generated == Some(true),
        attributes: column.attributes.as_ref().map(|a| ColumnAttributes {
            character_set: a.character_set.clone(),
            collation: a.collation.clone(),
            extra: a.extra.clone(),
            generation_expression: a.generation_expression.clone(),
        }),
    }
}

/// A described table in the form the staleness checks compare. Key columns come first, in the
/// order of the primary-key constraint (then the columns only flagged as key), the others after
/// them: the order the diagram's entities have, which the checks were written against.
pub(in crate::app) fn live_table(properties: &TableProperties) -> LiveTable {
    let primary_key = properties.keys.iter().flatten().find(|k| k.kind == CatalogConstraintKind::PrimaryKey);
    let key_names: Vec<&str> = primary_key.into_iter().flat_map(|k| k.columns.iter().map(String::as_str)).collect();
    let in_key = |column: &CatalogColumn| column.primary_key || key_names.contains(&column.name.as_str());
    let keyed = key_names.iter().filter_map(|name| properties.columns.iter().find(|c| c.name == *name));
    let flagged = properties.columns.iter().filter(|c| c.primary_key && !key_names.contains(&c.name.as_str()));
    let key: Vec<&CatalogColumn> = keyed.chain(flagged).collect();
    let rest = properties.columns.iter().filter(|c| !in_key(c));
    LiveTable {
        columns: key.iter().copied().chain(rest).map(column_spec).collect(),
        primary_key: (!key.is_empty()).then(|| PrimaryKeyRef {
            name: primary_key.map(|k| k.name.clone()),
            columns: key.iter().map(|c| c.name.clone()).collect(),
        }),
        indexes: properties.indexes.iter().flatten().map(|index| index.name.clone()).collect(),
        foreign_keys: properties.foreign_keys.iter().flatten().map(|fk| fk.name.clone()).collect(),
    }
}
