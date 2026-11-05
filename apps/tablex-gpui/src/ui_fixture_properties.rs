//! `TableProperties` answers of the UI fixture engine (the fixture stand-in for
//! `describe_object`). PostgreSQL reports every list; SQLite cannot
//! report References, so that sub-tab is hidden for it.

use crate::ui_fixture::{CLICKHOUSE_ID, POSTGRES_ID, SQLITE_ID};
use crate::ui_scenario::FixtureKind;
use crate::catalog::{
    CatalogColumn, CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex,
    CatalogTrigger, TableProperties,
};

#[cfg(test)]
thread_local! {
    /// Number of upcoming fixture answers that fail (tests: error -> retry).
    pub static FAIL_NEXT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// One described column (the structure editor starts from these).
fn column(
    name: &str,
    data_type: &str,
    nullable: bool,
    primary_key: bool,
    default: Option<&str>,
    comment: Option<&str>,
) -> CatalogColumn {
    CatalogColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable,
        primary_key,
        default_expression: default.map(str::to_owned),
        comment: comment.map(str::to_owned),
        ..CatalogColumn::default()
    }
}

pub fn table_properties(
    kind: FixtureKind,
    connection: &str,
    schema: &str,
    object: &str,
) -> Result<TableProperties, String> {
    #[cfg(test)]
    if FAIL_NEXT.with(|fail| {
        let remaining = fail.get();
        fail.set(remaining.saturating_sub(1));
        remaining > 0
    }) {
        return Err("fixture properties error".into());
    }
    if kind == FixtureKind::Error {
        return Err("fixture properties error".into());
    }
    let qualified = if schema.is_empty() {
        object.to_string()
    } else {
        format!("{schema}.{object}")
    };
    match connection {
        POSTGRES_ID => Ok(postgres(&qualified, object)),
        SQLITE_ID => Ok(sqlite(object)),
        CLICKHOUSE_ID => Ok(clickhouse(&qualified)),
        _ => Err("fixture connection has no table properties".into()),
    }
}

fn postgres(qualified: &str, object: &str) -> TableProperties {
    let foreign_key = |name: &str, table: &str, columns: &[&str], target: &str, target_columns: &[&str]| {
        CatalogForeignKey {
            name: name.into(),
            table: table.into(),
            columns: strings(columns),
            referenced_table: target.into(),
            referenced_columns: strings(target_columns),
            on_update: Some("NO ACTION".into()),
            on_delete: Some("CASCADE".into()),
            definition: None,
        }
    };
    let index = |name: &str, columns: &[&str], unique: bool, primary: bool, method: &str| {
        CatalogIndex {
            name: name.into(),
            columns: strings(columns),
            unique,
            primary,
            method: Some(method.into()),
            definition: Some(format!(
                "CREATE {}INDEX {name} ON {qualified} USING {method} ({})",
                if unique { "UNIQUE " } else { "" },
                columns.join(", ")
            )),
        }
    };
    TableProperties {
        columns: vec![
            column("id", "bigint", false, true, Some(&format!("nextval('{object}_id_seq'::regclass)")), None),
            column("name", "text", false, false, None, Some("Customer-facing order name")),
            column("status", "text", true, false, Some("'new'::text"), Some("Order lifecycle state")),
            column("active", "boolean", false, false, Some("true"), None),
            column("created", "date", false, false, Some("CURRENT_DATE"), None),
            column("score", "double precision", false, false, Some("0"), None),
            column("big_number", "bigint", false, false, None, None),
            column("payload", "jsonb", false, false, None, None),
        ],
        keys: Some(vec![
            CatalogConstraint {
                name: format!("{object}_pkey"),
                kind: CatalogConstraintKind::PrimaryKey,
                columns: strings(&["id"]),
                definition: Some("PRIMARY KEY (id)".into()),
            },
            CatalogConstraint {
                name: format!("{object}_name_key"),
                kind: CatalogConstraintKind::Unique,
                columns: strings(&["name"]),
                definition: Some("UNIQUE (name)".into()),
            },
            CatalogConstraint {
                name: format!("{object}_score_check"),
                kind: CatalogConstraintKind::Check,
                columns: strings(&["score"]),
                definition: Some("CHECK (score >= 0)".into()),
            },
        ]),
        foreign_keys: Some(vec![
            foreign_key(
                &format!("{object}_customer_fk"),
                qualified,
                &["customer_id"],
                "public.customers",
                &["id"],
            ),
            foreign_key(
                &format!("{object}_region_fk"),
                qualified,
                &["region_id", "country"],
                "public.regions",
                &["id", "country"],
            ),
        ]),
        indexes: Some(vec![
            index(&format!("{object}_pkey"), &["id"], true, true, "btree"),
            index(&format!("{object}_name_key"), &["name"], true, false, "btree"),
            index(&format!("{object}_payload_idx"), &["payload"], false, false, "gin"),
        ]),
        references: Some(vec![foreign_key(
            "order_items_order_fk",
            "public.order_items",
            &["order_id"],
            qualified,
            &["id"],
        )]),
        triggers: Some(vec![
            CatalogTrigger {
                name: format!("{object}_audit"),
                timing: Some("AFTER".into()),
                events: strings(&["INSERT", "UPDATE", "DELETE"]),
                enabled: Some(true),
                definition: Some(format!(
                    "CREATE TRIGGER {object}_audit\n    AFTER INSERT OR UPDATE OR DELETE ON {qualified}\n    FOR EACH ROW EXECUTE FUNCTION audit.record_change()"
                )),
            },
            CatalogTrigger {
                name: format!("{object}_touch"),
                timing: Some("BEFORE".into()),
                events: strings(&["UPDATE"]),
                enabled: Some(false),
                definition: Some(format!(
                    "CREATE TRIGGER {object}_touch BEFORE UPDATE ON {qualified} FOR EACH ROW EXECUTE FUNCTION touch_updated_at()"
                )),
            },
        ]),
        ddl: Some(format!(
            "CREATE TABLE {qualified} (\n    id bigint NOT NULL,\n    name text NOT NULL,\n    status text,\n    active boolean NOT NULL,\n    created date NOT NULL,\n    score double precision NOT NULL,\n    big_number bigint NOT NULL,\n    payload jsonb NOT NULL,\n    CONSTRAINT {object}_pkey PRIMARY KEY (id),\n    CONSTRAINT {object}_score_check CHECK (score >= 0)\n);\n\nCREATE UNIQUE INDEX {object}_name_key ON {qualified} USING btree (name);"
        )),
    }
}

/// ClickHouse has no constraints: the key lists carry the sorting key, the
/// partitioning and the TTL (`other`), and the DDL leads with a stats comment.
fn clickhouse(qualified: &str) -> TableProperties {
    let key = |name: &str, kind: CatalogConstraintKind, columns: &[&str], definition: &str| {
        CatalogConstraint {
            name: name.into(),
            kind,
            columns: strings(columns),
            definition: Some(definition.into()),
        }
    };
    TableProperties {
        columns: vec![
            column("id", "UInt64", false, true, None, None),
            column("name", "String", false, false, None, None),
            column("created", "DateTime", false, false, None, None),
        ],
        keys: Some(vec![
            key("PRIMARY", CatalogConstraintKind::PrimaryKey, &["id"], "id"),
            key("ORDER BY", CatalogConstraintKind::Other, &["id", "created"], "id, created"),
            key("PARTITION BY", CatalogConstraintKind::Other, &["toYYYYMM(created)"], "toYYYYMM(created)"),
            key("TTL", CatalogConstraintKind::Other, &["created + toIntervalDay(30)"], "created + toIntervalDay(30)"),
        ]),
        foreign_keys: Some(Vec::new()),
        indexes: Some(vec![CatalogIndex {
            name: "name_idx".into(),
            columns: strings(&["name"]),
            unique: false,
            primary: false,
            method: Some("bloom_filter GRANULARITY 4".into()),
            definition: Some("(name) TYPE bloom_filter".into()),
        }]),
        references: Some(Vec::new()),
        triggers: Some(Vec::new()),
        ddl: Some(format!(
            "-- engine: MergeTree, rows: 240, bytes: 18432, active parts: 2\nCREATE TABLE {qualified}\n(\n    `id` UInt64,\n    `name` String,\n    `created` DateTime\n)\nENGINE = MergeTree\nPARTITION BY toYYYYMM(created)\nORDER BY (id, created)\nTTL created + toIntervalDay(30)"
        )),
    }
}

fn sqlite(object: &str) -> TableProperties {
    TableProperties {
        columns: vec![
            column("id", "INTEGER", true, true, None, None),
            column("name", "TEXT", false, false, None, None),
        ],
        keys: Some(vec![CatalogConstraint {
            name: format!("{object}_pk"),
            kind: CatalogConstraintKind::PrimaryKey,
            columns: strings(&["id"]),
            definition: None,
        }]),
        foreign_keys: Some(Vec::new()),
        indexes: Some(vec![CatalogIndex {
            name: format!("sqlite_autoindex_{object}_1"),
            columns: strings(&["id"]),
            unique: true,
            primary: true,
            method: None,
            definition: None,
        }]),
        // SQLite cannot cheaply report referencing tables: hidden sub-tab.
        references: None,
        triggers: Some(Vec::new()),
        ddl: Some(format!(
            "CREATE TABLE \"{object}\" (\n    id INTEGER PRIMARY KEY,\n    name TEXT NOT NULL\n)"
        )),
    }
}
