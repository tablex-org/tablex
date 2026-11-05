//! Read-only restore of Swift workspace sessions.
//!
//! The Swift app persists `<data dir>/tablex/workspace-session.json`
//! with Mongo/Redis/pane fields this shell does not understand. We deserialize
//! only what we need (ignoring unknown fields), restore the SQL tabs we can
//! drive, and never write the file back — a reduced Rust schema would destroy
//! Swift-only state.

use crate::query_mode::QueryMode;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
struct SessionSnapshot {
    #[serde(default)]
    tabs: Vec<SessionTab>,
    #[serde(default, rename = "activeTabId")]
    active_tab_id: Option<String>,
    #[serde(default, rename = "runtimeStates")]
    runtime_states: std::collections::HashMap<String, RuntimeState>,
}

#[derive(Debug, Deserialize)]
struct SessionTab {
    id: String,
    #[serde(default, rename = "connectionUUID")]
    connection_uuid: Option<String>,
    #[serde(default, rename = "databaseName")]
    database_name: String,
    #[serde(default, rename = "schemaName")]
    schema: Option<String>,
    #[serde(default, rename = "collectionName")]
    collection: String,
    #[serde(default)]
    kind: String,
    #[serde(default, rename = "queryMode")]
    query_mode: Option<QueryMode>,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "aggregatePipelineJson")]
    aggregate_pipeline_json: Option<String>,
    #[serde(default, rename = "isPinned")]
    is_pinned: bool,
}

#[derive(Debug, Deserialize)]
struct RuntimeState {
    #[serde(default, rename = "postgresSQLText")]
    postgres_sql_text: Option<String>,
    #[serde(default, rename = "postgresSQLOutputLimit")]
    postgres_sql_output_limit: Option<i64>,
    /// WHERE / ORDER BY fragments of a table-data (`sqlTable`) tab.
    #[serde(default, rename = "postgresWhereClause")]
    postgres_where_clause: Option<String>,
    #[serde(default, rename = "postgresOrderByClause")]
    postgres_order_by_clause: Option<String>,
}

/// A SQL tab worth restoring, derived from a Swift session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredTab {
    pub source_id: String,
    pub active: bool,
    pub kind: String,
    /// Saved-connection UUID the tab should bind to.
    pub connection_id: String,
    pub database: String,
    pub schema: Option<String>,
    pub collection: String,
    pub query_mode: Option<QueryMode>,
    pub title: String,
    pub sql: String,
    pub output_limit: i64,
    pub is_pinned: bool,
    /// `sqlTable` tabs: persisted WHERE fragment.
    pub where_clause: String,
    /// `sqlTable` tabs: persisted ORDER BY fragment.
    pub order_by: String,
}

/// Reads the Swift workspace session and returns the tabs this shell can
/// drive: `queryDraft`/`postgresTable` SQL tabs bound to a PostgreSQL or
/// SQLite connection. Returns `None` when the file is missing or malformed —
/// restore is best-effort and never fatal.
pub fn restore_sql_tabs() -> Option<Vec<RestoredTab>> {
    let path = session_path();
    let data = std::fs::read_to_string(path).ok()?;
    restore_from_json(&data)
}

fn session_path() -> PathBuf {
    crate::platform::app_data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("workspace-session.json")
}

/// Pure extraction so fixture tests run without touching the user data dir.
fn restore_from_json(json: &str) -> Option<Vec<RestoredTab>> {
    let snapshot: SessionSnapshot = serde_json::from_str(json).ok()?;
    let mut restored = Vec::new();
    for tab in snapshot.tabs {
        // Only SQL drafts are restorable in this milestone.
        if tab.kind != "queryDraft" {
            continue;
        }
        // A tab without a connection runs nothing; skip rather than guess.
        let Some(connection_id) = tab.connection_uuid else {
            continue;
        };
        if connection_id.is_empty() {
            continue;
        }
        let state = snapshot.runtime_states.get(&tab.id);
        // SQL text lives in the per-tab runtime state, falling back to the
        // pipeline JSON field Swift used to store SQL drafts in.
        let sql = snapshot
            .runtime_states
            .get(&tab.id)
            .and_then(|state| state.postgres_sql_text.clone())
            .or(tab.aggregate_pipeline_json)
            .unwrap_or_default();
        let output_limit = snapshot
            .runtime_states
            .get(&tab.id)
            .and_then(|state| state.postgres_sql_output_limit)
            .unwrap_or(crate::query::DEFAULT_OUTPUT_LIMIT);
        restored.push(RestoredTab {
            active: snapshot.active_tab_id.as_deref() == Some(&tab.id),
            source_id: tab.id,
            kind: tab.kind,
            connection_id,
            database: tab.database_name,
            schema: tab.schema,
            collection: tab.collection,
            query_mode: tab.query_mode,
            title: if tab.title.is_empty() {
                "SQL Query".to_string()
            } else {
                tab.title
            },
            sql,
            output_limit,
            is_pinned: tab.is_pinned,
            where_clause: state
                .and_then(|state| state.postgres_where_clause.clone())
                .unwrap_or_default(),
            order_by: state
                .and_then(|state| state.postgres_order_by_clause.clone())
                .unwrap_or_default(),
        });
    }
    Some(restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_supported_sql_draft_tabs() {
        let json = r#"{
          "version": 1,
          "tabs": [
            {"id": "t1", "connectionUUID": "conn-1", "databaseName": "app",
             "kind": "queryDraft", "title": "Monthly report",
             "aggregatePipelineJson": "SELECT 1"},
            {"id": "t2", "connectionUUID": "conn-2", "databaseName": "shop",
             "kind": "postgresTrigger", "title": "Trigger"}
          ],
          "activeTabId": "t1",
          "runtimeStates": {
            "t1": {"postgresSQLText": "SELECT * FROM orders", "postgresSQLOutputLimit": 42}
          },
          "someSwiftOnlyField": {"paneLayout": []}
        }"#;

        let restored = restore_from_json(json).expect("restore");
        assert_eq!(restored.len(), 1, "only queryDraft tabs restore");
        assert_eq!(restored[0].connection_id, "conn-1");
        assert!(restored[0].active);
        assert_eq!(restored[0].database, "app");
        assert_eq!(restored[0].title, "Monthly report");
        assert_eq!(
            restored[0].sql, "SELECT * FROM orders",
            "runtime SQL text wins over the legacy pipeline field"
        );
        assert_eq!(restored[0].output_limit, 42);
    }

    #[test]
    fn restores_tab_metadata_needed_for_future_workspace_parity() {
        let json = r#"{
          "tabs": [
            {"id": "t1", "connectionUUID": "conn-1", "databaseName": "app",
             "schemaName": "public", "collectionName": "orders",
             "kind": "queryDraft", "queryMode": "postgresTable",
             "title": "Orders", "isPinned": true}
          ],
          "activeTabId": "t1",
          "runtimeStates": {
            "t1": {"postgresSQLText": "SELECT * FROM orders", "postgresSQLOutputLimit": 100}
          }
        }"#;

        let restored = restore_from_json(json).expect("restore");
        assert_eq!(restored[0].kind, "queryDraft");
        assert_eq!(restored[0].query_mode, Some(QueryMode::PostgresTable));
        assert_eq!(restored[0].schema.as_deref(), Some("public"));
        assert_eq!(restored[0].collection, "orders");
        assert!(restored[0].is_pinned);
    }

    #[test]
    fn falls_back_to_pipeline_json_when_runtime_state_is_missing() {
        let json = r#"{
          "tabs": [
            {"id": "t9", "connectionUUID": "c", "databaseName": "",
             "kind": "queryDraft", "title": "", "aggregatePipelineJson": "SELECT 2"}
          ]
        }"#;

        let restored = restore_from_json(json).expect("restore");
        assert_eq!(restored[0].sql, "SELECT 2");
        assert_eq!(restored[0].title, "SQL Query", "empty title gets a default");
    }

    #[test]
    fn skips_tabs_without_connections() {
        let json = r#"{
          "tabs": [
            {"id": "t1", "kind": "queryDraft", "title": "Orphan"},
            {"id": "t2", "connectionUUID": "", "kind": "queryDraft", "title": "Empty"},
            {"id": "t3", "connectionUUID": "conn-1", "databaseName": "",
             "kind": "queryDraft", "title": "Kept"}
          ]
        }"#;

        let restored = restore_from_json(json).expect("restore");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].title, "Kept");
    }

    #[test]
    fn malformed_json_returns_none_instead_of_panicking() {
        assert!(restore_from_json("{not valid").is_none());
        assert!(restore_from_json("{\"tabs\": \"oops\"}").is_none());
    }

    #[test]
    fn restores_table_data_tab_where_and_order_by() {
        let json = r#"{
          "tabs": [
            {"id": "t1", "connectionUUID": "conn-1", "databaseName": "app",
             "schemaName": "public", "collectionName": "orders",
             "kind": "queryDraft", "queryMode": "sqlTable", "title": "orders"},
            {"id": "t2", "connectionUUID": "conn-1", "databaseName": "app",
             "kind": "queryDraft", "title": "plain"}
          ],
          "runtimeStates": {
            "t1": {"postgresWhereClause": "id > 10 AND status = 'active'",
                   "postgresOrderByClause": "created DESC"}
          }
        }"#;

        let restored = restore_from_json(json).expect("restore");
        assert_eq!(restored[0].query_mode, Some(QueryMode::SqlTable));
        assert_eq!(restored[0].where_clause, "id > 10 AND status = 'active'");
        assert_eq!(restored[0].order_by, "created DESC");
        assert_eq!(restored[1].where_clause, "", "editor tabs carry no fragments");
    }

    #[test]
    fn mongo_kind_tabs_do_not_restore() {
        let json = r#"{
          "tabs": [
            {"id": "t1", "connectionUUID": "m", "databaseName": "db",
             "kind": "collection", "title": "users"}
          ]
        }"#;

        let restored = restore_from_json(json).expect("restore parses");
        assert!(restored.is_empty(), "collection tabs are skipped");
    }
}
