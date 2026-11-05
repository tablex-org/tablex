//! Inferred relationships: candidate foreign keys between the tables of a
//! diagram, found by naming convention and type match (no GPUI, no I/O).
//!
//! A column `<table>_id`, `<table>Id` or `<table>ID` (singular or plural
//! table name, any case, `uid` / `uuid` / `guid` / `pk` suffixes too) that is
//! not part of a foreign key yet points at that table's primary key (else a
//! column of the same name, else `id`). A pair whose types are incompatible is
//! dropped; the rest gets a confidence from how exact the name, the target
//! and the type are. Name, target and type are matched with graded strategies.

use std::collections::HashMap;

use super::annotations::{Annotations, IgnoredLink, RelationEnd};
use super::model::{DiagramColumn, DiagramEntity, DiagramGraph, EntityId, EntityState, TypeGlyph};

/// Most suggestions returned (the panel lists them all, so bound the list).
pub(in crate::app) const MAX_SUGGESTIONS: usize = 500;

/// How the column name pointed at the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum NameMatch {
    /// The column spells the table name (`orders` / `order_id`... the table
    /// name itself, only the key suffix added).
    Table,
    /// The column spells the singular or plural of the table name.
    Singular,
}

/// How well the column types agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum TypeMatch {
    /// The same base type (`integer` / `INTEGER`, `varchar(10)` / `varchar(40)`).
    Same,
    /// The same kind of type (`integer` and `bigint`).
    Compatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Level {
    High,
    Medium,
    Low,
}

impl Level {
    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        }
    }
}

/// One candidate foreign key: `child.child_column` references
/// `parent.parent_column`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::app) struct Suggestion {
    pub(in crate::app) child: EntityId,
    pub(in crate::app) child_column: usize,
    pub(in crate::app) parent: EntityId,
    pub(in crate::app) parent_column: usize,
    /// 0.0..=1.0.
    pub(in crate::app) confidence: f32,
    pub(in crate::app) name: NameMatch,
    pub(in crate::app) types: TypeMatch,
}

impl Suggestion {
    pub(in crate::app) fn level(&self) -> Level {
        if self.confidence >= 0.8 {
            Level::High
        } else if self.confidence >= 0.6 {
            Level::Medium
        } else {
            Level::Low
        }
    }

    /// The pair as the store identifies it (for ignoring).
    pub(in crate::app) fn link(&self, graph: &DiagramGraph) -> IgnoredLink {
        let end = |entity: EntityId, column: usize| {
            let e = &graph.entities[entity.0];
            RelationEnd {
                schema: e.table.schema.clone(),
                table: e.table.table.clone(),
                columns: vec![e.columns[column].name.clone()],
            }
        };
        IgnoredLink { child: end(self.child, self.child_column), parent: end(self.parent, self.parent_column) }
    }
}

/// Lower-case alphanumerics only: `Media_Type`, `mediaType` and `MEDIATYPE`
/// fold to the same text.
fn fold(name: &str) -> String {
    name.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// Singular of a folded English table name (`categories` -> `category`).
fn singular(name: &str) -> String {
    for (many, one) in [("people", "person"), ("children", "child")] {
        if let Some(head) = name.strip_suffix(many) {
            return format!("{head}{one}");
        }
    }
    let strip = |suffix: &str| name.strip_suffix(suffix).filter(|rest| !rest.is_empty());
    if let Some(rest) = strip("ies") {
        return format!("{rest}y");
    }
    for suffix in ["sses", "shes", "ches", "xes", "zes"] {
        if let Some(rest) = strip(suffix) {
            return format!("{rest}{}", &suffix[..suffix.len() - 2]);
        }
    }
    match strip("s") {
        Some(rest) if !name.ends_with("ss") && !name.ends_with("us") && !name.ends_with("is") => rest.to_owned(),
        _ => name.to_owned(),
    }
}

/// The table a folded column name points at: the name without its key suffix.
fn key_stem(folded: &str) -> Option<&str> {
    ["uuid", "guid", "uid", "id", "pk"]
        .iter()
        .find_map(|suffix| folded.strip_suffix(suffix))
        .filter(|stem| !stem.is_empty())
}

/// The type without length, precision and modifiers, lower-case.
fn base_type(data_type: &str) -> String {
    let end = data_type.find(['(', ' ', '[']).unwrap_or(data_type.len());
    data_type[..end].trim().to_lowercase()
}

fn type_match(child: &DiagramColumn, parent: &DiagramColumn) -> Option<TypeMatch> {
    if child.data_type.is_empty() || parent.data_type.is_empty() {
        return None;
    }
    if base_type(&child.data_type) == base_type(&parent.data_type) {
        return Some(TypeMatch::Same);
    }
    (child.glyph == parent.glyph && child.glyph != TypeGlyph::Other).then_some(TypeMatch::Compatible)
}

/// The column of `parent` a key named `child_column` would reference, and
/// whether that column is the parent's only primary key column.
fn target_column(parent: &DiagramEntity, child_column: &str) -> Option<(usize, bool)> {
    let keys: Vec<usize> =
        parent.columns.iter().enumerate().filter(|(_, c)| c.primary_key).map(|(i, _)| i).collect();
    if let [only] = keys.as_slice() {
        return Some((*only, true));
    }
    let wanted = fold(child_column);
    let same_name = parent.columns.iter().position(|c| fold(&c.name) == wanted);
    same_name.or_else(|| parent.columns.iter().position(|c| fold(&c.name) == "id")).map(|i| (i, false))
}

/// Candidate foreign keys among the entities of `graph`, best first: columns
/// that are neither part of a relationship (declared or logical) nor dismissed.
pub(in crate::app) fn suggest(graph: &DiagramGraph, annotations: &Annotations) -> Vec<Suggestion> {
    // Folded plural and singular table names -> entities.
    let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, entity) in graph.entities.iter().enumerate().filter(|(_, e)| e.state == EntityState::Ready) {
        let folded = fold(&entity.table.table);
        let one = singular(&folded);
        if one != folded {
            by_name.entry(one).or_default().push(i);
        }
        by_name.entry(folded).or_default().push(i);
    }
    let mut related = vec![Vec::new(); graph.entities.len()];
    for relation in &graph.relations {
        related[relation.child.0].extend(relation.child_columns.iter().copied());
    }

    let mut found = Vec::new();
    for (child_index, child) in graph.entities.iter().enumerate().filter(|(_, e)| e.state == EntityState::Ready) {
        for (column_index, column) in child.columns.iter().enumerate() {
            if column.foreign_key || related[child_index].contains(&column_index) {
                continue;
            }
            let folded = fold(&column.name);
            let Some(stem) = key_stem(&folded) else {
                continue;
            };
            let Some(candidates) = [singular(stem), stem.to_owned(), format!("{stem}s"), format!("{stem}es")]
                .iter()
                .find_map(|key| by_name.get(key))
            else {
                continue;
            };
            for &parent_index in candidates.iter().filter(|&&p| p != child_index) {
                let parent = &graph.entities[parent_index];
                let Some((parent_column, sole_key)) = target_column(parent, &column.name) else {
                    continue;
                };
                let Some(types) = type_match(column, &parent.columns[parent_column]) else {
                    continue;
                };
                let table_folded = fold(&parent.table.table);
                let name = if stem == table_folded { NameMatch::Table } else { NameMatch::Singular };
                let mut confidence: f32 = match name {
                    NameMatch::Table => 0.55,
                    NameMatch::Singular => 0.5,
                };
                confidence += if sole_key { 0.2 } else { 0.05 };
                if fold(&parent.columns[parent_column].name) == folded {
                    confidence += 0.1;
                }
                confidence += match types {
                    TypeMatch::Same => 0.15,
                    TypeMatch::Compatible => 0.08,
                };
                let suggestion = Suggestion {
                    child: EntityId(child_index),
                    child_column: column_index,
                    parent: EntityId(parent_index),
                    parent_column,
                    confidence: confidence.min(0.99),
                    name,
                    types,
                };
                if !annotations.is_ignored(&suggestion.link(graph)) {
                    found.push(suggestion);
                }
            }
        }
    }
    found.sort_by(|a, b| {
        b.confidence
            .total_cmp(&a.confidence)
            .then(a.child.cmp(&b.child))
            .then(a.child_column.cmp(&b.child_column))
            .then(a.parent.cmp(&b.parent))
    });
    found.truncate(MAX_SUGGESTIONS);
    found
}
