//! "Copy JOIN SQL": a SELECT joining the tables of one relation, or of every
//! relation of one entity, quoted for the connection's dialect (no GPUI).

use super::model::{DiagramGraph, DiagramRelation, EntityId};
use crate::sql_table::{qualified_table, SqlDialect};

/// One `JOIN`: the joined table (by alias `t<n>`) and its ON conditions.
struct Join {
    kind: &'static str,
    table: String,
    alias: usize,
    conditions: Vec<String>,
}

/// `alias."column"` pairs of the relation, `(child side, parent side)`.
/// `None` while a column list is unknown (the parent is still loading).
fn column_pairs<'a>(graph: &'a DiagramGraph, relation: &DiagramRelation) -> Option<Vec<(&'a str, &'a str)>> {
    let child = &graph.entities[relation.child.0].columns;
    let parent = &graph.entities[relation.parent.0].columns;
    if relation.child_columns.is_empty() || relation.child_columns.len() != relation.parent_columns.len() {
        return None;
    }
    Some(
        relation
            .child_columns
            .iter()
            .zip(&relation.parent_columns)
            .map(|(&c, &p)| (child[c].name.as_str(), parent[p].name.as_str()))
            .collect(),
    )
}

/// `None` for a name the dialect cannot quote unambiguously (a backslash on ClickHouse): no SQL is offered.
fn table_name(graph: &DiagramGraph, entity: EntityId, dialect: SqlDialect) -> Option<String> {
    let table = &graph.entities[entity.0].table;
    qualified_table(dialect, table.schema.as_deref(), &table.table).ok()
}

fn render(graph: &DiagramGraph, root: EntityId, joins: &[Join], dialect: SqlDialect) -> Option<String> {
    let mut sql = format!("SELECT *\nFROM {} AS t1", table_name(graph, root, dialect)?);
    for join in joins {
        sql.push_str(&format!(
            "\n{} {} AS t{}\n  ON {}",
            join.kind,
            join.table,
            join.alias,
            join.conditions.join(" AND ")
        ));
    }
    Some(sql)
}

/// Joins `relation`'s child to its parent (INNER when the FK is mandatory,
/// LEFT when it is nullable). `None` when its columns are not known yet.
pub(in crate::app) fn relation_join(graph: &DiagramGraph, relation: usize, dialect: SqlDialect) -> Option<String> {
    let relation = graph.relations.get(relation)?;
    let pairs = column_pairs(graph, relation)?;
    let conditions = pairs.iter().map(|(c, p)| format!("t1.{} = t2.{}", dialect.quote(c), dialect.quote(p))).collect();
    let join = Join {
        kind: if relation.optional { "LEFT JOIN" } else { "INNER JOIN" },
        table: table_name(graph, relation.parent, dialect)?,
        alias: 2,
        conditions,
    };
    render(graph, relation.child, &[join], dialect)
}

/// Joins `entity` to the other end of each of its relations, one join (and
/// alias) per relation so a self reference or two FKs to one table stay
/// distinct. Parents are INNER/LEFT by nullability; children are LEFT.
/// `None` when no relation has known columns.
pub(in crate::app) fn entity_join(graph: &DiagramGraph, entity: EntityId, dialect: SqlDialect) -> Option<String> {
    let mut joins: Vec<Join> = Vec::new();
    for relation in graph.relations.iter().filter(|r| r.child == entity || r.parent == entity) {
        let Some(pairs) = column_pairs(graph, relation) else {
            continue;
        };
        let alias = joins.len() + 2;
        // Outgoing FK (a self reference counts as outgoing).
        let outgoing = relation.child == entity;
        let (other, kind) = if outgoing {
            (relation.parent, if relation.optional { "LEFT JOIN" } else { "INNER JOIN" })
        } else {
            (relation.child, "LEFT JOIN")
        };
        let conditions = pairs
            .iter()
            .map(|(c, p)| {
                let (root_column, other_column) = if outgoing { (c, p) } else { (p, c) };
                format!("t1.{} = t{alias}.{}", dialect.quote(root_column), dialect.quote(other_column))
            })
            .collect();
        joins.push(Join { kind, table: table_name(graph, other, dialect)?, alias, conditions });
    }
    if joins.is_empty() {
        return None;
    }
    render(graph, entity, &joins, dialect)
}
