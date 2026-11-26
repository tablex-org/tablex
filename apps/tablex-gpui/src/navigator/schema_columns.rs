//! The columns of every table and view of one schema, from the driver's one schema-wide catalog
//! request (`metadata.schema_columns`, see `engine_driver_api::sql`), kept compactly so the
//! navigator tree, SQL completion and whatever else needs a table's columns read them without a
//! round trip per table.
//!
//! Memory: a column is two shared strings (names and types repeat across tables, so each distinct
//! text is stored once) and nothing else; a table is one boxed slice. The store is capped at
//! [`MAX_COLUMNS`]; a schema beyond it is not stored at all and its tables are asked for one by
//! one, as before.

use engine_driver_api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Most columns one schema keeps. At the cap a schema is too large to be worth reading whole
/// (10 000 tables of 50 columns): the per-table requests serve it instead.
pub const MAX_COLUMNS: usize = 100_000;

/// One column: what the tree and completion show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredColumn {
    pub name: Arc<str>,
    pub data_type: Arc<str>,
}

/// The columns of the tables of one schema, each in the driver's order.
#[derive(Debug, Default)]
pub struct SchemaColumns {
    tables: HashMap<Box<str>, Box<[StoredColumn]>>,
    columns: usize,
}

impl SchemaColumns {
    /// The columns of `table` (its exact name); `None` when the schema listing did not have it.
    pub fn table(&self, table: &str) -> Option<&[StoredColumn]> {
        self.tables.get(table).map(|columns| &columns[..])
    }

    /// Forgets `table`: it is asked for on its own from now on.
    pub fn remove_table(&mut self, table: &str) {
        if let Some(columns) = self.tables.remove(table) {
            self.columns -= columns.len();
        }
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    pub fn column_count(&self) -> usize {
        self.columns
    }
}

/// Why a schema listing was not stored.
#[derive(Debug)]
pub enum SchemaColumnsError<E> {
    Fetch(E),
    /// More than [`MAX_COLUMNS`] columns.
    TooLarge,
    RepeatedCursor,
}

/// Builds a [`SchemaColumns`] from the `column` nodes of the driver's pages, one page at a time:
/// a node is read in place (nothing of it is kept but the two shared strings), so the pages never
/// pile up.
#[derive(Default)]
struct Builder {
    done: SchemaColumns,
    interned: HashSet<Arc<str>>,
    table: String,
    columns: Vec<StoredColumn>,
}

impl Builder {
    fn intern(&mut self, text: &str) -> Arc<str> {
        if let Some(known) = self.interned.get(text) {
            return Arc::clone(known);
        }
        let new: Arc<str> = Arc::from(text);
        self.interned.insert(Arc::clone(&new));
        new
    }

    fn flush(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let columns = std::mem::take(&mut self.columns);
        let table = std::mem::take(&mut self.table);
        // A table whose columns come in two runs (a driver that does not keep them together) keeps both.
        match self.done.tables.remove(table.as_str()) {
            Some(before) => {
                let mut all = before.into_vec();
                all.extend(columns);
                self.done.tables.insert(table.into(), all.into());
            }
            None => {
                self.done.tables.insert(table.into(), columns.into());
            }
        }
    }

    /// Adds one page. `false` once the cap is passed.
    fn page(&mut self, nodes: &[NavigatorNode]) -> bool {
        for node in nodes.iter().filter(|node| node.kind == "column") {
            let text = |key: &str| node.payload.get(key).and_then(Value::as_str);
            let Some(table) = text("object").filter(|table| !table.is_empty()) else { continue };
            let name = text("column").unwrap_or(&node.label);
            let data_type = text("data_type").or(node.subtitle.as_deref()).unwrap_or_default();
            if table != self.table {
                self.flush();
                self.table.clear();
                self.table.push_str(table);
            }
            let column = StoredColumn { name: self.intern(name), data_type: self.intern(data_type) };
            self.columns.push(column);
            self.done.columns += 1;
            if self.done.columns > MAX_COLUMNS {
                return false;
            }
        }
        true
    }

    fn finish(mut self) -> SchemaColumns {
        self.flush();
        self.done.tables.shrink_to_fit();
        self.done
    }
}

/// The request for the schema-wide column listing: the schema's own node as parent (the database's
/// for a driver without schemas) and `payload.columns`.
pub fn request(database: &str, schema: &str, supports_schemas: bool) -> NavigatorRequest {
    let mut payload = serde_json::json!({ "database": database, "columns": true });
    let parent = if supports_schemas {
        payload["schema"] = schema.into();
        format!("schema:{database}:{schema}")
    } else {
        format!("database:{database}")
    };
    NavigatorRequest {
        parent: Some(parent),
        database: Some(database.to_owned()),
        payload: Some(payload),
        cursor: None,
        limit: super::sql_driver::PAGE_LIMIT,
    }
}

/// Reads every page `fetch` returns for `request` into one [`SchemaColumns`], page by page.
pub fn collect<E>(
    mut request: NavigatorRequest,
    mut fetch: impl FnMut(&NavigatorRequest) -> Result<NavigatorPage, E>,
) -> Result<SchemaColumns, SchemaColumnsError<E>> {
    let mut builder = Builder::default();
    let mut cursors = HashSet::new();
    loop {
        let page = fetch(&request).map_err(SchemaColumnsError::Fetch)?;
        if !builder.page(&page.nodes) {
            return Err(SchemaColumnsError::TooLarge);
        }
        match page.next_cursor {
            Some(next) => {
                if !cursors.insert(next.clone()) {
                    return Err(SchemaColumnsError::RepeatedCursor);
                }
                request.cursor = Some(next);
            }
            None => return Ok(builder.finish()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn column(table: &str, name: &str, data_type: &str) -> NavigatorNode {
        NavigatorNode {
            id: format!("column:{table}:{name}"),
            kind: "column".into(),
            label: name.into(),
            subtitle: Some(data_type.into()),
            icon: None,
            has_children: false,
            payload: json!({"object": table, "column": name, "name": name, "data_type": data_type, "nullable": true, "primary_key": false}),
            actions: Vec::new(),
        }
    }

    /// Pages of `size` nodes over `nodes`, the cursor being the position.
    fn pages(nodes: Vec<NavigatorNode>, size: usize) -> impl FnMut(&NavigatorRequest) -> Result<NavigatorPage, String> {
        move |request| {
            let from: usize = request.cursor.as_deref().map_or(0, |cursor| cursor.parse().unwrap());
            let to = (from + size).min(nodes.len());
            Ok(NavigatorPage {
                nodes: nodes[from..to].to_vec(),
                next_cursor: (to < nodes.len()).then(|| to.to_string()),
            })
        }
    }

    #[test]
    fn tables_keep_their_columns_in_order_across_pages_that_cut_them() {
        let nodes = vec![
            column("a", "id", "int"),
            column("a", "name", "text"),
            column("b", "id", "int"),
            column("b", "ref", "int"),
            column("b", "note", "text"),
        ];
        for size in [1, 2, 5] {
            let all = collect(request("d", "s", true), pages(nodes.clone(), size)).unwrap();
            let names = |table| all.table(table).unwrap().iter().map(|column| column.name.to_string()).collect::<Vec<_>>();
            assert_eq!(names("a"), ["id", "name"], "page size {size}");
            assert_eq!(names("b"), ["id", "ref", "note"]);
            assert_eq!(all.table("a").unwrap()[1].data_type.as_ref(), "text");
            assert!(all.table("c").is_none());
            assert_eq!((all.table_count(), all.column_count()), (2, 5));
        }
    }

    #[test]
    fn a_name_or_a_type_is_stored_once_for_the_whole_schema() {
        let nodes: Vec<_> = (0..50)
            .flat_map(|n| [column(&format!("t{n}"), "id", "bigint"), column(&format!("t{n}"), "created_at", "timestamp")])
            .collect();
        let all = collect(request("d", "s", true), pages(nodes, 20)).unwrap();
        let first = &all.table("t0").unwrap()[0];
        let last = &all.table("t49").unwrap()[0];
        assert!(Arc::ptr_eq(&first.name, &last.name) && Arc::ptr_eq(&first.data_type, &last.data_type));
    }

    /// 100 000 columns (the cap) are 3.2 MB of columns plus the one slice and key of each table.
    #[test]
    fn a_column_is_two_shared_strings_and_nothing_else() {
        assert_eq!(std::mem::size_of::<StoredColumn>(), 32);
    }

    #[test]
    fn a_table_is_forgotten_on_its_own() {
        let nodes = vec![column("a", "id", "int"), column("b", "id", "int")];
        let mut all = collect(request("d", "s", true), pages(nodes, 10)).unwrap();
        all.remove_table("a");
        assert!(all.table("a").is_none() && all.table("b").is_some());
        assert_eq!(all.column_count(), 1);
    }

    #[test]
    fn a_schema_over_the_cap_is_not_stored() {
        let nodes: Vec<_> = (0..MAX_COLUMNS + 1).map(|n| column(&format!("t{}", n / 50), &format!("c{}", n % 50), "int")).collect();
        let mut fetched = 0;
        let mut served = pages(nodes, 5_000);
        let result = collect(request("d", "s", true), |request| {
            fetched += 1;
            served(request)
        });
        assert!(matches!(result, Err(SchemaColumnsError::TooLarge)));
        assert_eq!(fetched, 21, "the read stops at the page that passes the cap");
    }

    #[test]
    fn a_driver_that_repeats_a_cursor_is_refused_and_a_failure_passes_through() {
        let looping = |_: &NavigatorRequest| -> Result<NavigatorPage, String> {
            Ok(NavigatorPage { nodes: vec![column("a", "id", "int")], next_cursor: Some("same".into()) })
        };
        assert!(matches!(collect(request("d", "s", true), looping), Err(SchemaColumnsError::RepeatedCursor)));
        let failing = |_: &NavigatorRequest| -> Result<NavigatorPage, String> { Err("boom".into()) };
        assert!(matches!(collect(request("d", "s", true), failing), Err(SchemaColumnsError::Fetch(error)) if error == "boom"));
    }

    #[test]
    fn the_request_names_the_schema_or_the_database_of_a_schemaless_driver() {
        let with = request("app", "public", true);
        assert_eq!(with.parent.as_deref(), Some("schema:app:public"));
        assert_eq!(with.payload, Some(json!({"database": "app", "schema": "public", "columns": true})));
        let without = request("main", "main", false);
        assert_eq!(without.parent.as_deref(), Some("database:main"));
        assert_eq!(without.payload, Some(json!({"database": "main", "columns": true})));
        assert_eq!(without.limit, super::super::sql_driver::PAGE_LIMIT);
    }
}
