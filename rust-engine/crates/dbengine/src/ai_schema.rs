use crate::sql_completion::{
    CatalogSnapshot, ColumnInfo, ForeignKeyInfo, IndexInfo, SchemaInfo, TableInfo,
};
use serde::{Deserialize, Serialize};

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DigestCaps {
    pub max_tables: usize,
    pub max_columns_per_table: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DigestRequest {
    pub catalog: CatalogSnapshot,
    pub query: String,
    #[serde(default = "DigestCaps::default_caps")]
    pub caps: DigestCaps,
    #[serde(default)]
    pub mentioned_tables: Vec<String>,
}

impl DigestCaps {
    fn default_caps() -> Self {
        Self {
            max_tables: 20,
            max_columns_per_table: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDigestTable {
    pub schema: String,
    pub name: String,
    pub columns: Vec<SchemaDigestColumn>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<SchemaDigestIndex>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<SchemaDigestForeignKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDigestColumn {
    pub name: String,
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    #[serde(skip_serializing_if = "is_false")]
    pub is_primary_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDigestIndex {
    pub name: String,
    pub columns: Vec<String>,
    pub is_unique: bool,
    pub is_primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDigestForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub foreign_schema: String,
    pub foreign_table: String,
    pub foreign_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SchemaDigest {
    pub tables: Vec<SchemaDigestTable>,
    pub truncated: bool,
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

struct ScoredTable<'a> {
    schema: &'a SchemaInfo,
    table: &'a TableInfo,
    score: i32,
}

fn table_relevance(table: &TableInfo, tokens: &[String]) -> i32 {
    let table_lower = table.name.to_ascii_lowercase();
    let mut score = 0;
    for token in tokens {
        if table_lower == *token {
            score += 10;
        } else if table_lower.contains(token) {
            score += 5;
        }
        for col in &table.columns {
            let col_lower = col.name.to_ascii_lowercase();
            if col_lower == *token {
                score += 3;
            }
        }
    }
    score
}

fn column_relevance(col: &ColumnInfo, tokens: &[String]) -> i32 {
    let col_lower = col.name.to_ascii_lowercase();
    let mut score = 0;
    for token in tokens {
        if col_lower == *token {
            score += 3;
        } else if col_lower.contains(token) {
            score += 1;
        }
    }
    score
}

fn mentioned_table_score(
    schema: &SchemaInfo,
    table: &TableInfo,
    mentioned_tables: &[String],
) -> i32 {
    if mentioned_tables.is_empty() {
        return 0;
    }
    let schema_lower = schema.name.to_ascii_lowercase();
    let table_lower = table.name.to_ascii_lowercase();
    let qualified = format!("{}.{}", schema_lower, table_lower);
    let quoted_qualified = format!("{}.\"{}\"", schema_lower, table_lower);
    for mention in mentioned_tables {
        let mention = mention.trim().trim_matches('"').to_ascii_lowercase();
        if mention == table_lower || mention == qualified || mention == quoted_qualified {
            return 1_000;
        }
    }
    0
}

fn digest_index(index: &IndexInfo) -> SchemaDigestIndex {
    SchemaDigestIndex {
        name: index.name.clone(),
        columns: index.columns.clone(),
        is_unique: index.is_unique,
        is_primary: index.is_primary,
    }
}

fn digest_foreign_key(foreign_key: &ForeignKeyInfo) -> SchemaDigestForeignKey {
    SchemaDigestForeignKey {
        name: foreign_key.name.clone(),
        columns: foreign_key.columns.clone(),
        foreign_schema: foreign_key.foreign_schema.clone(),
        foreign_table: foreign_key.foreign_table.clone(),
        foreign_columns: foreign_key.foreign_columns.clone(),
    }
}

pub fn build_schema_digest(
    catalog: &CatalogSnapshot,
    query: &str,
    caps: &DigestCaps,
    mentioned_tables: &[String],
) -> SchemaDigest {
    let tokens = tokenize_query(query);
    let total_tables: usize = catalog.schemas.iter().map(|s| s.tables.len()).sum();

    let mut scored: Vec<ScoredTable> = catalog
        .schemas
        .iter()
        .flat_map(|schema| {
            schema.tables.iter().map(|table| ScoredTable {
                schema,
                table,
                score: table_relevance(table, &tokens)
                    + mentioned_table_score(schema, table, mentioned_tables),
            })
        })
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.schema.name.cmp(&b.schema.name))
            .then_with(|| a.table.name.cmp(&b.table.name))
    });

    let truncated = total_tables > caps.max_tables;
    let selected = scored.into_iter().take(caps.max_tables);

    let tables: Vec<SchemaDigestTable> = selected
        .map(|st| {
            let mut cols: Vec<&ColumnInfo> = st.table.columns.iter().collect();
            cols.sort_by(|a, b| {
                column_relevance(b, &tokens)
                    .cmp(&column_relevance(a, &tokens))
                    .then_with(|| a.name.cmp(&b.name))
            });
            let cols = cols
                .into_iter()
                .take(caps.max_columns_per_table)
                .map(|c| SchemaDigestColumn {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    is_primary_key: c.is_primary_key,
                    comment: c.comment.clone(),
                })
                .collect();

            SchemaDigestTable {
                schema: st.schema.name.clone(),
                name: st.table.name.clone(),
                columns: cols,
                indexes: st.table.indexes.iter().map(digest_index).collect(),
                foreign_keys: st
                    .table
                    .foreign_keys
                    .iter()
                    .map(digest_foreign_key)
                    .collect(),
                comment: st.table.comment.clone(),
            }
        })
        .collect();

    let any_col_truncated = catalog.schemas.iter().any(|s| {
        s.tables
            .iter()
            .any(|t| t.columns.len() > caps.max_columns_per_table)
    });

    SchemaDigest {
        tables,
        truncated: truncated || any_col_truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_catalog(schemas: Vec<SchemaInfo>) -> CatalogSnapshot {
        CatalogSnapshot {
            current_schema: None,
            current_table: None,
            schemas,
        }
    }

    fn make_schema(name: &str, tables: Vec<TableInfo>) -> SchemaInfo {
        SchemaInfo {
            name: name.to_string(),
            tables,
        }
    }

    fn make_table(name: &str, columns: Vec<ColumnInfo>) -> TableInfo {
        TableInfo {
            name: name.to_string(),
            columns,
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            comment: None,
        }
    }

    fn make_col(name: &str, data_type: Option<&str>) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            data_type: data_type.map(|s| s.to_string()),
            nullable: None,
            is_primary_key: false,
            comment: None,
        }
    }

    fn make_table_with_metadata(name: &str) -> TableInfo {
        TableInfo {
            name: name.to_string(),
            columns: vec![ColumnInfo {
                name: "user_id".to_string(),
                data_type: Some("uuid".to_string()),
                nullable: Some(false),
                is_primary_key: false,
                comment: Some("Owner user id".to_string()),
            }],
            indexes: vec![crate::sql_completion::IndexInfo {
                name: "idx_orders_user_id".to_string(),
                columns: vec!["user_id".to_string()],
                is_unique: false,
                is_primary: false,
            }],
            foreign_keys: vec![crate::sql_completion::ForeignKeyInfo {
                name: "orders_user_id_fkey".to_string(),
                columns: vec!["user_id".to_string()],
                foreign_schema: "public".to_string(),
                foreign_table: "users".to_string(),
                foreign_columns: vec!["id".to_string()],
            }],
            comment: Some("Purchase orders".to_string()),
        }
    }

    #[test]
    fn tables_matching_query_tokens_rank_first() {
        let catalog = make_catalog(vec![make_schema(
            "public",
            vec![
                make_table("orders", vec![make_col("id", Some("int8"))]),
                make_table("users", vec![make_col("id", Some("int8"))]),
            ],
        )]);
        let caps = DigestCaps {
            max_tables: 10,
            max_columns_per_table: 10,
        };
        let digest = build_schema_digest(&catalog, "SELECT * FROM users", &caps, &[]);
        assert_eq!(digest.tables[0].name, "users");
    }

    #[test]
    fn max_tables_cap_respected_and_truncated_flag() {
        let catalog = make_catalog(vec![make_schema(
            "public",
            vec![
                make_table("t1", vec![make_col("a", None)]),
                make_table("t2", vec![make_col("b", None)]),
                make_table("t3", vec![make_col("c", None)]),
            ],
        )]);
        let caps = DigestCaps {
            max_tables: 2,
            max_columns_per_table: 10,
        };
        let digest = build_schema_digest(&catalog, "", &caps, &[]);
        assert_eq!(digest.tables.len(), 2);
        assert!(digest.truncated);
    }

    #[test]
    fn max_columns_per_table_cap_respected() {
        let catalog = make_catalog(vec![make_schema(
            "public",
            vec![make_table(
                "big",
                (0..5).map(|i| make_col(&format!("c{}", i), None)).collect(),
            )],
        )]);
        let caps = DigestCaps {
            max_tables: 10,
            max_columns_per_table: 3,
        };
        let digest = build_schema_digest(&catalog, "", &caps, &[]);
        assert_eq!(digest.tables[0].columns.len(), 3);
        assert!(digest.truncated);
    }

    #[test]
    fn deterministic_ordering_for_ties() {
        let catalog = make_catalog(vec![
            make_schema(
                "alpha",
                vec![make_table("zebra", vec![make_col("id", None)])],
            ),
            make_schema(
                "beta",
                vec![make_table("alpha", vec![make_col("id", None)])],
            ),
        ]);
        let caps = DigestCaps {
            max_tables: 10,
            max_columns_per_table: 10,
        };
        let digest = build_schema_digest(&catalog, "", &caps, &[]);
        assert_eq!(digest.tables[0].schema, "alpha");
        assert_eq!(digest.tables[0].name, "zebra");
        assert_eq!(digest.tables[1].schema, "beta");
        assert_eq!(digest.tables[1].name, "alpha");
    }

    #[test]
    fn empty_catalog_returns_empty_digest() {
        let catalog = make_catalog(vec![]);
        let caps = DigestCaps {
            max_tables: 10,
            max_columns_per_table: 10,
        };
        let digest = build_schema_digest(&catalog, "SELECT 1", &caps, &[]);
        assert!(digest.tables.is_empty());
        assert!(!digest.truncated);
    }

    #[test]
    fn digest_request_roundtrip() {
        let request_json = r#"{"catalog":{"current_schema":null,"current_table":null,"schemas":[{"name":"public","tables":[{"name":"users","columns":[{"name":"id","data_type":"int8"}]}]}]},"query":"SELECT * FROM users","caps":{"max_tables":20,"max_columns_per_table":10}}"#;
        let request: DigestRequest = serde_json::from_str(request_json).unwrap();
        assert_eq!(request.query, "SELECT * FROM users");
        assert_eq!(request.caps.max_tables, 20);
        let digest = build_schema_digest(
            &request.catalog,
            &request.query,
            &request.caps,
            &request.mentioned_tables,
        );
        assert_eq!(digest.tables.len(), 1);
        assert_eq!(digest.tables[0].name, "users");
    }

    #[test]
    fn digest_request_defaults_caps() {
        let request_json = r#"{"catalog":{"current_schema":null,"current_table":null,"schemas":[{"name":"public","tables":[{"name":"t","columns":[]}]}]},"query":""}"#;
        let request: DigestRequest = serde_json::from_str(request_json).unwrap();
        assert_eq!(request.caps.max_tables, 20);
        assert_eq!(request.caps.max_columns_per_table, 10);
    }

    #[test]
    fn digest_preserves_table_column_index_and_foreign_key_metadata() {
        let catalog = make_catalog(vec![make_schema(
            "public",
            vec![make_table_with_metadata("orders")],
        )]);
        let caps = DigestCaps {
            max_tables: 10,
            max_columns_per_table: 10,
        };
        let digest = build_schema_digest(&catalog, "SELECT * FROM orders", &caps, &[]);

        let table = &digest.tables[0];
        assert_eq!(table.comment.as_deref(), Some("Purchase orders"));
        assert_eq!(table.indexes[0].name, "idx_orders_user_id");
        assert_eq!(table.indexes[0].columns, vec!["user_id"]);
        assert_eq!(table.foreign_keys[0].foreign_table, "users");

        let column = &table.columns[0];
        assert_eq!(column.comment.as_deref(), Some("Owner user id"));
        assert_eq!(column.nullable, Some(false));
        assert!(!column.is_primary_key);
    }

    #[test]
    fn mentioned_tables_are_included_before_unmentioned_relevance_matches() {
        let catalog = make_catalog(vec![make_schema(
            "public",
            vec![
                make_table("orders", vec![make_col("id", None)]),
                make_table("users", vec![make_col("email", None)]),
            ],
        )]);
        let caps = DigestCaps {
            max_tables: 1,
            max_columns_per_table: 10,
        };
        let request = DigestRequest {
            catalog,
            query: "SELECT email FROM users".to_string(),
            caps,
            mentioned_tables: vec!["public.orders".to_string()],
        };

        let digest = build_schema_digest(
            &request.catalog,
            &request.query,
            &request.caps,
            &request.mentioned_tables,
        );
        assert_eq!(digest.tables[0].name, "orders");
        assert!(digest.truncated);
    }
}
