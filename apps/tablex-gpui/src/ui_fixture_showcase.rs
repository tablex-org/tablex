//! The `showcase` scenario's workspace: what the UI fixture engine answers when the app is
//! shown the way it looks in daily use (marketing screenshots, docs). A small fictional online
//! shop: a PostgreSQL `shop` database with its tables, views and their keys, indexes and triggers,
//! a MongoDB `shop` database holding the orders, plus the connections around them.
//!
//! Everything is made up (names, `example.com` addresses, amounts, the `prod-db.example.com`
//! host) and none of it says "fixture": every string here can end up on screen. It is generated
//! on request from compact tables, so nothing is built before the scenario asks for it.

use crate::catalog::{
    CatalogColumn, CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex,
    CatalogTrigger, TableProperties,
};
use crate::navigator::{self, KindOfObject, LoadedChild, ObjectGroup};
use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, Decimal128Array, Int64Array, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use bson::{oid::ObjectId, Bson, DateTime, Document};
use std::sync::Arc;

/// The PostgreSQL database and its two schemas.
pub const DATABASE: &str = "shop";
pub const SCHEMAS: [&str; 2] = ["public", "analytics"];
/// Host of the Production connection (the reserved `example.com` domain).
pub const PRODUCTION_HOST: &str = "prod-db.example.com";

// ---- the PostgreSQL catalog --------------------------------------------------------------------

const NULLABLE: u8 = 1;
const PRIMARY: u8 = 2;

/// `(name, type, flags, default)`.
struct Col(&'static str, &'static str, u8, Option<&'static str>);
/// `(name, columns, unique, method)`.
struct Idx(&'static str, &'static [&'static str], bool, &'static str);
/// `(name, columns, referenced table, referenced columns, on delete)`.
struct Fk(&'static str, &'static [&'static str], &'static str, &'static [&'static str], &'static str);
/// `(name, timing, events, enabled)`.
struct Trg(&'static str, &'static str, &'static [&'static str], bool);

struct Table {
    schema: &'static str,
    name: &'static str,
    columns: &'static [Col],
    indexes: &'static [Idx],
    /// `(name, column, expression)`.
    checks: &'static [(&'static str, &'static str, &'static str)],
    fks: &'static [Fk],
    triggers: &'static [Trg],
}

const TOUCH: Trg = Trg("touch_updated_at", "BEFORE", &["UPDATE"], true);

static TABLES: &[Table] = &[
    Table {
        schema: "public",
        name: "customers",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('customers_id_seq'::regclass)")),
            Col("name", "text", 0, None),
            Col("email", "text", 0, None),
            Col("country", "text", 0, None),
            Col("status", "text", 0, Some("'active'::text")),
            Col("marketing_opt_in", "boolean", 0, Some("false")),
            Col("created_at", "timestamptz", 0, Some("now()")),
            Col("lifetime_value", "numeric(12,2)", 0, Some("0")),
        ],
        indexes: &[
            Idx("customers_email_key", &["email"], true, "btree"),
            Idx("customers_country_idx", &["country"], false, "btree"),
            Idx("customers_created_at_idx", &["created_at"], false, "btree"),
        ],
        checks: &[("customers_status_check", "status", "status = ANY (ARRAY['new', 'active', 'vip', 'inactive'])")],
        fks: &[],
        triggers: &[Trg("customers_touch", "BEFORE", &["UPDATE"], true)],
    },
    Table {
        schema: "public",
        name: "orders",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('orders_id_seq'::regclass)")),
            Col("customer_id", "bigint", 0, None),
            Col("status", "text", 0, Some("'pending'::text")),
            Col("currency", "character(3)", 0, Some("'USD'::bpchar")),
            Col("subtotal", "numeric(12,2)", 0, None),
            Col("tax", "numeric(12,2)", 0, Some("0")),
            Col("total", "numeric(12,2)", 0, None),
            Col("coupon_id", "bigint", NULLABLE, None),
            Col("placed_at", "timestamptz", 0, Some("now()")),
            Col("shipped_at", "timestamptz", NULLABLE, None),
            Col("updated_at", "timestamptz", 0, Some("now()")),
        ],
        indexes: &[
            Idx("orders_customer_id_idx", &["customer_id"], false, "btree"),
            Idx("orders_status_placed_at_idx", &["status", "placed_at"], false, "btree"),
            Idx("orders_placed_at_idx", &["placed_at"], false, "brin"),
        ],
        checks: &[
            ("orders_total_check", "total", "total >= 0"),
            ("orders_status_check", "status", "status = ANY (ARRAY['pending', 'paid', 'shipped', 'delivered', 'refunded', 'cancelled'])"),
        ],
        fks: &[
            Fk("orders_customer_id_fkey", &["customer_id"], "customers", &["id"], "RESTRICT"),
            Fk("orders_coupon_id_fkey", &["coupon_id"], "coupons", &["id"], "SET NULL"),
        ],
        triggers: &[
            Trg("orders_touch", "BEFORE", &["UPDATE"], true),
            Trg("orders_audit", "AFTER", &["INSERT", "UPDATE", "DELETE"], true),
        ],
    },
    Table {
        schema: "public",
        name: "order_items",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('order_items_id_seq'::regclass)")),
            Col("order_id", "bigint", 0, None),
            Col("product_id", "bigint", 0, None),
            Col("quantity", "integer", 0, Some("1")),
            Col("unit_price", "numeric(12,2)", 0, None),
            Col("discount", "numeric(5,2)", 0, Some("0")),
        ],
        indexes: &[
            Idx("order_items_order_id_product_id_key", &["order_id", "product_id"], true, "btree"),
            Idx("order_items_product_id_idx", &["product_id"], false, "btree"),
        ],
        checks: &[("order_items_quantity_check", "quantity", "quantity > 0")],
        fks: &[
            Fk("order_items_order_id_fkey", &["order_id"], "orders", &["id"], "CASCADE"),
            Fk("order_items_product_id_fkey", &["product_id"], "products", &["id"], "RESTRICT"),
        ],
        triggers: &[],
    },
    Table {
        schema: "public",
        name: "products",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('products_id_seq'::regclass)")),
            Col("sku", "text", 0, None),
            Col("name", "text", 0, None),
            Col("category", "text", 0, None),
            Col("price", "numeric(12,2)", 0, None),
            Col("cost", "numeric(12,2)", 0, None),
            Col("active", "boolean", 0, Some("true")),
            Col("created_at", "timestamptz", 0, Some("now()")),
        ],
        indexes: &[
            Idx("products_sku_key", &["sku"], true, "btree"),
            Idx("products_category_idx", &["category"], false, "btree"),
        ],
        checks: &[("products_price_check", "price", "price >= 0")],
        fks: &[],
        triggers: &[Trg("products_touch", "BEFORE", &["UPDATE"], true)],
    },
    Table {
        schema: "public",
        name: "invoices",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('invoices_id_seq'::regclass)")),
            Col("order_id", "bigint", 0, None),
            Col("invoice_no", "text", 0, Some("('INV-'::text || nextval('invoice_no_seq'::regclass))")),
            Col("issued_on", "date", 0, Some("CURRENT_DATE")),
            Col("due_on", "date", 0, None),
            Col("amount_due", "numeric(12,2)", 0, None),
            Col("status", "text", 0, Some("'open'::text")),
        ],
        indexes: &[
            Idx("invoices_invoice_no_key", &["invoice_no"], true, "btree"),
            Idx("invoices_order_id_idx", &["order_id"], false, "btree"),
        ],
        checks: &[],
        fks: &[Fk("invoices_order_id_fkey", &["order_id"], "orders", &["id"], "RESTRICT")],
        triggers: &[],
    },
    Table {
        schema: "public",
        name: "payments",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('payments_id_seq'::regclass)")),
            Col("invoice_id", "bigint", 0, None),
            Col("method", "text", 0, None),
            Col("amount", "numeric(12,2)", 0, None),
            Col("paid_at", "timestamptz", 0, Some("now()")),
            Col("provider_ref", "text", NULLABLE, None),
        ],
        indexes: &[Idx("payments_invoice_id_idx", &["invoice_id"], false, "btree")],
        checks: &[("payments_amount_check", "amount", "amount > 0")],
        fks: &[Fk("payments_invoice_id_fkey", &["invoice_id"], "invoices", &["id"], "RESTRICT")],
        triggers: &[Trg("payments_audit", "AFTER", &["INSERT"], true)],
    },
    Table {
        schema: "public",
        name: "shipments",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('shipments_id_seq'::regclass)")),
            Col("order_id", "bigint", 0, None),
            Col("carrier", "text", 0, None),
            Col("tracking_number", "text", NULLABLE, None),
            Col("shipped_at", "timestamptz", NULLABLE, None),
            Col("delivered_at", "timestamptz", NULLABLE, None),
            Col("status", "text", 0, Some("'label_created'::text")),
        ],
        indexes: &[
            Idx("shipments_order_id_idx", &["order_id"], false, "btree"),
            Idx("shipments_tracking_number_key", &["tracking_number"], true, "btree"),
        ],
        checks: &[],
        fks: &[Fk("shipments_order_id_fkey", &["order_id"], "orders", &["id"], "CASCADE")],
        triggers: &[Trg("shipments_touch", "BEFORE", &["UPDATE"], false)],
    },
    Table {
        schema: "public",
        name: "reviews",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('reviews_id_seq'::regclass)")),
            Col("product_id", "bigint", 0, None),
            Col("customer_id", "bigint", 0, None),
            Col("rating", "smallint", 0, None),
            Col("title", "text", NULLABLE, None),
            Col("body", "text", NULLABLE, None),
            Col("created_at", "timestamptz", 0, Some("now()")),
        ],
        indexes: &[
            Idx("reviews_product_id_customer_id_key", &["product_id", "customer_id"], true, "btree"),
            Idx("reviews_created_at_idx", &["created_at"], false, "btree"),
        ],
        checks: &[("reviews_rating_check", "rating", "rating BETWEEN 1 AND 5")],
        fks: &[
            Fk("reviews_product_id_fkey", &["product_id"], "products", &["id"], "CASCADE"),
            Fk("reviews_customer_id_fkey", &["customer_id"], "customers", &["id"], "CASCADE"),
        ],
        triggers: &[],
    },
    Table {
        schema: "public",
        name: "inventory",
        columns: &[
            Col("product_id", "bigint", PRIMARY, None),
            Col("warehouse", "text", PRIMARY, None),
            Col("on_hand", "integer", 0, Some("0")),
            Col("reserved", "integer", 0, Some("0")),
            Col("reorder_level", "integer", 0, Some("10")),
            Col("updated_at", "timestamptz", 0, Some("now()")),
        ],
        indexes: &[Idx("inventory_warehouse_idx", &["warehouse"], false, "btree")],
        checks: &[("inventory_on_hand_check", "on_hand", "on_hand >= 0")],
        fks: &[Fk("inventory_product_id_fkey", &["product_id"], "products", &["id"], "CASCADE")],
        triggers: &[TOUCH],
    },
    Table {
        schema: "public",
        name: "coupons",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('coupons_id_seq'::regclass)")),
            Col("code", "text", 0, None),
            Col("percent_off", "integer", 0, None),
            Col("valid_from", "date", 0, None),
            Col("valid_until", "date", 0, None),
            Col("max_redemptions", "integer", NULLABLE, None),
        ],
        indexes: &[Idx("coupons_code_key", &["code"], true, "btree")],
        checks: &[("coupons_percent_off_check", "percent_off", "percent_off BETWEEN 1 AND 100")],
        fks: &[],
        triggers: &[],
    },
    Table {
        schema: "analytics",
        name: "daily_sales",
        columns: &[
            Col("day", "date", PRIMARY, None),
            Col("orders", "integer", 0, Some("0")),
            Col("units", "integer", 0, Some("0")),
            Col("revenue", "numeric(14,2)", 0, Some("0")),
        ],
        indexes: &[],
        checks: &[],
        fks: &[],
        triggers: &[],
    },
    Table {
        schema: "analytics",
        name: "page_views",
        columns: &[
            Col("id", "bigint", PRIMARY, Some("nextval('page_views_id_seq'::regclass)")),
            Col("session_id", "uuid", 0, None),
            Col("path", "text", 0, None),
            Col("referrer", "text", NULLABLE, None),
            Col("viewed_at", "timestamptz", 0, Some("now()")),
        ],
        indexes: &[
            Idx("page_views_session_id_idx", &["session_id"], false, "hash"),
            Idx("page_views_viewed_at_idx", &["viewed_at"], false, "brin"),
        ],
        checks: &[],
        fks: &[],
        triggers: &[],
    },
];

const VIEW_COLUMNS: &[(&str, &[&str])] = &[
    ("active_customers", &["id", "name", "email", "country", "orders_90d", "revenue_90d"]),
    ("monthly_revenue", &["month", "orders", "units", "revenue", "avg_order_value"]),
];

fn table(schema: &str, name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|table| table.schema == schema && table.name == name)
}

/// Databases of a showcase connection, by kind.
pub fn databases(kind: crate::connections::ConnectionKind) -> Vec<&'static str> {
    use crate::connections::ConnectionKind::*;
    match kind {
        Postgres => vec![DATABASE],
        Clickhouse => vec!["default", "analytics"],
        _ => vec!["main"],
    }
}

/// Objects of a group: the shop's tables and views, a few for the other SQL connections.
/// `None`: the group is not table-like (the caller serves the schema objects).
pub fn objects(
    connection: &str,
    database: &str,
    schema: &str,
    group: ObjectGroup,
) -> Option<Vec<LoadedChild>> {
    let make = |names: &[&str], kind| {
        names.iter().map(|name| navigator::object_child(connection, database, schema, name, kind)).collect()
    };
    let postgres = connection == crate::ui_fixture::POSTGRES_ID;
    Some(match group {
        ObjectGroup::Tables if postgres => {
            let names: Vec<&str> = TABLES.iter().filter(|table| table.schema == schema).map(|table| table.name).collect();
            // The browser lists them alphabetically, as the driver does.
            let mut names = names;
            names.sort_unstable();
            make(&names, KindOfObject::Table)
        }
        ObjectGroup::Views if postgres => {
            if schema == "public" {
                make(&["active_customers", "monthly_revenue"], KindOfObject::View)
            } else {
                Vec::new()
            }
        }
        ObjectGroup::Tables if connection == crate::ui_fixture::CLICKHOUSE_ID => {
            make(&["events", "orders_daily", "page_views", "sessions"], KindOfObject::Table)
        }
        ObjectGroup::Tables => make(&["bookmarks", "notes", "settings"], KindOfObject::Table),
        ObjectGroup::Views | ObjectGroup::MaterializedViews => Vec::new(),
        _ => return None,
    })
}

fn column_names(schema: &str, object: &str) -> Vec<&'static str> {
    if let Some(table) = table(schema, object) {
        return table.columns.iter().map(|column| column.0).collect();
    }
    match VIEW_COLUMNS.iter().find(|(name, _)| *name == object) {
        Some((_, columns)) => columns.to_vec(),
        None => vec!["id", "name", "created_at"],
    }
}

/// The column nodes of the tree for `object`.
pub fn columns(connection: &str, database: &str, schema: &str, object: &str) -> Vec<LoadedChild> {
    column_names(schema, object)
        .into_iter()
        .map(|column| navigator::column_child(connection, database, schema, object, column))
        .collect()
}

/// The columns a table's data tab shows (types, keys, defaults) for the structure panel.
pub fn catalog_columns(schema: &str, object: &str) -> Vec<CatalogColumn> {
    match table(schema, object) {
        Some(table) => table.columns.iter().map(catalog_column).collect(),
        None => column_names(schema, object)
            .into_iter()
            .map(|name| CatalogColumn {
                name: name.into(),
                data_type: if name == "id" { "bigint" } else { "text" }.into(),
                nullable: name != "id",
                primary_key: name == "id",
                ..CatalogColumn::default()
            })
            .collect(),
    }
}

fn catalog_column(column: &Col) -> CatalogColumn {
    CatalogColumn {
        name: column.0.into(),
        data_type: column.1.into(),
        nullable: column.2 & NULLABLE != 0,
        primary_key: column.2 & PRIMARY != 0,
        is_generated: Some(false),
        default_expression: column.3.map(str::to_owned),
        auto_increment: Some(column.3.is_some_and(|default| default.starts_with("nextval"))),
        ..CatalogColumn::default()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// What `describe_object` answers for a table of the shop: its keys, indexes, foreign keys (and
/// the ones that point at it), triggers and DDL, derived from the compact table above.
pub fn table_properties(connection: &str, schema: &str, object: &str) -> Result<TableProperties, String> {
    if connection != crate::ui_fixture::POSTGRES_ID {
        // The other SQL connections only list a few plain tables.
        return Ok(TableProperties {
            columns: catalog_columns(schema, object),
            keys: Some(Vec::new()),
            foreign_keys: Some(Vec::new()),
            indexes: Some(Vec::new()),
            references: None,
            triggers: Some(Vec::new()),
            ddl: None,
        });
    }
    let Some(def) = table(schema, object) else {
        return Err(format!("relation \"{schema}.{object}\" does not exist"));
    };
    let qualified = format!("{schema}.{object}");
    let primary: Vec<&str> = def.columns.iter().filter(|column| column.2 & PRIMARY != 0).map(|column| column.0).collect();
    let pkey = format!("{object}_pkey");
    let mut keys = vec![CatalogConstraint {
        name: pkey.clone(),
        kind: CatalogConstraintKind::PrimaryKey,
        columns: strings(&primary),
        definition: Some(format!("PRIMARY KEY ({})", primary.join(", "))),
    }];
    keys.extend(def.indexes.iter().filter(|index| index.2).map(|index| CatalogConstraint {
        name: index.0.into(),
        kind: CatalogConstraintKind::Unique,
        columns: strings(index.1),
        definition: Some(format!("UNIQUE ({})", index.1.join(", "))),
    }));
    keys.extend(def.checks.iter().map(|(name, column, expression)| CatalogConstraint {
        name: (*name).into(),
        kind: CatalogConstraintKind::Check,
        columns: strings(&[*column]),
        definition: Some(format!("CHECK ({expression})")),
    }));

    let index = |name: &str, columns: &[&str], unique: bool, primary: bool, method: &str| CatalogIndex {
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
    };
    let mut indexes = vec![index(&pkey, &primary, true, true, "btree")];
    indexes.extend(def.indexes.iter().map(|idx| index(idx.0, idx.1, idx.2, false, idx.3)));

    let foreign_key = |owner: &str, fk: &Fk| CatalogForeignKey {
        name: fk.0.into(),
        table: format!("{schema}.{owner}"),
        columns: strings(fk.1),
        referenced_table: format!("{schema}.{}", fk.2),
        referenced_columns: strings(fk.3),
        on_update: Some("NO ACTION".into()),
        on_delete: Some(fk.4.into()),
        definition: Some(format!(
            "FOREIGN KEY ({}) REFERENCES {schema}.{}({}) ON DELETE {}",
            fk.1.join(", "),
            fk.2,
            fk.3.join(", "),
            fk.4
        )),
    };
    let references = TABLES
        .iter()
        .filter(|other| other.schema == schema)
        .flat_map(|other| other.fks.iter().filter(|fk| fk.2 == object).map(|fk| foreign_key(other.name, fk)))
        .collect();
    let triggers = def
        .triggers
        .iter()
        .map(|trigger| CatalogTrigger {
            name: trigger.0.into(),
            timing: Some(trigger.1.into()),
            events: strings(trigger.2),
            enabled: Some(trigger.3),
            definition: Some(format!(
                "CREATE TRIGGER {} {} {} ON {qualified} FOR EACH ROW EXECUTE FUNCTION {schema}.touch_updated_at()",
                trigger.0,
                trigger.1,
                trigger.2.join(" OR ")
            )),
        })
        .collect();

    let mut lines: Vec<String> = def
        .columns
        .iter()
        .map(|column| {
            let mut line = format!("    {} {}", column.0, column.1);
            if column.2 & NULLABLE == 0 {
                line.push_str(" NOT NULL");
            }
            if let Some(default) = column.3 {
                line.push_str(&format!(" DEFAULT {default}"));
            }
            line
        })
        .collect();
    lines.push(format!("    CONSTRAINT {pkey} PRIMARY KEY ({})", primary.join(", ")));
    lines.extend(def.indexes.iter().filter(|idx| idx.2).map(|idx| format!("    CONSTRAINT {} UNIQUE ({})", idx.0, idx.1.join(", "))));
    lines.extend(def.checks.iter().map(|(name, _, expression)| format!("    CONSTRAINT {name} CHECK ({expression})")));
    lines.extend(def.fks.iter().map(|fk| {
        format!("    CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {schema}.{}({}) ON DELETE {}", fk.0, fk.1.join(", "), fk.2, fk.3.join(", "), fk.4)
    }));
    Ok(TableProperties {
        columns: def.columns.iter().map(catalog_column).collect(),
        keys: Some(keys),
        foreign_keys: Some(def.fks.iter().map(|fk| foreign_key(object, fk)).collect()),
        indexes: Some(indexes),
        references: Some(references),
        triggers: Some(triggers),
        ddl: Some(format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"))),
    })
}

// ---- rows --------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Text,
    Money,
    Date,
    Timestamp,
    Bool,
}

impl Kind {
    fn of(data_type: &str) -> Self {
        match data_type {
            "bigint" | "integer" | "smallint" => Self::Int,
            "date" => Self::Date,
            "timestamptz" => Self::Timestamp,
            "boolean" => Self::Bool,
            other if other.starts_with("numeric") => Self::Money,
            _ => Self::Text,
        }
    }

    fn arrow(self) -> DataType {
        match self {
            Self::Int => DataType::Int64,
            Self::Text => DataType::Utf8,
            Self::Money => DataType::Decimal128(12, 2),
            Self::Date => DataType::Date32,
            Self::Timestamp => DataType::Timestamp(TimeUnit::Microsecond, None),
            Self::Bool => DataType::Boolean,
        }
    }
}

#[derive(Clone)]
enum Cell {
    Int(i64),
    Text(String),
    /// Hundredths.
    Money(i128),
    /// Days since 1970-01-01.
    Date(i32),
    /// Seconds since the epoch.
    Timestamp(i64),
    Bool(bool),
}

fn text(value: &str) -> Cell {
    Cell::Text(value.to_owned())
}

/// An Arrow batch of `rows` (cells in column order) under `columns` `(name, kind)`.
fn batch(columns: &[(&str, Kind)], rows: &[Vec<Cell>]) -> Result<RecordBatch, String> {
    let schema = Arc::new(Schema::new(
        columns.iter().map(|(name, kind)| Field::new(*name, kind.arrow(), true)).collect::<Vec<_>>(),
    ));
    let arrays: Vec<ArrayRef> = columns
        .iter()
        .enumerate()
        .map(|(at, (_, kind))| -> ArrayRef {
            let cells = rows.iter().map(|row| &row[at]);
            match kind {
                Kind::Int => Arc::new(Int64Array::from_iter(cells.map(|cell| match cell {
                    Cell::Int(value) => Some(*value),
                    _ => None,
                }))),
                Kind::Text => Arc::new(StringArray::from_iter(cells.map(|cell| match cell {
                    Cell::Text(value) => Some(value.as_str()),
                    _ => None,
                }))),
                Kind::Money => Arc::new(
                    Decimal128Array::from_iter(cells.map(|cell| match cell {
                        Cell::Money(value) => Some(*value),
                        _ => None,
                    }))
                    .with_precision_and_scale(12, 2)
                    .expect("12,2 is a valid decimal"),
                ),
                Kind::Date => Arc::new(Date32Array::from_iter(cells.map(|cell| match cell {
                    Cell::Date(value) => Some(*value),
                    _ => None,
                }))),
                Kind::Timestamp => Arc::new(TimestampMicrosecondArray::from_iter(cells.map(|cell| match cell {
                    Cell::Timestamp(seconds) => Some(seconds * 1_000_000),
                    _ => None,
                }))),
                Kind::Bool => Arc::new(BooleanArray::from_iter(cells.map(|cell| match cell {
                    Cell::Bool(value) => Some(*value),
                    _ => None,
                }))),
            }
        })
        .collect();
    RecordBatch::try_new(schema, arrays).map_err(|error| error.to_string())
}

/// A small deterministic generator: the same rows on every run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// 2026-01-01 as days since the epoch, and as seconds.
const DAY_2026: i32 = 20_454;
const SECONDS_PER_DAY: i64 = 86_400;

const FIRST_NAMES: [&str; 20] = [
    "Alice", "Ben", "Carla", "Dmitri", "Elena", "Farid", "Grace", "Hugo", "Isla", "Jonas", "Kira", "Leon", "Mia", "Noah",
    "Olivia", "Pablo", "Quinn", "Rosa", "Sven", "Tara",
];
const LAST_NAMES: [&str; 20] = [
    "Anders", "Brooks", "Chen", "Duarte", "Eriksen", "Fischer", "Garcia", "Hoang", "Ivanov", "Jensen", "Kowalski",
    "Laurent", "Moreau", "Novak", "Okafor", "Petrov", "Rossi", "Silva", "Tanaka", "Weber",
];
const COUNTRIES: [&str; 10] = [
    "United States", "Germany", "France", "Japan", "Brazil", "Canada", "Australia", "United Kingdom", "Spain", "Netherlands",
];

struct Customer {
    id: i64,
    name: String,
    email: String,
    country: &'static str,
    status: &'static str,
    opt_in: bool,
    created: i64,
    lifetime: i128,
}

/// The `index`-th customer (0-based); the first 60 are distinct people.
fn customer(index: usize) -> Customer {
    let mut random = Lcg(index as u64 * 7919 + 17);
    let first = FIRST_NAMES[index % 20];
    let last = LAST_NAMES[(index * 7 + index / 20) % 20];
    let status = match random.below(10) {
        0 => "new",
        1 => "inactive",
        2 | 3 => "vip",
        _ => "active",
    };
    Customer {
        id: index as i64 + 1,
        name: format!("{first} {last}"),
        email: format!("{}.{}@example.com", first.to_lowercase(), last.to_lowercase()),
        country: COUNTRIES[random.below(10) as usize],
        status,
        opt_in: random.below(3) != 0,
        created: i64::from(DAY_2026 - 700 + random.below(690) as i32) * SECONDS_PER_DAY + random.below(86_400) as i64,
        lifetime: 12_000 + i128::from(random.below(480_000)),
    }
}

/// Rows of a table's data tab; only the customers have any (the others open empty).
fn table_rows(object: &str) -> Vec<Vec<Cell>> {
    if object != "customers" {
        return Vec::new();
    }
    (0..60)
        .map(|index| {
            let customer = customer(index);
            vec![
                Cell::Int(customer.id),
                Cell::Text(customer.name),
                Cell::Text(customer.email),
                text(customer.country),
                text(customer.status),
                Cell::Bool(customer.opt_in),
                Cell::Timestamp(customer.created),
                Cell::Money(customer.lifetime),
            ]
        })
        .collect()
}

/// One page of a table's data: `(rows, total)`.
pub fn table_page(schema: &str, object: &str, offset: u64, limit: i64) -> Result<(Vec<RecordBatch>, i64), String> {
    let rows = table_rows(object);
    let total = rows.len() as i64;
    let start = usize::try_from(offset).unwrap_or(usize::MAX).min(rows.len());
    let end = start.saturating_add(limit.max(1) as usize).min(rows.len());
    if start == end {
        return Ok((Vec::new(), total));
    }
    let columns: Vec<(&str, Kind)> = table(schema, object)
        .map(|table| table.columns.iter().map(|column| (column.0, Kind::of(column.1))).collect())
        .unwrap_or_default();
    Ok((vec![batch(&columns, &rows[start..end])?], total))
}

/// The query of the showcase's SQL tab.
#[cfg(any(test, feature = "ui-scenarios"))]
pub const TOP_CUSTOMERS_SQL: &str = "\
SELECT c.name                           AS customer,
       c.email,
       c.country,
       COUNT(DISTINCT o.id)             AS orders,
       SUM(oi.quantity * oi.unit_price) AS revenue,
       MAX(o.placed_at)::date           AS last_order,
       c.status
FROM customers c
JOIN orders o       ON o.customer_id = c.id
JOIN order_items oi ON oi.order_id   = o.id
WHERE o.placed_at >= DATE '2026-01-01'
  AND o.status <> 'cancelled'
GROUP BY c.id, c.name, c.email, c.country, c.status
ORDER BY revenue DESC
LIMIT 40;";

/// The result of [`TOP_CUSTOMERS_SQL`]: 40 customers, biggest revenue first.
pub fn top_customers() -> Result<RecordBatch, String> {
    let mut random = Lcg(2026);
    let mut rows: Vec<(i128, Vec<Cell>)> = (0..40)
        .map(|index| {
            let customer = customer(index * 3 / 2);
            let orders = 3 + random.below(22) as i64;
            let revenue = orders as i128 * (6_500 + i128::from(random.below(14_000)));
            let last_order = DAY_2026 + 270 + random.below(52) as i32;
            (
                revenue,
                vec![
                    Cell::Text(customer.name),
                    Cell::Text(customer.email),
                    text(customer.country),
                    Cell::Int(orders),
                    Cell::Money(revenue),
                    Cell::Date(last_order),
                    text(customer.status),
                ],
            )
        })
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.0));
    let rows: Vec<Vec<Cell>> = rows.into_iter().map(|(_, row)| row).collect();
    batch(
        &[
            ("customer", Kind::Text),
            ("email", Kind::Text),
            ("country", Kind::Text),
            ("orders", Kind::Int),
            ("revenue", Kind::Money),
            ("last_order", Kind::Date),
            ("status", Kind::Text),
        ],
        &rows,
    )
}

// ---- MongoDB -----------------------------------------------------------------------------------

pub const MONGO_DATABASES: [&str; 2] = ["shop", "analytics"];
pub const MONGO_COLLECTIONS: [&str; 6] = ["carts", "customers", "orders", "products", "reviews", "sessions"];

const CATALOG: [(&str, &str, f64); 12] = [
    ("TS-CLASSIC-M", "Classic Tee (M)", 24.0),
    ("HD-FLEECE-L", "Fleece Hoodie (L)", 64.0),
    ("CP-CANVAS", "Canvas Cap", 19.5),
    ("BT-STEEL-750", "Steel Bottle 750 ml", 28.0),
    ("NB-DOT-A5", "Dot Notebook A5", 12.5),
    ("BG-ROLL-20", "Roll-top Backpack 20 L", 89.0),
    ("MG-CERAMIC", "Ceramic Mug", 14.0),
    ("SK-WOOL-2P", "Wool Socks (2 pack)", 16.0),
    ("LP-DESK-LED", "LED Desk Lamp", 47.0),
    ("KB-MECH-75", "Mechanical Keyboard 75%", 119.0),
    ("MS-WIRELESS", "Wireless Mouse", 39.0),
    ("HP-OVEREAR", "Over-ear Headphones", 149.0),
];

/// The `orders` collection of the MongoDB `shop` database: 40 orders, newest first.
pub fn mongo_orders() -> Vec<Document> {
    let statuses = ["paid", "shipped", "delivered", "delivered", "pending", "refunded"];
    let carriers = ["DHL Express", "UPS Ground", "FedEx Home", "Royal Mail"];
    (0..40)
        .map(|index| {
            let mut random = Lcg(index as u64 * 104_729 + 5);
            let customer = customer(index * 3 % 60);
            let lines = 1 + random.below(3) as usize;
            let mut total = 0.0;
            let items: Vec<Bson> = (0..lines)
                .map(|_| {
                    let (sku, name, price) = CATALOG[random.below(12) as usize];
                    let quantity = 1 + random.below(3) as i32;
                    total += price * f64::from(quantity);
                    Bson::Document(bson::doc! { "sku": sku, "name": name, "qty": quantity, "price": price })
                })
                .collect();
            let placed = (i64::from(DAY_2026) + 320 - index as i64) * SECONDS_PER_DAY + random.below(86_000) as i64;
            let status = statuses[random.below(6) as usize];
            let timestamp = 0x6a00_0000_u32 + (placed as u32 & 0x00ff_ffff);
            let id = ObjectId::parse_str(format!("{timestamp:08x}{:010x}{:06x}", 0x004d_2f9a_61c3_u64 + index as u64 * 977, index + 1))
                .expect("a 24-digit hex string");
            bson::doc! {
                "_id": id,
                "number": format!("ORD-{}", 10_421 + 40 - index),
                "status": status,
                "customer": { "name": customer.name, "email": customer.email, "country": customer.country },
                "items": items,
                "total": (total * 100.0).round() / 100.0,
                "currency": "USD",
                "shipping": { "carrier": carriers[random.below(4) as usize], "express": random.below(4) == 0 },
                "placedAt": DateTime::from_millis(placed * 1000),
                "paid": status != "pending",
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_fixture::POSTGRES_ID;

    #[test]
    fn every_table_describes_itself_with_keys_indexes_and_consistent_references() {
        let names: Vec<&str> = TABLES.iter().map(|table| table.name).collect();
        for def in TABLES {
            let properties = table_properties(POSTGRES_ID, def.schema, def.name).unwrap();
            assert_eq!(properties.columns.len(), def.columns.len(), "{}", def.name);
            let keys = properties.keys.as_ref().unwrap();
            assert!(keys.iter().any(|key| key.kind == CatalogConstraintKind::PrimaryKey), "{} has a primary key", def.name);
            assert!(properties.indexes.as_ref().unwrap().iter().any(|index| index.primary), "{}", def.name);
            for fk in properties.foreign_keys.as_ref().unwrap() {
                let target = fk.referenced_table.rsplit('.').next().unwrap();
                assert!(names.contains(&target), "{} points at {target}", def.name);
            }
            let ddl = properties.ddl.as_ref().unwrap();
            assert!(ddl.starts_with(&format!("CREATE TABLE {}.{} (", def.schema, def.name)), "{ddl}");
        }
        // `orders` is pointed at by its items, invoices and shipments.
        let orders = table_properties(POSTGRES_ID, "public", "orders").unwrap();
        let mut pointing: Vec<String> = orders.references.unwrap().into_iter().map(|fk| fk.table).collect();
        pointing.sort();
        assert_eq!(pointing, ["public.invoices", "public.order_items", "public.shipments"]);
        assert!(table_properties(POSTGRES_ID, "public", "missing").is_err());
    }

    #[test]
    fn the_rows_are_made_up_and_stable() {
        let top = top_customers().unwrap();
        assert_eq!((top.num_rows(), top.num_columns()), (40, 7));
        let again = top_customers().unwrap();
        assert_eq!(top, again, "the same rows on every run");
        // Revenue runs from the biggest down.
        let revenue = top.column(4).as_any().downcast_ref::<Decimal128Array>().unwrap();
        assert!((1..revenue.len()).all(|at| revenue.value(at - 1) >= revenue.value(at)));
        let (batches, total) = table_page("public", "customers", 0, 25).unwrap();
        assert_eq!((batches[0].num_rows(), total), (25, 60));
        let (second, _) = table_page("public", "customers", 50, 25).unwrap();
        assert_eq!(second[0].num_rows(), 10);
        assert!(table_page("public", "orders", 0, 25).unwrap().0.is_empty(), "the other tables open empty");
        assert_eq!(mongo_orders().len(), 40);
    }

    #[test]
    fn nothing_in_the_workspace_data_says_fixture_or_names_a_real_host() {
        let mut everything = String::new();
        for def in TABLES {
            everything.push_str(&format!("{:?}", table_properties(POSTGRES_ID, def.schema, def.name).unwrap()));
        }
        everything.push_str(TOP_CUSTOMERS_SQL);
        everything.push_str(&format!("{:?}{:?}", top_customers().unwrap(), mongo_orders()));
        for (_, columns) in VIEW_COLUMNS {
            everything.push_str(&columns.join(" "));
        }
        let lower = everything.to_lowercase();
        assert!(!lower.contains("fixture"));
        // Every address is on a reserved domain.
        for word in everything.split(|c: char| !(c.is_alphanumeric() || "@._-".contains(c))) {
            if let Some((_, domain)) = word.split_once('@') {
                assert_eq!(domain, "example.com", "{word}");
            }
        }
    }
}
