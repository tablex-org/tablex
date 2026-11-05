//! Pure diagram graph (no GPUI). Owner: DG-M.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::catalog::{CatalogConstraintKind, CatalogForeignKey, TableProperties};

/// A table identity inside one connection. `schema` is `None` for engines
/// without a schema level (SQLite, MySQL/MariaDB database-as-schema,
/// ClickHouse).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::app) struct TableRef {
    pub(in crate::app) database: String,
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: String,
}

/// Index into `DiagramGraph::entities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(in crate::app) struct EntityId(pub(in crate::app) usize);

/// Glyph shown before a column name (`123`, `A-Z`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum TypeGlyph {
    Numeric,
    Text,
    Temporal,
    Boolean,
    Binary,
    Json,
    Other,
}

/// Where a column stands against the database: a pending structure edit
/// (D3) marks it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app) enum ColumnState {
    #[default]
    Existing,
    /// Added by a pending change.
    Added,
    /// Renamed or altered by a pending change.
    Changed,
    /// Dropped by a pending change.
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct DiagramColumn {
    pub(in crate::app) name: String,
    pub(in crate::app) data_type: String,
    pub(in crate::app) glyph: TypeGlyph,
    pub(in crate::app) primary_key: bool,
    /// Part of at least one outgoing foreign key of this entity.
    pub(in crate::app) foreign_key: bool,
    pub(in crate::app) nullable: bool,
    /// Default expression reported by the catalog (inspector).
    pub(in crate::app) default: Option<String>,
    /// The column's comment, when the catalog has one.
    pub(in crate::app) comment: Option<String>,
    /// Restated by MySQL's `MODIFY COLUMN`.
    pub(in crate::app) auto_increment: bool,
    /// A generated column (its expression is unknown, so it is not restated).
    pub(in crate::app) generated: bool,
    /// MySQL / MariaDB: what restating the column needs (character set, `EXTRA`, ...).
    pub(in crate::app) attributes: Option<crate::sql_ddl::ColumnAttributes>,
    pub(in crate::app) pending: ColumnState,
}

/// One index of an entity, as the catalog reports it (inspector).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct DiagramIndex {
    pub(in crate::app) name: String,
    pub(in crate::app) columns: Vec<String>,
    pub(in crate::app) unique: bool,
    pub(in crate::app) primary: bool,
    pub(in crate::app) method: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum EntityState {
    /// Related table whose properties are still being fetched.
    Loading,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct DiagramEntity {
    pub(in crate::app) table: TableRef,
    /// Display name (schema-qualified only when it differs from the focus).
    pub(in crate::app) label: String,
    /// Primary-key columns first (in key order), then the rest in table order.
    pub(in crate::app) columns: Vec<DiagramColumn>,
    pub(in crate::app) state: EntityState,
    pub(in crate::app) focus: bool,
    /// Column sets (indices into `columns`) that identify a row: the primary
    /// key, unique constraints and unique indexes. Empty while unknown.
    pub(in crate::app) unique_keys: Vec<Vec<usize>>,
    /// Indexes of the table; empty while unknown.
    pub(in crate::app) indexes: Vec<DiagramIndex>,
    /// Name of the primary key constraint, when the catalog reported one
    /// (PostgreSQL needs it to replace the key).
    pub(in crate::app) primary_key_name: Option<String>,
}

/// Where a relation comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum RelationOrigin {
    /// A foreign key of the catalog.
    #[default]
    Declared,
    /// A user-declared (logical) relationship with this id; kept only in the
    /// diagram store, never written to the database.
    Logical(u64),
    /// A foreign key a pending structure change would create (D3); not in
    /// the database yet.
    Draft,
}

/// One foreign key: `child.columns` reference `parent.columns`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct DiagramRelation {
    pub(in crate::app) name: String,
    pub(in crate::app) child: EntityId,
    /// Indices into the child entity's `columns`.
    pub(in crate::app) child_columns: Vec<usize>,
    pub(in crate::app) parent: EntityId,
    /// Indices into the parent entity's `columns` (may be empty while the
    /// parent is still loading).
    pub(in crate::app) parent_columns: Vec<usize>,
    /// Some child FK column is nullable (or unknown, while the child loads):
    /// a child row may have no parent.
    pub(in crate::app) optional: bool,
    /// The FK columns include a unique key of the child: at most one child
    /// row per parent row (one-to-one).
    pub(in crate::app) unique: bool,
    pub(in crate::app) origin: RelationOrigin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::app) struct DiagramGraph {
    pub(in crate::app) entities: Vec<DiagramEntity>,
    pub(in crate::app) relations: Vec<DiagramRelation>,
    /// Schema graphs only: foreign keys whose referenced table is outside the
    /// shown set (not drawn).
    pub(in crate::app) external_relations: usize,
}

/// What `data` knows about one related table.
#[derive(Debug, Clone)]
pub(in crate::app) enum RelatedProperties {
    Loading,
    Ready(Arc<TableProperties>),
    Failed(String),
}

/// One foreign key between two tables, before entity ids exist.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RelationSpec {
    child: TableRef,
    parent: TableRef,
    child_columns: Vec<String>,
    parent_columns: Vec<String>,
    name: String,
}

impl TableRef {
    /// Resolves a driver-supplied table name (bare, `schema.table`, optionally
    /// quoted with `"…"` / `` `…` `` / `[…]`) relative to `focus`: same
    /// database, schema from the qualifier else the focus schema.
    fn resolve(raw: &str, focus: &TableRef) -> Self {
        let mut parts = split_qualified_name(raw);
        let table = parts.pop().unwrap_or_default();
        let schema = match parts.pop() {
            // Database-as-schema engines may qualify with the database name.
            Some(qualifier) if focus.schema.is_none() && qualifier == focus.database => None,
            Some(qualifier) => Some(qualifier),
            None => focus.schema.clone(),
        };
        Self {
            database: focus.database.clone(),
            schema,
            table,
        }
    }
}

/// Splits `a.b.c` on unquoted dots and strips quoting (doubled quote chars
/// inside a quoted part are unescaped).
fn split_qualified_name(raw: &str) -> Vec<String> {
    let mut parts = Vec::with_capacity(2);
    let mut current = String::new();
    let mut chars = raw.trim().chars().peekable();
    while let Some(c) = chars.next() {
        let close = match c {
            '"' | '`' => c,
            '[' => ']',
            '.' => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            _ => {
                current.push(c);
                continue;
            }
        };
        while let Some(q) = chars.next() {
            if q != close {
                current.push(q);
            } else if chars.next_if_eq(&close).is_some() {
                current.push(close);
            } else {
                break;
            }
        }
    }
    parts.push(current);
    parts
}

impl RelationSpec {
    fn new(child: TableRef, parent: TableRef, fk: &CatalogForeignKey) -> Self {
        Self {
            child,
            parent,
            child_columns: fk.columns.clone(),
            parent_columns: fk.referenced_columns.clone(),
            name: fk.name.clone(),
        }
    }
}

/// `(child, parent, fk)` for the FKs `owner` declares and the FKs of other
/// tables that reference it; names are resolved relative to `owner`.
fn fk_edges<'a>(
    owner: &'a TableRef,
    properties: &'a TableProperties,
) -> impl Iterator<Item = (TableRef, TableRef, &'a CatalogForeignKey)> + 'a {
    let outgoing = properties.foreign_keys.iter().flatten().map(|fk| {
        (
            owner.clone(),
            TableRef::resolve(&fk.referenced_table, owner),
            fk,
        )
    });
    let incoming = properties
        .references
        .iter()
        .flatten()
        .map(|fk| (TableRef::resolve(&fk.table, owner), owner.clone(), fk));
    outgoing.chain(incoming)
}

/// Sorts by (child, parent, columns, name) and drops later duplicates of the
/// same tables + columns, so output never depends on driver ordering.
fn sort_dedup(specs: &mut Vec<RelationSpec>) {
    specs.sort();
    specs.dedup_by(|b, a| {
        (&a.child, &a.parent, &a.child_columns, &a.parent_columns)
            == (&b.child, &b.parent, &b.child_columns, &b.parent_columns)
    });
}

/// Every FK touching the focus, deduplicated and sorted.
fn relation_specs(focus: &TableRef, properties: &TableProperties) -> Vec<RelationSpec> {
    let mut specs: Vec<RelationSpec> = fk_edges(focus, properties)
        .map(|(child, parent, fk)| RelationSpec::new(child, parent, fk))
        .collect();
    sort_dedup(&mut specs);
    specs
}

/// Outgoing FK columns of `table` known from the relations, first-seen order.
fn known_fk_columns<'a>(specs: &'a [RelationSpec], table: &TableRef) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for name in specs
        .iter()
        .filter(|s| s.child == *table)
        .flat_map(|s| s.child_columns.iter())
    {
        if !names.contains(&name.as_str()) {
            names.push(name);
        }
    }
    names
}

/// Resolves specs to relations with column indices into `entities`.
fn link_relations(
    specs: &[RelationSpec],
    entities: &[DiagramEntity],
    id_of: impl Fn(&TableRef) -> EntityId,
) -> Vec<DiagramRelation> {
    let indices = |entity: EntityId, names: &[String]| -> Vec<usize> {
        names
            .iter()
            .filter_map(|n| entities[entity.0].columns.iter().position(|c| c.name == *n))
            .collect()
    };
    specs
        .iter()
        .map(|s| {
            let (child, parent) = (id_of(&s.child), id_of(&s.parent));
            let child_columns = indices(child, &s.child_columns);
            let child_entity = &entities[child.0];
            let optional = child_columns.is_empty()
                || child_columns.iter().any(|&c| child_entity.columns[c].nullable);
            let unique = !child_columns.is_empty()
                && child_entity
                    .unique_keys
                    .iter()
                    .any(|key| !key.is_empty() && key.iter().all(|c| child_columns.contains(c)));
            DiagramRelation {
                name: s.name.clone(),
                child,
                child_columns,
                parent,
                parent_columns: indices(parent, &s.parent_columns),
                optional,
                unique,
                origin: RelationOrigin::Declared,
            }
        })
        .collect()
}

/// Related tables (not the focus) in `TableRef` order.
fn related_set(focus: &TableRef, specs: &[RelationSpec]) -> BTreeSet<TableRef> {
    specs
        .iter()
        .flat_map(|s| [&s.child, &s.parent])
        .filter(|t| *t != focus)
        .cloned()
        .collect()
}

impl DiagramEntity {
    /// Columns from loaded properties: primary key first (constraint order,
    /// then any flagged leftovers), rest in table order. `fk_columns` are
    /// extra names known to be outgoing FK columns.
    pub(in crate::app) fn from_properties(
        table: TableRef,
        label: String,
        properties: &TableProperties,
        fk_columns: &[&str],
        focus: bool,
    ) -> Self {
        let pk_names: Vec<&str> = properties
            .keys
            .iter()
            .flatten()
            .find(|k| k.kind == CatalogConstraintKind::PrimaryKey)
            .into_iter()
            .flat_map(|k| k.columns.iter().map(String::as_str))
            .collect();
        let is_fk = |name: &str| {
            fk_columns.contains(&name)
                || properties
                    .foreign_keys
                    .iter()
                    .flatten()
                    .any(|fk| fk.columns.iter().any(|c| c == name))
        };
        let make = |c: &crate::catalog::CatalogColumn, primary_key: bool| DiagramColumn {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            glyph: TypeGlyph::for_type(&c.data_type),
            primary_key,
            foreign_key: is_fk(&c.name),
            nullable: c.nullable,
            default: c.default_expression.clone(),
            comment: c.comment.clone(),
            auto_increment: c.auto_increment == Some(true),
            generated: c.is_generated == Some(true),
            attributes: c.attributes.as_ref().map(|a| crate::sql_ddl::ColumnAttributes {
                character_set: a.character_set.clone(),
                collation: a.collation.clone(),
                extra: a.extra.clone(),
                generation_expression: a.generation_expression.clone(),
            }),
            pending: ColumnState::Existing,
        };
        let in_key = |c: &crate::catalog::CatalogColumn| {
            c.primary_key || pk_names.contains(&c.name.as_str())
        };
        let keyed = pk_names
            .iter()
            .filter_map(|n| properties.columns.iter().find(|c| c.name == *n));
        let flagged_extra = properties
            .columns
            .iter()
            .filter(|c| c.primary_key && !pk_names.contains(&c.name.as_str()));
        let mut columns: Vec<DiagramColumn> =
            keyed.chain(flagged_extra).map(|c| make(c, true)).collect();
        columns.extend(
            properties
                .columns
                .iter()
                .filter(|c| !in_key(c))
                .map(|c| make(c, false)),
        );
        let unique_keys = unique_key_columns(properties, &columns);
        Self {
            table,
            label,
            columns,
            state: EntityState::Ready,
            focus,
            unique_keys,
            primary_key_name: properties
                .keys
                .iter()
                .flatten()
                .find(|k| k.kind == CatalogConstraintKind::PrimaryKey)
                .map(|k| k.name.clone()),
            indexes: properties
                .indexes
                .iter()
                .flatten()
                .map(|i| DiagramIndex {
                    name: i.name.clone(),
                    columns: i.columns.clone(),
                    unique: i.unique,
                    primary: i.primary,
                    method: i.method.clone(),
                })
                .collect(),
        }
    }

    /// Entity for a related table in whatever state its fetch is in.
    fn related(
        table: TableRef,
        label: String,
        properties: RelatedProperties,
        fk_columns: &[&str],
    ) -> Self {
        match properties {
            RelatedProperties::Ready(props) => {
                Self::from_properties(table, label, &props, fk_columns, false)
            }
            RelatedProperties::Loading => {
                Self::placeholder(table, label, fk_columns, EntityState::Loading)
            }
            RelatedProperties::Failed(msg) => {
                Self::placeholder(table, label, fk_columns, EntityState::Failed(msg))
            }
        }
    }

    /// Entity whose properties are unavailable: only the FK columns known
    /// from the focus' `references` entries.
    fn placeholder(
        table: TableRef,
        label: String,
        fk_columns: &[&str],
        state: EntityState,
    ) -> Self {
        let columns = fk_columns
            .iter()
            .map(|name| DiagramColumn {
                name: (*name).to_owned(),
                data_type: String::new(),
                glyph: TypeGlyph::Other,
                primary_key: false,
                foreign_key: true,
                nullable: true,
                default: None,
                comment: None,
                auto_increment: false,
                generated: false,
                attributes: None,
                pending: ColumnState::Existing,
            })
            .collect();
        Self {
            table,
            label,
            columns,
            state,
            focus: false,
            unique_keys: Vec::new(),
            indexes: Vec::new(),
            primary_key_name: None,
        }
    }
}

/// Primary-key, unique-constraint and unique-index column sets of
/// `properties`, as indices into `columns` (sets with an unknown column are
/// dropped).
fn unique_key_columns(properties: &TableProperties, columns: &[DiagramColumn]) -> Vec<Vec<usize>> {
    let constraints = properties
        .keys
        .iter()
        .flatten()
        .filter(|k| matches!(k.kind, CatalogConstraintKind::PrimaryKey | CatalogConstraintKind::Unique))
        .map(|k| &k.columns);
    let indexes = properties
        .indexes
        .iter()
        .flatten()
        .filter(|i| i.unique || i.primary)
        .map(|i| &i.columns);
    let mut keys: Vec<Vec<usize>> = constraints
        .chain(indexes)
        .filter_map(|names| {
            names
                .iter()
                .map(|n| columns.iter().position(|c| c.name == *n))
                .collect::<Option<Vec<usize>>>()
        })
        .filter(|key| !key.is_empty())
        .collect();
    // Primary-key flags of drivers that report no key constraint.
    keys.push(
        columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.primary_key)
            .map(|(i, _)| i)
            .collect(),
    );
    keys.retain(|key| !key.is_empty());
    keys.sort();
    keys.dedup();
    keys
}

/// Whether `needle` (already lower-case) occurs in `haystack`, ignoring case.
/// Allocation-free for ASCII text.
pub(in crate::app) fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if haystack.is_ascii() && needle.is_ascii() {
        return haystack
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()));
    }
    haystack.to_lowercase().contains(needle)
}

impl DiagramEntity {
    /// Search hit on the table name or label.
    pub(in crate::app) fn name_matches(&self, needle: &str) -> bool {
        contains_ci(&self.table.table, needle) || contains_ci(&self.label, needle)
    }
}

impl DiagramGraph {
    /// Entities whose table name or any column name contains `query` (case
    /// insensitive; blank matches nothing), in entity order.
    pub(in crate::app) fn search(&self, query: &str) -> Vec<EntityId> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.entities
            .iter()
            .enumerate()
            .filter(|(_, e)| e.name_matches(&needle) || e.columns.iter().any(|c| contains_ci(&c.name, &needle)))
            .map(|(i, _)| EntityId(i))
            .collect()
    }
}

impl DiagramGraph {
    /// Builds the depth-1 graph: the focus entity (index 0), every table its
    /// foreign keys reference, and every table whose foreign keys reference
    /// it (`focus.references`). Deterministic order (by `TableRef`) after the
    /// focus; duplicate edges collapse; self-references are kept.
    /// `related` supplies each related table's properties (or its state).
    pub(in crate::app) fn build(
        focus: &TableRef,
        focus_properties: &TableProperties,
        related: &dyn Fn(&TableRef) -> RelatedProperties,
    ) -> Self {
        let specs = relation_specs(focus, focus_properties);
        let others: Vec<TableRef> = related_set(focus, &specs).into_iter().collect();
        let id_of = |t: &TableRef| {
            if t == focus {
                EntityId(0)
            } else {
                // `others` holds every non-focus table in `specs`.
                EntityId(1 + others.binary_search(t).unwrap_or_default())
            }
        };
        let label_of = |t: &TableRef| match &t.schema {
            Some(schema) if t.schema != focus.schema => format!("{schema}.{}", t.table),
            _ => t.table.clone(),
        };

        let mut entities = Vec::with_capacity(1 + others.len());
        entities.push(DiagramEntity::from_properties(
            focus.clone(),
            focus.table.clone(),
            focus_properties,
            &known_fk_columns(&specs, focus),
            true,
        ));
        entities.extend(others.iter().map(|table| {
            let label = label_of(table);
            let fk_columns = known_fk_columns(&specs, table);
            DiagramEntity::related(table.clone(), label, related(table), &fk_columns)
        }));
        let relations = link_relations(&specs, &entities, id_of);
        Self {
            entities,
            relations,
            external_relations: 0,
        }
    }

    /// Whole-schema graph: every table is an entity (none is the focus) and
    /// every foreign key with both ends in the set is a relation. Foreign keys
    /// are read from each table's own list and from the `references` of the
    /// table they point at, so a still-loading child is linked by its parent.
    /// Deterministic order by `TableRef`; duplicates collapse (the first of
    /// equal refs wins); self-references are kept; foreign keys leaving the
    /// set are only counted in `external_relations`.
    pub(in crate::app) fn build_schema(tables: &[(TableRef, RelatedProperties)]) -> Self {
        let mut order: Vec<usize> = (0..tables.len()).collect();
        order.sort_by(|&a, &b| tables[a].0.cmp(&tables[b].0));
        order.dedup_by(|b, a| tables[*a].0 == tables[*b].0);
        let refs: Vec<&TableRef> = order.iter().map(|&i| &tables[i].0).collect();
        let position = |t: &TableRef| refs.binary_search(&t).ok();

        let mut specs = Vec::new();
        let mut external = Vec::new();
        for &i in &order {
            let (owner, RelatedProperties::Ready(props)) = &tables[i] else {
                continue;
            };
            for (child, parent, fk) in fk_edges(owner, props) {
                let inside = position(&child).is_some() && position(&parent).is_some();
                if inside {
                    specs.push(RelationSpec::new(child, parent, fk));
                } else if child == *owner {
                    external.push(RelationSpec::new(child, parent, fk));
                }
            }
        }
        sort_dedup(&mut specs);
        sort_dedup(&mut external);

        let mixed_schemas = refs.iter().any(|t| t.schema != refs[0].schema);
        let entities: Vec<DiagramEntity> = order
            .iter()
            .map(|&i| {
                let (table, props) = &tables[i];
                let label = match &table.schema {
                    Some(schema) if mixed_schemas => format!("{schema}.{}", table.table),
                    _ => table.table.clone(),
                };
                DiagramEntity::related(
                    table.clone(),
                    label,
                    props.clone(),
                    &known_fk_columns(&specs, table),
                )
            })
            .collect();
        let relations = link_relations(&specs, &entities, |t| {
            EntityId(position(t).unwrap_or_default())
        });
        Self {
            entities,
            relations,
            external_relations: external.len(),
        }
    }

    /// The related tables (excluding the focus) the graph needs properties
    /// for, capped at `limit`, in deterministic order. Used by `data`.
    pub(in crate::app) fn related_tables(
        focus: &TableRef,
        focus_properties: &TableProperties,
        limit: usize,
    ) -> Vec<TableRef> {
        related_set(focus, &relation_specs(focus, focus_properties))
            .into_iter()
            .take(limit)
            .collect()
    }
}

impl TypeGlyph {
    /// Maps an engine type name (any dialect) to its glyph. Allocation-free.
    pub(in crate::app) fn for_type(data_type: &str) -> Self {
        let mut t = data_type.trim();
        // ClickHouse wrappers: Nullable(T), LowCardinality(T), nested.
        while let Some(inner) = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|w| strip_prefix_ci(t, w))
        {
            t = inner.trim().strip_suffix(')').unwrap_or(inner).trim();
        }
        if t.ends_with("[]") {
            return Self::Other;
        }
        let end = t
            .find(|c: char| c == '(' || c == '[' || c.is_whitespace())
            .unwrap_or(t.len());
        let (base, rest) = t.split_at(end);
        let is = |names: &[&str]| names.iter().any(|n| base.eq_ignore_ascii_case(n));
        if is(&["bool", "boolean"])
            || (base.eq_ignore_ascii_case("tinyint") && rest.starts_with("(1)"))
        {
            Self::Boolean
        } else if is(&[
            "int",
            "integer",
            "tinyint",
            "smallint",
            "mediumint",
            "bigint",
            "int2",
            "int4",
            "int8",
            "int16",
            "int32",
            "int64",
            "int128",
            "int256",
            "uint8",
            "uint16",
            "uint32",
            "uint64",
            "uint128",
            "uint256",
            "serial",
            "smallserial",
            "bigserial",
            "serial2",
            "serial4",
            "serial8",
            "numeric",
            "decimal",
            "decimal32",
            "decimal64",
            "decimal128",
            "decimal256",
            "number",
            "dec",
            "fixed",
            "float",
            "float4",
            "float8",
            "float32",
            "float64",
            "double",
            "real",
            "money",
        ]) {
            Self::Numeric
        } else if is(&[
            "char",
            "varchar",
            "character",
            "nchar",
            "nvarchar",
            "national",
            "text",
            "tinytext",
            "mediumtext",
            "longtext",
            "ntext",
            "clob",
            "string",
            "fixedstring",
            "uuid",
            "enum",
            "enum8",
            "enum16",
            "citext",
            "bpchar",
            "name",
            "set",
        ]) {
            Self::Text
        } else if is(&[
            "date",
            "date32",
            "time",
            "timetz",
            "timestamp",
            "timestamptz",
            "datetime",
            "datetime2",
            "datetime64",
            "smalldatetime",
            "interval",
            "year",
        ]) {
            Self::Temporal
        } else if is(&[
            "bytea",
            "blob",
            "tinyblob",
            "mediumblob",
            "longblob",
            "binary",
            "varbinary",
        ]) {
            Self::Binary
        } else if is(&["json", "jsonb"]) {
            Self::Json
        } else {
            Self::Other
        }
    }
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}
