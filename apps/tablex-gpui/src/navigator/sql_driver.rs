//! A SQL driver's navigator (`engine_driver_api::sql`) mapped onto the typed
//! SQL tree (`Database -> Schema -> Tables/Views -> Object -> Column`).
//!
//! A driver without a schema level (`SqlDialectInfo::supports_schemas ==
//! false`: SQLite, MySQL, MariaDB) gets one schema named like its database.
//! The driver is never asked for it.

use super::{
    column_child, database_child, object_child, schema_child, schema_object_child, KindOfObject,
    LoadRequest, LoadedChild, ObjectGroup, SchemaObjectRef,
};
use crate::connections::ConnectionKind;
use engine_driver_api::sql::TableColumn;
use engine_driver_api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// The most a driver returns per navigator call (`limit` is capped there).
pub const PAGE_LIMIT: u64 = 5_000;
/// Nodes one catalog load collects across pages before it gives up, the same
/// bound the database enumeration uses.
pub const MAX_NODES: usize = 100_000;

/// Kinds whose catalog is the typed SQL tree.
pub fn is_sql_kind(kind: ConnectionKind) -> bool {
    matches!(
        kind,
        ConnectionKind::Postgres
            | ConnectionKind::Sqlite
            | ConnectionKind::Mysql
            | ConnectionKind::Mariadb
            | ConnectionKind::Clickhouse
    )
}

/// Why a catalog load did not produce its nodes.
#[derive(Debug)]
pub enum PageError<E> {
    Fetch(E),
    TooManyNodes,
    RepeatedCursor,
}

impl<E: std::fmt::Display> std::fmt::Display for PageError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fetch(error) => error.fmt(f),
            Self::TooManyNodes => write!(f, "Catalog exceeds {MAX_NODES} entries"),
            Self::RepeatedCursor => f.write_str("Catalog repeated a page cursor"),
        }
    }
}

/// The driver call that answers `load`, or `None` for a level the driver is
/// not asked about (the schema of a driver without schemas, provider loads).
/// `filter` is the remote-search name filter of an object group.
pub fn catalog_request(
    load: &LoadRequest,
    supports_schemas: bool,
    filter: Option<&str>,
) -> Option<NavigatorRequest> {
    let (parent, database, payload) = match load {
        LoadRequest::Databases { .. } => (None, None, None),
        LoadRequest::Schemas { database, .. } if supports_schemas => (
            Some(format!("database:{database}")),
            Some(database.clone()),
            Some(json!({ "database": database })),
        ),
        LoadRequest::Objects {
            database,
            schema,
            group,
            ..
        } => {
            let mut payload = json!({ "database": database });
            let parent = if supports_schemas {
                payload["schema"] = json!(schema);
                format!("schema:{database}:{schema}")
            } else {
                format!("database:{database}")
            };
            if let Some(filter) = filter.filter(|filter| !filter.is_empty()) {
                payload["filter"] = json!(filter);
            }
            // Tables and views come from the plain listing; every other group asks
            // for its own kind of object.
            if let Some(kind) = group.schema_object_kind() {
                payload["kind"] = json!(kind.as_str());
            }
            (Some(parent), Some(database.clone()), Some(payload))
        }
        LoadRequest::Columns {
            database,
            schema,
            object,
            ..
        } => {
            let mut payload = json!({ "database": database, "object": object });
            if supports_schemas {
                payload["schema"] = json!(schema);
            }
            (None, Some(database.clone()), Some(payload))
        }
        LoadRequest::Schemas { .. } | LoadRequest::Provider { .. } => return None,
    };
    Some(NavigatorRequest {
        parent,
        database,
        payload,
        cursor: None,
        limit: match filter {
            Some(_) => super::REMOTE_FILTER_LIMIT as u64,
            None => PAGE_LIMIT,
        },
    })
}

/// Every node `fetch` returns for `request`, following the page cursors. A
/// remote-search load (`single_page`) is one call: its cap is the truncation
/// the sidebar already reports.
pub fn collect_pages<E>(
    mut request: NavigatorRequest,
    single_page: bool,
    mut fetch: impl FnMut(&NavigatorRequest) -> Result<NavigatorPage, E>,
) -> Result<Vec<NavigatorNode>, PageError<E>> {
    let mut nodes = Vec::new();
    let mut cursors = HashSet::new();
    loop {
        let page = fetch(&request).map_err(PageError::Fetch)?;
        nodes.extend(page.nodes);
        if nodes.len() > MAX_NODES {
            return Err(PageError::TooManyNodes);
        }
        match page.next_cursor {
            Some(next) if !single_page => {
                if !cursors.insert(next.clone()) {
                    return Err(PageError::RepeatedCursor);
                }
                request.cursor = Some(next);
            }
            _ => return Ok(nodes),
        }
    }
}

/// The tree children `load` expands to. The id of a node comes from its
/// payload (the contract's `SqlObjectRef` keys), the label when it has none.
pub fn catalog_children(load: &LoadRequest, nodes: &[NavigatorNode]) -> Vec<LoadedChild> {
    let connection = load.connection_id();
    nodes
        .iter()
        .filter_map(|node| match load {
            LoadRequest::Databases { .. } => (node.kind == "database")
                .then(|| database_child(&connection, name_of(node, "database"))),
            LoadRequest::Schemas { database, .. } => (node.kind == "schema")
                .then(|| schema_child(&connection, database, name_of(node, "schema"))),
            LoadRequest::Objects {
                database,
                schema,
                group,
                ..
            } => {
                let table_like = match (group, node.kind.as_str()) {
                    (ObjectGroup::Tables, "table") => Some(KindOfObject::Table),
                    (ObjectGroup::Views, "view") => Some(KindOfObject::View),
                    (ObjectGroup::MaterializedViews, "materialized_view") => {
                        Some(KindOfObject::MaterializedView)
                    }
                    _ => None,
                };
                // A MariaDB sequence is table-like: its node carries a table reference.
                let table_like = table_like.or_else(|| {
                    (matches!(group, ObjectGroup::Sequences)
                        && node.kind == "sequence"
                        && SchemaObjectRef::deserialize(&node.payload).is_err())
                    .then_some(KindOfObject::Sequence)
                });
                if let Some(kind) = table_like {
                    return Some(object_child(
                        &connection,
                        database,
                        schema,
                        name_of(node, "object"),
                        kind,
                    ));
                }
                // A driver that does not know the group's kind answers tables and
                // views: nothing of them belongs here.
                let wanted = group.schema_object_kind().filter(|_| !group.is_table_like())?;
                (node.kind == wanted.as_str()).then(|| {
                    let reference = SchemaObjectRef::deserialize(&node.payload).unwrap_or_else(|_| {
                        SchemaObjectRef {
                            database: Some(database.clone()),
                            schema: Some(schema.clone()),
                            kind: wanted,
                            name: node.label.clone(),
                            arguments: None,
                            table: None,
                            identity: None,
                        }
                    });
                    schema_object_child(&connection, database, schema, *group, &node.id, &node.label, reference)
                })
            }
            LoadRequest::Columns {
                database,
                schema,
                object,
                ..
            } => (node.kind == "column").then(|| {
                column_child(&connection, database, schema, object, name_of(node, "column"))
            }),
            LoadRequest::Provider { .. } => None,
        })
        .collect()
}

/// The catalog columns (types, keys, defaults) of a `Columns` page; the
/// payload of each node is a `SqlColumnNode`.
pub fn catalog_columns(nodes: &[NavigatorNode]) -> Vec<crate::catalog::CatalogColumn> {
    nodes
        .iter()
        .filter(|node| node.kind == "column")
        .map(|node| {
            let details = TableColumn::deserialize(&node.payload).unwrap_or_else(|_| TableColumn {
                name: node.label.clone(),
                data_type: node.subtitle.clone().unwrap_or_default(),
                nullable: true,
                ..Default::default()
            });
            crate::catalog::CatalogColumn {
                name: details.name,
                data_type: details.data_type,
                nullable: details.nullable,
                primary_key: details.primary_key,
                is_generated: details.is_generated,
                default_expression: details.default_expression,
                auto_increment: details.auto_increment,
                comment: details.comment,
                attributes: details.attributes.map(|attributes| crate::catalog::CatalogColumnAttributes {
                    character_set: attributes.character_set,
                    collation: attributes.collation,
                    extra: attributes.extra,
                    generation_expression: attributes.generation_expression,
                }),
            }
        })
        .collect()
}

fn name_of<'a>(node: &'a NavigatorNode, key: &str) -> &'a str {
    node.payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(&node.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: &str, label: &str, payload: Value) -> NavigatorNode {
        NavigatorNode {
            id: format!("{kind}:{label}"),
            kind: kind.into(),
            label: label.into(),
            subtitle: None,
            icon: None,
            has_children: kind != "column",
            payload,
            actions: Vec::new(),
        }
    }

    fn objects(group: ObjectGroup) -> LoadRequest {
        LoadRequest::Objects {
            connection: "c".into(),
            database: "app".into(),
            schema: "public".into(),
            group,
        }
    }

    #[test]
    fn requests_follow_the_dialect_and_carry_the_filter() {
        let with_schemas = catalog_request(&objects(ObjectGroup::Tables), true, Some("ord")).unwrap();
        assert_eq!(with_schemas.parent.as_deref(), Some("schema:app:public"));
        assert_eq!(with_schemas.database.as_deref(), Some("app"));
        assert_eq!(
            with_schemas.payload,
            Some(json!({"database": "app", "schema": "public", "filter": "ord"}))
        );
        assert_eq!(with_schemas.limit, super::super::REMOTE_FILTER_LIMIT as u64);

        let flat = catalog_request(&objects(ObjectGroup::Tables), false, None).unwrap();
        assert_eq!(flat.parent.as_deref(), Some("database:app"));
        assert_eq!(flat.payload, Some(json!({"database": "app"})));
        assert_eq!(flat.limit, PAGE_LIMIT);

        let columns = LoadRequest::Columns {
            connection: "c".into(),
            database: "app".into(),
            schema: "public".into(),
            object: "orders".into(),
        };
        assert_eq!(
            catalog_request(&columns, true, None).unwrap().payload,
            Some(json!({"database": "app", "schema": "public", "object": "orders"}))
        );
        assert_eq!(
            catalog_request(&columns, false, None).unwrap().payload,
            Some(json!({"database": "app", "object": "orders"}))
        );

        let roots = catalog_request(&LoadRequest::Databases { connection: "c".into() }, false, None)
            .unwrap();
        assert!(roots.parent.is_none() && roots.database.is_none() && roots.payload.is_none());
    }

    #[test]
    fn the_schema_level_of_a_schemaless_driver_is_not_a_driver_call() {
        let schemas = LoadRequest::Schemas {
            connection: "c".into(),
            database: "main".into(),
        };
        assert!(catalog_request(&schemas, false, None).is_none());
        assert_eq!(
            catalog_request(&schemas, true, None).unwrap().parent.as_deref(),
            Some("database:main")
        );
    }

    #[test]
    fn object_groups_split_tables_from_views() {
        let nodes = [
            node("table", "orders", json!({"object": "orders"})),
            node("view", "daily", json!({"object": "daily"})),
            node("materialized_view", "rollup", json!({"object": "rollup"})),
            node("table", "labelled", Value::Null),
            node("function", "ignored", Value::Null),
        ];
        let labels = |group| {
            catalog_children(&objects(group), &nodes)
                .into_iter()
                .map(|child| (child.label, child.object_kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(ObjectGroup::Tables),
            [
                ("orders".to_string(), Some(KindOfObject::Table)),
                ("labelled".to_string(), Some(KindOfObject::Table)),
            ]
        );
        assert_eq!(labels(ObjectGroup::Views), [("daily".to_string(), Some(KindOfObject::View))]);
        // A materialized view has its own group.
        assert_eq!(
            labels(ObjectGroup::MaterializedViews),
            [("rollup".to_string(), Some(KindOfObject::MaterializedView))]
        );
    }

    #[test]
    fn a_sequence_with_a_table_reference_is_a_table_like_row_and_one_with_a_source_reference_is_not() {
        let group = LoadRequest::Objects {
            connection: "c".into(),
            database: "app".into(),
            schema: "app".into(),
            group: ObjectGroup::Sequences,
        };
        // MariaDB: the node carries a `SqlObjectRef`, so it opens as data, like a table.
        let mariadb = catalog_children(&group, &[node("sequence", "ids", json!({"database": "app", "object": "ids"}))]);
        assert_eq!(
            (mariadb[0].object_kind, mariadb[0].schema_object.is_none()),
            (Some(KindOfObject::Sequence), true)
        );
        assert_eq!(mariadb[0].id, crate::navigator::object_child("c", "app", "app", "ids", KindOfObject::Sequence).id);
        // PostgreSQL: a `SchemaObjectRef`, so it has a source and nothing else.
        let postgres = catalog_children(
            &group,
            &[node("sequence", "ids", json!({"database": "app", "schema": "public", "kind": "sequence", "name": "ids"}))],
        );
        assert_eq!((postgres[0].object_kind, postgres[0].schema_object.is_some()), (None, true));
        // The row is keyed by the driver's node id, not by what it says.
        assert_eq!(
            postgres[0].id,
            crate::navigator::schema_object_id("c", "app", "app", ObjectGroup::Sequences, "sequence:ids")
        );
    }

    #[test]
    fn schema_object_groups_ask_for_their_kind_and_keep_only_that_kind() {
        let schema_objects = |group| LoadRequest::Objects {
            connection: "c".into(),
            database: "app".into(),
            schema: "public".into(),
            group,
        };
        let request = catalog_request(&schema_objects(ObjectGroup::Functions), true, Some("ad")).unwrap();
        assert_eq!(
            request.payload,
            Some(json!({"database": "app", "schema": "public", "filter": "ad", "kind": "function"}))
        );
        // MySQL has no schema level: the database is the parent.
        let flat = catalog_request(&schema_objects(ObjectGroup::Triggers), false, None).unwrap();
        assert_eq!(flat.payload, Some(json!({"database": "app", "kind": "trigger"})));
        let matviews = catalog_request(&schema_objects(ObjectGroup::MaterializedViews), true, None).unwrap();
        assert_eq!(matviews.payload.unwrap()["kind"], "materialized_view");
        // Tables and views are still asked for with the plain listing.
        let tables = catalog_request(&schema_objects(ObjectGroup::Tables), true, None).unwrap();
        assert!(tables.payload.unwrap().get("kind").is_none());

        let reference = json!({"database": "app", "schema": "public", "kind": "function",
                               "name": "add", "arguments": "integer, integer"});
        let nodes = [
            node("function", "add(integer, integer)", reference.clone()),
            node("trigger", "audit on orders", json!({"kind": "trigger", "name": "audit", "table": "orders"})),
            // A driver that does not know the kind answers tables: none of them is a function.
            node("table", "orders", json!({"object": "orders"})),
        ];
        let functions = catalog_children(&schema_objects(ObjectGroup::Functions), &nodes);
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].label, "add(integer, integer)");
        assert_eq!(
            functions[0].schema_object,
            Some(serde_json::from_value::<SchemaObjectRef>(reference).unwrap())
        );
        assert_eq!(
            functions[0].id,
            crate::navigator::schema_object_id("c", "app", "public", ObjectGroup::Functions, "function:add(integer, integer)")
        );
        let triggers = catalog_children(&schema_objects(ObjectGroup::Triggers), &nodes);
        assert_eq!(triggers.iter().map(|child| child.label.as_str()).collect::<Vec<_>>(), ["audit on orders"]);
        assert!(catalog_children(&schema_objects(ObjectGroup::Sequences), &nodes).is_empty());
        // A payload that is not a reference still yields the row, with one built from the label.
        let bare = catalog_children(&schema_objects(ObjectGroup::Functions), &[node("function", "f()", Value::Null)]);
        assert_eq!(bare[0].schema_object.as_ref().unwrap().name, "f()");
    }

    #[test]
    fn databases_schemas_and_columns_map_to_the_native_ids() {
        let databases = catalog_children(
            &LoadRequest::Databases { connection: "c".into() },
            &[node("database", "app", json!({"database": "app"}))],
        );
        assert_eq!(databases, [crate::navigator::database_child("c", "app")]);
        let schemas = catalog_children(
            &LoadRequest::Schemas { connection: "c".into(), database: "app".into() },
            &[node("schema", "public", json!({"database": "app", "schema": "public"}))],
        );
        assert_eq!(schemas, [crate::navigator::schema_child("c", "app", "public")]);
        let columns = catalog_children(
            &LoadRequest::Columns {
                connection: "c".into(),
                database: "app".into(),
                schema: "public".into(),
                object: "orders".into(),
            },
            &[node("column", "id", json!({"column": "id", "name": "id"}))],
        );
        assert_eq!(
            columns,
            [crate::navigator::column_child("c", "app", "public", "orders", "id")]
        );
    }

    #[test]
    fn column_payloads_become_catalog_columns() {
        let columns = catalog_columns(&[node(
            "column",
            "id",
            json!({"object": "orders", "column": "id", "name": "id", "data_type": "integer",
                   "nullable": false, "primary_key": true, "auto_increment": true}),
        )]);
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "id");
        assert_eq!(columns[0].data_type, "integer");
        assert!(columns[0].primary_key && !columns[0].nullable);
        assert_eq!(columns[0].auto_increment, Some(true));
        // A payload without details still yields the column.
        let bare = catalog_columns(&[node("column", "x", Value::Null)]);
        assert_eq!(bare[0].name, "x");
    }

    #[test]
    fn pages_are_followed_until_the_last_one() {
        let pages = std::cell::RefCell::new(vec![
            NavigatorPage { nodes: vec![node("table", "a", Value::Null)], next_cursor: Some("1".into()) },
            NavigatorPage { nodes: vec![node("table", "b", Value::Null)], next_cursor: None },
        ]);
        let mut cursors = Vec::new();
        let request = NavigatorRequest { parent: None, database: None, payload: None, cursor: None, limit: 0 };
        let nodes = collect_pages::<String>(request.clone(), false, |request| {
            cursors.push(request.cursor.clone());
            Ok(pages.borrow_mut().remove(0))
        })
        .unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(cursors, [None, Some("1".into())]);

        // A filtered (remote search) load is one call, whatever the cursor says.
        let mut calls = 0;
        let nodes = collect_pages::<String>(request.clone(), true, |_| {
            calls += 1;
            Ok(NavigatorPage { nodes: vec![node("table", "a", Value::Null)], next_cursor: Some("1".into()) })
        })
        .unwrap();
        assert_eq!((nodes.len(), calls), (1, 1));

        // A driver that keeps repeating a cursor cannot loop the load forever.
        let looped = collect_pages::<String>(request.clone(), false, |_| {
            Ok(NavigatorPage { nodes: Vec::new(), next_cursor: Some("same".into()) })
        });
        assert!(matches!(looped, Err(PageError::RepeatedCursor)));
        // A catalog past the bound fails instead of filling memory.
        let mut page = 0;
        let huge = collect_pages::<String>(request.clone(), false, |_| {
            page += 1;
            Ok(NavigatorPage {
                nodes: (0..PAGE_LIMIT).map(|_| node("table", "t", Value::Null)).collect(),
                next_cursor: Some(page.to_string()),
            })
        });
        assert!(matches!(huge, Err(PageError::TooManyNodes)));
        assert_eq!(page as usize, MAX_NODES / PAGE_LIMIT as usize + 1);
        let failed = collect_pages(request, false, |_| Err::<NavigatorPage, _>("boom".to_string()));
        assert!(matches!(failed, Err(PageError::Fetch(error)) if error == "boom"));
    }
}
