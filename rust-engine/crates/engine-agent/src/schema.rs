//! Tool descriptions and JSON Schemas.
//!
//! The schemas are written by hand (no schema generator is in the dependency
//! tree) in the subset that strict tool use accepts: `type`, `properties`,
//! `required`, `additionalProperties: false`, `items`, `enum`, `description`.
//! No `$ref`, no recursion, no `minimum` / `maximum` / `pattern` (limits are
//! stated in the descriptions and enforced by the tool layer). Optional
//! request fields are simply not listed in `required`.
//!
//! [`validate_request`] checks a request against that subset; the tool layer
//! runs every request through it before parsing, and front ends may do the
//! same for input that skipped server-side validation. [`validate`] is the
//! strict form for outputs: a `null` is only accepted where the schema's
//! type allows one.

use crate::tools::ToolName;
use serde_json::{json, Map, Value};

/// One tool as a model sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Value,
    pub output_schema: Value,
    /// A valid request (documentation and tests; not sent to the model).
    pub example: Value,
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn string_enum(values: &[&str], description: &str) -> Value {
    json!({"type": "string", "enum": values, "description": description})
}

fn array(items: Value, description: &str) -> Value {
    json!({"type": "array", "items": items, "description": description})
}

fn object(properties: Vec<(&str, Value)>, required: &[&str]) -> Value {
    let properties: Map<String, Value> = properties
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect();
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

const CONNECTION_ID: &str = "Connection id from list_connections.";
const ACCESS_LEVELS: [&str; 4] = ["off", "schema_only", "read", "read_write"];
const ROUTINE_KINDS: [&str; 4] = ["function", "procedure", "trigger", "event"];
const KINDS: [&str; 7] = [
    "postgresql",
    "mysql",
    "mariadb",
    "sqlite",
    "clickhouse",
    "mongodb",
    "redis",
];

fn name_item() -> Value {
    object(
        vec![
            ("name", string("Object name.")),
            (
                "kind",
                string("database, schema, table, view, materialized_view, ..."),
            ),
        ],
        &["name", "kind"],
    )
}

/// The reference `list_routines` returns and `get_routine_source` takes back as `ref`.
fn routine_ref() -> Value {
    object(
        vec![
            ("database", string("Database name.")),
            ("schema", string("Schema name.")),
            ("kind", string_enum(&ROUTINE_KINDS, "Kind of object.")),
            ("name", string("Object name.")),
            ("arguments", string("Input argument types of a PostgreSQL function or procedure.")),
            ("table", string("The table of a trigger.")),
            ("identity", string("Opaque key the database uses to find the object again.")),
        ],
        &["kind", "name"],
    )
}

fn column_result() -> Value {
    object(
        vec![
            ("name", string("Column name.")),
            ("type", string("Column type as the database reports it.")),
            ("nullable", boolean("The column accepts NULL.")),
            ("primary_key", boolean("Part of the primary key.")),
            ("default", string("Default expression.")),
            ("comment", string("Column comment.")),
        ],
        &["name", "type", "nullable", "primary_key"],
    )
}

fn names(description: &str) -> Value {
    array(json!({"type": "string"}), description)
}

fn statement_output() -> Value {
    object(
        vec![
            (
                "columns",
                array(
                    object(
                        vec![
                            ("name", string("Column name.")),
                            ("type", string("Column type.")),
                        ],
                        &["name", "type"],
                    ),
                    "Result columns.",
                ),
            ),
            (
                "rows",
                array(
                    array(json!({"type": "string"}), "One row of cell texts (NULL for SQL NULL)."),
                    "Result rows. Cells over the cell limit end with a [truncated, N bytes in total] marker.",
                ),
            ),
            ("row_count", integer("Rows returned.")),
            ("truncated", boolean("More rows exist than were returned.")),
            (
                "truncation",
                string_enum(&["row_limit", "byte_limit"], "Why the result was cut."),
            ),
            ("cells_truncated", integer("Cells that were cut.")),
            ("affected_rows", integer("Rows changed by a write.")),
            ("row_limit", integer("The row limit that applied.")),
            (
                "read_only_enforced",
                boolean("The database ran the statement read-only, so a hidden write would have failed. False for writes, Redis and drivers without that support."),
            ),
            ("elapsed_ms", integer("Time the statement took.")),
        ],
        &["columns", "rows", "row_count", "truncated", "cells_truncated", "row_limit", "read_only_enforced", "elapsed_ms"],
    )
}

/// Every tool, in a stable order, with the transaction tools and `execute_query`'s
/// `transaction_id`.
pub fn tool_specs() -> Vec<ToolSpec> {
    tool_specs_for(true)
}

/// The tools a layer offers: the transaction tools (and `execute_query`'s `transaction_id`)
/// only when `transactions` is true.
pub fn tool_specs_for(transactions: bool) -> Vec<ToolSpec> {
    ToolName::ALL
        .into_iter()
        .filter(|tool| transactions || !tool.is_transaction_tool())
        .map(|tool| spec_for(tool, transactions))
        .collect()
}

/// The spec with every optional part (`transaction_id`).
pub fn spec(tool: ToolName) -> ToolSpec {
    spec_for(tool, true)
}

macro_rules! execute_query_text {
    () => {
        "Run exactly one SQL statement. Needs access `read` or more; a statement that changes data \
        needs `read_write` and may be put to the user for approval when the connection allows it. \
        A refusal or a decline is final for this request: do not retry the statement or reword it to get \
        around it, tell the user instead. Multi-statement scripts are refused. At most 1000 rows (default \
        200) and a total size limit are returned; check `truncated`. Dangerous statements (DROP, TRUNCATE, \
        DELETE or UPDATE without WHERE, ...) are refused; a front end that can ask the user about them \
        says to pass `dangerous` true, one that cannot (the MCP server) says so and the user has to run \
        the statement themselves."
    };
}

fn spec_for(tool: ToolName, transactions: bool) -> ToolSpec {
    let connection = ("connection_id", string(CONNECTION_ID));
    let database = |text: &str| ("database", string(text));
    match tool {
        ToolName::ListConnections => ToolSpec {
            name: tool.as_str(),
            description: "List the saved connections with their type and the access you have on each. \
                A connection with access `off` is listed by name only and cannot be opened or used \
                until the user enables agent access for it.",
            input_schema: object(vec![], &[]),
            output_schema: object(
                vec![(
                    "connections",
                    array(
                        object(
                            vec![
                                ("id", string("Connection id.")),
                                ("name", string("Display name.")),
                                ("kind", string_enum(&KINDS, "Database engine.")),
                                ("connection_type", string("Connection type name, e.g. Development or Production.")),
                                ("read_only", boolean("The connection type refuses writes.")),
                                ("production", boolean("A production connection.")),
                                ("access", string_enum(&ACCESS_LEVELS, "What you may do now.")),
                                ("configured_access", string_enum(&ACCESS_LEVELS, "The connection's own setting.")),
                                ("open", boolean("A session is open.")),
                            ],
                            &["id", "name", "access", "open"],
                        ),
                        "Saved connections.",
                    ),
                )],
                &["connections"],
            ),
            example: json!({}),
        },
        ToolName::OpenSession => ToolSpec {
            name: tool.as_str(),
            description: "Connect to a saved connection. Other tools need an open session. \
                Needs agent access `schema_only` or more.",
            input_schema: object(vec![connection.clone()], &["connection_id"]),
            output_schema: object(
                vec![
                    connection.clone(),
                    ("kind", string_enum(&KINDS, "Database engine.")),
                    ("access", string_enum(&ACCESS_LEVELS, "What you may do now.")),
                    ("language", string_enum(&["sql", "redis", "other"], "Statement language; `other` is browse only.")),
                    ("supports_schemas", boolean("Tables live in schemas inside databases.")),
                    ("already_open", boolean("The session was already open.")),
                    ("statement_timeout_secs", integer("Statements are cancelled after this many seconds.")),
                ],
                &["connection_id", "kind", "access", "language", "supports_schemas", "already_open", "statement_timeout_secs"],
            ),
            example: json!({"connection_id": "c1"}),
        },
        ToolName::CloseSession => ToolSpec {
            name: tool.as_str(),
            description: "Close the session of a connection.",
            input_schema: object(vec![connection.clone()], &["connection_id"]),
            output_schema: object(
                vec![connection.clone(), ("closed", boolean("A session was open and is now closed."))],
                &["connection_id", "closed"],
            ),
            example: json!({"connection_id": "c1"}),
        },
        ToolName::ListNamespaces => ToolSpec {
            name: tool.as_str(),
            description: "List databases (no `database` given) or the schemas of a database. \
                Engines without schemas return an empty list for a database.",
            input_schema: object(
                vec![
                    connection.clone(),
                    database("List the schemas of this database; omit to list databases."),
                    ("cursor", string("`next_cursor` of the previous page.")),
                    ("limit", integer("Items per page (default 100, at most 500).")),
                ],
                &["connection_id"],
            ),
            output_schema: object(
                vec![
                    ("namespaces", array(name_item(), "Databases or schemas.")),
                    ("next_cursor", string("Pass as `cursor` for the next page.")),
                ],
                &["namespaces"],
            ),
            example: json!({"connection_id": "c1", "limit": 50}),
        },
        ToolName::ListTables => ToolSpec {
            name: tool.as_str(),
            description: "List the tables and views of a namespace, sorted by name. `database` may be \
                omitted when the connection has a single database; `schema` is needed for engines with schemas.",
            input_schema: object(
                vec![
                    connection.clone(),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                    ("filter", string("Case-insensitive substring of the table name.")),
                    ("cursor", string("`next_cursor` of the previous page.")),
                    ("limit", integer("Items per page (default 100, at most 500).")),
                ],
                &["connection_id"],
            ),
            output_schema: object(
                vec![
                    ("tables", array(name_item(), "Tables and views.")),
                    ("next_cursor", string("Pass as `cursor` for the next page.")),
                ],
                &["tables"],
            ),
            example: json!({"connection_id": "c1", "schema": "public", "database": "app"}),
        },
        ToolName::DescribeTable => ToolSpec {
            name: tool.as_str(),
            description: "Describe a table: columns with type, nullability and comment, primary key, \
                unique columns, indexes, foreign keys and (when the database provides it) the DDL.",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("table", string("Table or view name.")),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                ],
                &["connection_id", "table"],
            ),
            output_schema: object(
                vec![
                    ("name", string("Table name.")),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                    ("columns", array(column_result(), "Columns in table order.")),
                    ("primary_key", names("Primary key columns in key order.")),
                    ("unique_columns", names("Columns that are unique on their own.")),
                    (
                        "indexes",
                        array(
                            object(
                                vec![
                                    ("name", string("Index name.")),
                                    ("columns", names("Indexed columns.")),
                                    ("unique", boolean("A unique index.")),
                                ],
                                &["name", "columns", "unique"],
                            ),
                            "Indexes.",
                        ),
                    ),
                    (
                        "foreign_keys",
                        array(
                            object(
                                vec![
                                    ("name", string("Constraint name.")),
                                    ("columns", names("Referencing columns.")),
                                    ("referenced_table", string("Referenced table.")),
                                    ("referenced_columns", names("Referenced columns.")),
                                    ("on_delete", string("ON DELETE action.")),
                                    ("on_update", string("ON UPDATE action.")),
                                ],
                                &["name", "columns", "referenced_table", "referenced_columns"],
                            ),
                            "Foreign keys.",
                        ),
                    ),
                    ("ddl", string("CREATE statement, cut to a size limit.")),
                    ("ddl_truncated", boolean("The DDL was cut.")),
                ],
                &["name", "columns", "primary_key", "unique_columns", "indexes", "foreign_keys"],
            ),
            example: json!({"connection_id": "c1", "table": "orders"}),
        },
        ToolName::GetSchemaContext => ToolSpec {
            name: tool.as_str(),
            description: "A compact digest of every table of a namespace (columns, keys, foreign keys, \
                comments) sized to `budget`. Stable between calls for the same schema. Over budget it drops \
                comments, then non-key columns of the widest tables, then tables, and says what it omitted.",
            input_schema: object(
                vec![
                    connection.clone(),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                    ("budget", integer("Size limit in tokens (estimated; default 4000).")),
                ],
                &["connection_id"],
            ),
            output_schema: object(
                vec![
                    ("text", string("The digest.")),
                    ("estimated_tokens", integer("Estimated size of the digest.")),
                    ("budget", integer("The budget that applied.")),
                    ("tables_total", integer("Tables in the namespace.")),
                    ("tables_included", integer("Tables in the digest.")),
                    ("columns_omitted", integer("Non-key columns dropped.")),
                    ("comments_dropped", boolean("Column comments were dropped.")),
                    ("omitted_tables", names("Tables not in the digest.")),
                    ("within_budget", boolean("The digest fits the budget.")),
                ],
                &[
                    "text", "estimated_tokens", "budget", "tables_total", "tables_included",
                    "columns_omitted", "comments_dropped", "omitted_tables", "within_budget",
                ],
            ),
            example: json!({"connection_id": "c1", "budget": 2000}),
        },
        ToolName::ExecuteQuery => {
            let mut properties = vec![
                connection.clone(),
                ("statement", string("One SQL statement.")),
                database("Database to run in."),
                ("row_limit", integer("Rows to return (default 200, at most 1000).")),
                ("dangerous", boolean("Ask the user to approve a dangerous statement.")),
            ];
            if transactions {
                properties.push((
                    "transaction_id",
                    string("Run in this open transaction (from begin_transaction) instead of committing by itself; omit `database` then."),
                ));
            }
            ToolSpec {
                name: tool.as_str(),
                description: if transactions {
                    concat!(
                        execute_query_text!(),
                        " With `transaction_id` the statement runs inside that transaction: a change is \
                        approved one statement at a time and is NOT committed until commit_transaction."
                    )
                } else {
                    execute_query_text!()
                },
                input_schema: object(properties, &["connection_id", "statement"]),
                output_schema: statement_output(),
                example: json!({"connection_id": "c1", "statement": "SELECT id, name FROM users ORDER BY id", "row_limit": 50}),
            }
        }
        ToolName::ExplainQuery => ToolSpec {
            name: tool.as_str(),
            description: "Show the execution plan of one SQL statement (EXPLAIN, never ANALYZE; the statement \
                is not executed). Needs access `read` or more. Pass the statement without EXPLAIN.",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("statement", string("The statement to explain, without EXPLAIN.")),
                    database("Database to run in."),
                ],
                &["connection_id", "statement"],
            ),
            output_schema: statement_output(),
            example: json!({"connection_id": "c1", "statement": "SELECT * FROM orders WHERE user_id = 1"}),
        },
        ToolName::ListRoutines => ToolSpec {
            name: tool.as_str(),
            description: "List the functions, procedures, triggers or events of a namespace, sorted by name. \
                `database` may be omitted when the connection has a single database; `schema` is needed for \
                engines with schemas. Each item has a `ref` to pass to get_routine_source. A database without \
                objects of that kind returns an empty list. SQL databases only.",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("kind", string_enum(&ROUTINE_KINDS, "Which objects to list.")),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                    ("filter", string("Case-insensitive substring of the name.")),
                    ("cursor", string("`next_cursor` of the previous page.")),
                    ("limit", integer("Items per page (default 100, at most 500).")),
                ],
                &["connection_id", "kind"],
            ),
            output_schema: object(
                vec![
                    (
                        "routines",
                        array(
                            object(
                                vec![
                                    ("name", string("Object name.")),
                                    ("kind", string_enum(&ROUTINE_KINDS, "Kind of object.")),
                                    ("arguments", string("Input argument types (PostgreSQL overloads).")),
                                    ("table", string("The table of a trigger.")),
                                    ("ref", routine_ref()),
                                ],
                                &["name", "kind", "ref"],
                            ),
                            "Functions, procedures, triggers or events.",
                        ),
                    ),
                    ("next_cursor", string("Pass as `cursor` for the next page.")),
                ],
                &["routines"],
            ),
            example: json!({"connection_id": "c1", "kind": "function", "schema": "public", "database": "app"}),
        },
        ToolName::GetRoutineSource => ToolSpec {
            name: tool.as_str(),
            description: "The CREATE statement or source text of one function, procedure, trigger or event. Pass \
                the `ref` from list_routines, or `kind` and `name` (with `database` and `schema` as for \
                list_tables, `arguments` for a PostgreSQL overload, `table` for a trigger). The source is cut to a \
                size limit. SQL databases only.",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("ref", routine_ref()),
                    ("kind", string_enum(&ROUTINE_KINDS, "Kind of object, when no `ref` is given.")),
                    ("name", string("Object name, when no `ref` is given.")),
                    database("Database name."),
                    ("schema", string("Schema name.")),
                    ("arguments", string("Input argument types of a PostgreSQL overload, e.g. `integer, text`.")),
                    ("table", string("The table of a trigger.")),
                ],
                &["connection_id"],
            ),
            output_schema: object(
                vec![
                    ("kind", string_enum(&ROUTINE_KINDS, "Kind of object.")),
                    ("name", string("Object name.")),
                    ("source", string("The CREATE statement or source text.")),
                    ("truncated", boolean("The source was cut.")),
                ],
                &["kind", "name", "source", "truncated"],
            ),
            example: json!({"connection_id": "c1", "kind": "trigger", "name": "audit_orders", "table": "orders", "schema": "public", "database": "app"}),
        },
        ToolName::BeginTransaction => ToolSpec {
            name: tool.as_str(),
            description: "Begin a transaction on a PostgreSQL or SQLite connection, on a session of its own. \
                Needs access `read_write` and the user's permission to change data through this front end. \
                Run statements in it with execute_query and `transaction_id`; nothing a write does is \
                committed until commit_transaction, and the user is asked to approve the commit with the list \
                of changes. One open transaction per connection, a few in all. A transaction is rolled back \
                after 5 minutes without a call on it, and when its connection's session is closed. \
                Always end it with commit_transaction or rollback_transaction.",
            input_schema: object(vec![connection.clone()], &["connection_id"]),
            output_schema: object(
                vec![
                    ("transaction_id", string("Pass to execute_query, commit_transaction and rollback_transaction.")),
                    connection.clone(),
                    ("idle_timeout_secs", integer("The transaction is rolled back after this many seconds without a call on it.")),
                ],
                &["transaction_id", "connection_id", "idle_timeout_secs"],
            ),
            example: json!({"connection_id": "c1"}),
        },
        ToolName::CommitTransaction => ToolSpec {
            name: tool.as_str(),
            description: "Commit an open transaction and close its session. When statements changed data the \
                user is asked to approve the commit with the list of those statements; a decline is final: \
                the transaction stays open, nothing is committed, roll it back. A transaction with no changes \
                is committed without asking. A PostgreSQL transaction that hit an error cannot be committed: \
                roll it back.",
            input_schema: object(vec![("transaction_id", string("From begin_transaction."))], &["transaction_id"]),
            output_schema: object(
                vec![
                    ("transaction_id", string("The transaction.")),
                    ("committed", boolean("The transaction was committed.")),
                    ("changes", integer("Write statements that were sent in the transaction.")),
                ],
                &["transaction_id", "committed", "changes"],
            ),
            example: json!({"transaction_id": "tx-1"}),
        },
        ToolName::RollbackTransaction => ToolSpec {
            name: tool.as_str(),
            description: "Roll back an open transaction (undoing everything it changed) and close its session. \
                No approval needed. A transaction that is already gone (rolled back after the idle timeout) \
                answers `rolled_back` false.",
            input_schema: object(vec![("transaction_id", string("From begin_transaction."))], &["transaction_id"]),
            output_schema: object(
                vec![
                    ("transaction_id", string("The transaction.")),
                    ("rolled_back", boolean("The transaction was open and is rolled back.")),
                    ("changes", integer("Write statements that were sent in the transaction, all undone.")),
                ],
                &["transaction_id", "rolled_back", "changes"],
            ),
            example: json!({"transaction_id": "tx-1"}),
        },
        ToolName::ExecuteRedisCommand => ToolSpec {
            name: tool.as_str(),
            description: "Run one Redis command line on a Redis connection. Reads need access `read`; a command \
                that changes data needs `read_write` and may be put to the user for approval when the connection \
                allows it. A refusal or a decline is final for this request: do not retry the command or reword it \
                to get around it, tell the user instead. Some commands (KEYS, CONFIG, EVAL, FLUSHALL, ...) are \
                never allowed. FLUSHDB is dangerous: see execute_query.",
            input_schema: object(
                vec![
                    connection,
                    ("command", string("One command, e.g. `GET user:1`.")),
                    database("Redis database index, e.g. 0."),
                    ("row_limit", integer("Rows to return (default 200, at most 1000).")),
                    ("dangerous", boolean("Ask the user to approve a dangerous command.")),
                ],
                &["connection_id", "command"],
            ),
            output_schema: statement_output(),
            example: json!({"connection_id": "c2", "command": "SCAN 0 MATCH user:* COUNT 100", "database": "0"}),
        },
    }
}

/// Checks `value` against `schema` (the subset above), strictly: a `null` is
/// valid only where the schema's `type` lists `null`. Use it for outputs (a
/// real client's validator treats an explicit `null` for an optional property
/// as a violation). `Err` carries a message with the path of the first
/// violation.
pub fn validate(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "$", false)
}

/// Like [`validate`], for requests: a client that writes an explicit `null`
/// for an optional property means "not given", so that is accepted.
pub fn validate_request(schema: &Value, value: &Value) -> Result<(), String> {
    check(schema, value, "$", true)
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        _ => false,
    }
}

fn check(schema: &Value, value: &Value, path: &str, null_is_absent: bool) -> Result<(), String> {
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        return if options
            .iter()
            .any(|option| check(option, value, path, null_is_absent).is_ok())
        {
            Ok(())
        } else {
            Err(format!("{path}: matches none of the allowed shapes"))
        };
    }
    match schema.get("type") {
        Some(Value::String(expected)) if !type_matches(expected, value) => {
            return Err(format!(
                "{path}: expected {expected}, got {}",
                kind_of(value)
            ));
        }
        Some(Value::Array(expected))
            if !expected
                .iter()
                .filter_map(Value::as_str)
                .any(|expected| type_matches(expected, value)) =>
        {
            return Err(format!("{path}: unexpected {}", kind_of(value)));
        }
        _ => {}
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let list: Vec<String> = allowed.iter().map(|v| v.to_string()).collect();
            return Err(format!("{path}: must be one of {}", list.join(", ")));
        }
    }
    if let (Some(fields), Some(properties)) = (
        value.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let nullable = properties
                .get(required)
                .and_then(|property| property.get("type"))
                .and_then(Value::as_array)
                .is_some_and(|types| types.iter().any(|kind| kind == "null"));
            if !fields.contains_key(required) && !nullable {
                return Err(format!("{path}: missing required field `{required}`"));
            }
        }
        let is_required = |name: &str| {
            schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|names| names.iter().any(|n| n == name))
        };
        for (name, field) in fields {
            match properties.get(name) {
                // The schema sent to a model has no null for optional properties (it would count
                // against the provider's union limit); in a request, an explicit null from a
                // client that writes one means "not given". Outputs never get this leniency.
                Some(_) if null_is_absent && field.is_null() && !is_required(name) => {}
                Some(property) => {
                    check(property, field, &format!("{path}.{name}"), null_is_absent)?;
                }
                None if schema.get("additionalProperties") == Some(&Value::Bool(false)) => {
                    return Err(format!("{path}: unknown field `{name}`"));
                }
                None => {}
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            check(
                item_schema,
                item,
                &format!("{path}[{index}]"),
                null_is_absent,
            )?;
        }
    }
    Ok(())
}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::*;
    use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef};

    const ALLOWED_KEYWORDS: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "description",
    ];

    fn walk(schema: &Value, path: &str, visit: &mut impl FnMut(&Value, &str)) {
        visit(schema, path);
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            for (name, property) in properties {
                walk(property, &format!("{path}.{name}"), visit);
            }
        }
        if let Some(items) = schema.get("items") {
            walk(items, &format!("{path}[]"), visit);
        }
    }

    #[test]
    fn schemas_stay_inside_the_strict_tool_use_subset() {
        for spec in tool_specs() {
            for (label, schema) in [
                ("input", &spec.input_schema),
                ("output", &spec.output_schema),
            ] {
                assert_eq!(schema["type"], "object", "{} {label}", spec.name);
                walk(schema, "$", &mut |node, path| {
                    let object = node
                        .as_object()
                        .unwrap_or_else(|| panic!("{} {path}", spec.name));
                    for key in object.keys() {
                        assert!(
                            ALLOWED_KEYWORDS.contains(&key.as_str()),
                            "{} {label} {path}: keyword `{key}` is outside the strict subset",
                            spec.name
                        );
                    }
                    if node["type"] == "object" {
                        assert_eq!(
                            node["additionalProperties"], false,
                            "{} {label} {path}",
                            spec.name
                        );
                        let properties = node["properties"].as_object().unwrap();
                        for required in node["required"].as_array().unwrap() {
                            assert!(
                                properties.contains_key(required.as_str().unwrap()),
                                "{} {label} {path}",
                                spec.name
                            );
                        }
                    }
                    if node["type"] == "array" {
                        assert!(node.get("items").is_some(), "{} {label} {path}", spec.name);
                    }
                });
            }
        }
    }

    #[test]
    fn tool_names_are_stable_and_unique() {
        let names: Vec<&str> = tool_specs().iter().map(|spec| spec.name).collect();
        assert_eq!(
            names,
            [
                "list_connections",
                "open_session",
                "close_session",
                "list_namespaces",
                "list_tables",
                "describe_table",
                "get_schema_context",
                "execute_query",
                "explain_query",
                "execute_redis_command",
                "list_routines",
                "get_routine_source",
                "begin_transaction",
                "commit_transaction",
                "rollback_transaction",
            ]
        );
        // Off by default: neither the tools nor `transaction_id` exist for the model.
        let plain = tool_specs_for(false);
        assert_eq!(plain.len(), 12);
        let execute = plain.iter().find(|spec| spec.name == "execute_query").unwrap();
        assert!(execute.input_schema["properties"].get("transaction_id").is_none());
        assert!(!execute.description.contains("transaction"));
        assert!(spec(ToolName::ExecuteQuery).input_schema["properties"]
            .get("transaction_id")
            .is_some());
        for tool in ToolName::ALL {
            assert_eq!(ToolName::parse(tool.as_str()), Some(tool));
        }
        assert_eq!(ToolName::parse("drop_database"), None);
    }

    fn parses<T: serde::de::DeserializeOwned>(value: Value) {
        serde_json::from_value::<T>(value).expect("the example parses into its request type");
    }

    #[test]
    fn every_schema_validates_its_own_example_and_the_request_type_parses_it() {
        for spec in tool_specs() {
            validate_request(&spec.input_schema, &spec.example)
                .unwrap_or_else(|e| panic!("{}: {e}", spec.name));
            let example = spec.example.clone();
            match ToolName::parse(spec.name).unwrap() {
                ToolName::ListConnections => parses::<ListConnectionsRequest>(example),
                ToolName::OpenSession | ToolName::CloseSession => {
                    parses::<ConnectionRequest>(example)
                }
                ToolName::ListNamespaces => parses::<ListNamespacesRequest>(example),
                ToolName::ListTables => parses::<ListTablesRequest>(example),
                ToolName::DescribeTable => parses::<DescribeTableRequest>(example),
                ToolName::GetSchemaContext => parses::<GetSchemaContextRequest>(example),
                ToolName::ExecuteQuery => parses::<ExecuteQueryRequest>(example),
                ToolName::ExplainQuery => parses::<ExplainQueryRequest>(example),
                ToolName::ExecuteRedisCommand => parses::<ExecuteRedisCommandRequest>(example),
                ToolName::ListRoutines => parses::<ListRoutinesRequest>(example),
                ToolName::GetRoutineSource => parses::<GetRoutineSourceRequest>(example),
                ToolName::BeginTransaction => parses::<ConnectionRequest>(example),
                ToolName::CommitTransaction | ToolName::RollbackTransaction => {
                    parses::<TransactionRequest>(example)
                }
            }
        }
    }

    #[test]
    fn schemas_reject_unknown_fields_missing_fields_and_wrong_types() {
        for spec in tool_specs() {
            let mut with_extra = spec.example.clone();
            with_extra["surprise"] = json!(1);
            let error = validate_request(&spec.input_schema, &with_extra).unwrap_err();
            assert!(
                error.contains("unknown field `surprise`"),
                "{}: {error}",
                spec.name
            );
            for required in spec.input_schema["required"].as_array().unwrap() {
                let name = required.as_str().unwrap();
                let mut without = spec.example.clone();
                without.as_object_mut().unwrap().remove(name);
                let error = validate_request(&spec.input_schema, &without).unwrap_err();
                assert!(
                    error.contains("missing required field"),
                    "{}: {error}",
                    spec.name
                );
            }
            // An optional property may be left out, or sent as null.
            for name in spec.input_schema["properties"].as_object().unwrap().keys() {
                let required = spec.input_schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r == name);
                if !required && spec.example.get(name).is_some() {
                    let mut without = spec.example.clone();
                    without.as_object_mut().unwrap().remove(name);
                    validate_request(&spec.input_schema, &without).unwrap();
                    // An explicit null from a client means "not given".
                    without[name] = Value::Null;
                    validate_request(&spec.input_schema, &without).unwrap();
                    // ... but the strict form (outputs) does not accept it.
                    assert!(
                        validate(&spec.input_schema, &without).is_err(),
                        "{}: an explicit null for `{name}` passes the strict form",
                        spec.name
                    );
                }
            }
            assert!(validate_request(&spec.input_schema, &json!("text")).is_err());
        }
        let execute = spec(ToolName::ExecuteQuery);
        assert!(validate_request(
            &execute.input_schema,
            &json!({"connection_id": "c", "statement": "s", "database": 5})
        )
        .is_err());
        assert!(validate_request(
            &execute.input_schema,
            &json!({"connection_id": null, "statement": "s"})
        )
        .is_err());
        assert!(validate_request(&execute.input_schema, &json!({"connection_id": "c", "statement": "s", "database": "d", "row_limit": 5, "dangerous": true})).is_ok());
        assert!(validate_request(
            &execute.input_schema,
            &json!({"connection_id": "c", "statement": 5})
        )
        .is_err());
        assert!(validate_request(
            &execute.input_schema,
            &json!({"connection_id": "c", "statement": "x", "row_limit": 1.5})
        )
        .is_err());
        assert!(validate_request(
            &execute.input_schema,
            &json!({"connection_id": "c", "statement": "x", "dangerous": "yes"})
        )
        .is_err());
    }

    #[test]
    fn request_types_agree_with_their_schemas() {
        // A fully populated request serialises to exactly the schema's properties.
        let requests: Vec<(ToolName, Value)> = vec![
            (
                ToolName::ListNamespaces,
                serde_json::to_value(ListNamespacesRequest {
                    connection_id: "c".into(),
                    database: Some("d".into()),
                    cursor: Some("1".into()),
                    limit: Some(5),
                })
                .unwrap(),
            ),
            (
                ToolName::ListTables,
                serde_json::to_value(ListTablesRequest {
                    connection_id: "c".into(),
                    database: Some("d".into()),
                    schema: Some("s".into()),
                    filter: Some("f".into()),
                    cursor: Some("1".into()),
                    limit: Some(5),
                })
                .unwrap(),
            ),
            (
                ToolName::DescribeTable,
                serde_json::to_value(DescribeTableRequest {
                    connection_id: "c".into(),
                    table: "t".into(),
                    database: Some("d".into()),
                    schema: Some("s".into()),
                })
                .unwrap(),
            ),
            (
                ToolName::GetSchemaContext,
                serde_json::to_value(GetSchemaContextRequest {
                    connection_id: "c".into(),
                    database: Some("d".into()),
                    schema: Some("s".into()),
                    budget: Some(10),
                })
                .unwrap(),
            ),
            (
                ToolName::ExecuteQuery,
                serde_json::to_value(ExecuteQueryRequest {
                    connection_id: "c".into(),
                    statement: "s".into(),
                    database: Some("d".into()),
                    row_limit: Some(1),
                    dangerous: Some(true),
                    transaction_id: Some("tx-1".into()),
                })
                .unwrap(),
            ),
            (
                ToolName::ExplainQuery,
                serde_json::to_value(ExplainQueryRequest {
                    connection_id: "c".into(),
                    statement: "s".into(),
                    database: Some("d".into()),
                })
                .unwrap(),
            ),
            (
                ToolName::ListRoutines,
                serde_json::to_value(ListRoutinesRequest {
                    connection_id: "c".into(),
                    kind: RoutineKind::Function,
                    database: Some("d".into()),
                    schema: Some("s".into()),
                    filter: Some("f".into()),
                    cursor: Some("1".into()),
                    limit: Some(5),
                })
                .unwrap(),
            ),
            (
                ToolName::GetRoutineSource,
                serde_json::to_value(GetRoutineSourceRequest {
                    connection_id: "c".into(),
                    reference: Some(SchemaObjectRef {
                        database: Some("d".into()),
                        schema: Some("s".into()),
                        kind: SchemaObjectKind::Trigger,
                        name: "n".into(),
                        arguments: Some("integer".into()),
                        table: Some("t".into()),
                        identity: Some("23".into()),
                    }),
                    kind: Some(RoutineKind::Trigger),
                    name: Some("n".into()),
                    database: Some("d".into()),
                    schema: Some("s".into()),
                    arguments: Some("integer".into()),
                    table: Some("t".into()),
                })
                .unwrap(),
            ),
            (
                ToolName::ExecuteRedisCommand,
                serde_json::to_value(ExecuteRedisCommandRequest {
                    connection_id: "c".into(),
                    command: "x".into(),
                    database: Some("0".into()),
                    row_limit: Some(1),
                    dangerous: Some(true),
                })
                .unwrap(),
            ),
        ];
        for (tool, request) in requests {
            let spec = spec(tool);
            validate_request(&spec.input_schema, &request)
                .unwrap_or_else(|e| panic!("{}: {e}", spec.name));
            let mut have: Vec<&String> = request.as_object().unwrap().keys().collect();
            let mut want: Vec<&String> = spec.input_schema["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect();
            have.sort();
            want.sort();
            assert_eq!(have, want, "{}", spec.name);
        }
    }
}
