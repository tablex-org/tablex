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

/// A JSON document whose keys are not known beforehand (a MongoDB filter, sort or stage). It has
/// no `properties`, so it is outside the strict tool use subset: a tool that takes one is never
/// sent as a strict tool.
fn document(description: &str) -> Value {
    json!({"type": "object", "description": description})
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

fn documents_output() -> Value {
    object(
        vec![
            (
                "documents",
                array(
                    document("One document as relaxed Extended JSON. A document too large to show is replaced by {\"$truncated\": \"the start of its text\"}."),
                    "The documents, in the order the server returned them. Strings over the cell limit end with a [truncated, N bytes in total] marker.",
                ),
            ),
            ("document_count", integer("Documents returned.")),
            ("truncated", boolean("More documents exist than were returned.")),
            (
                "truncation",
                string_enum(&["row_limit", "byte_limit"], "Why the result was cut."),
            ),
            ("cells_truncated", integer("Strings that were cut, and documents too large to show.")),
            ("limit", integer("The document limit that applied.")),
            ("elapsed_ms", integer("Time the query took.")),
        ],
        &["documents", "document_count", "truncated", "cells_truncated", "limit", "elapsed_ms"],
    )
}

fn mongo_write_output() -> Value {
    object(
        vec![
            ("n", integer("Documents inserted, matched (update) or deleted.")),
            ("n_modified", integer("Documents an update changed.")),
            (
                "upserted",
                array(
                    document("One inserted document: {\"index\": 0, \"_id\": ...}."),
                    "Documents an update inserted (upsert).",
                ),
            ),
            (
                "write_errors",
                array(
                    document("One refused write: {\"index\": 0, \"code\": 11000, \"errmsg\": \"...\"}."),
                    "Writes the server refused; the rest of the command may have been applied.",
                ),
            ),
            ("elapsed_ms", integer("Time the command took.")),
        ],
        &["n", "elapsed_ms"],
    )
}

/// What the three connection tools answer.
fn connection_change_output() -> Value {
    object(
        vec![
            ("connection_id", string("The new connection's id, or the removed one's.")),
            ("agent_access", string_enum(&ACCESS_LEVELS, "What you may do on the new connection.")),
            ("password_saved", boolean("A password was saved for the new connection.")),
            ("app_notified", boolean("The running tablex was told to reload its connections (false when it is not running).")),
            ("warning", string("Something that went wrong after the change was made.")),
        ],
        &["connection_id", "app_notified"],
    )
}

const MONGO_JSON: &str = "JSON as MongoDB takes it; typed values use Extended JSON, e.g. \
    {\"$oid\": \"...\"}, {\"$date\": \"2024-01-31T00:00:00Z\"}, {\"$numberLong\": \"123\"}.";

/// Every tool, in a stable order, with the transaction tools, `execute_query`'s `transaction_id`,
/// the tools that show things in the app and the ones that manage the saved connections.
pub fn tool_specs() -> Vec<ToolSpec> {
    tool_specs_for(true, true, true)
}

/// The tools a layer offers: the transaction tools (and `execute_query`'s `transaction_id`)
/// only when `transactions` is true, `open_table` and `execute_and_show` only when `app` is, and
/// `add_connection`, `duplicate_connection` and `remove_connection` only when `admin` is.
pub fn tool_specs_for(transactions: bool, app: bool, admin: bool) -> Vec<ToolSpec> {
    ToolName::ALL
        .into_iter()
        .filter(|tool| transactions || !tool.is_transaction_tool())
        .filter(|tool| app || !tool.is_app_tool())
        .filter(|tool| admin || !tool.is_admin_tool())
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
        200) and a total size limit are returned; check `truncated`. Adding structure (CREATE TABLE, INDEX, \
        VIEW, SEQUENCE, SCHEMA, TYPE; ALTER TABLE ... ADD COLUMN, CONSTRAINT or INDEX; COMMENT ON) is an \
        ordinary change, approved like an INSERT. Dangerous statements (DROP, TRUNCATE, ALTER TABLE ... \
        DROP, ALTER or MODIFY COLUMN, RENAME, DELETE or UPDATE without WHERE, ...) are refused unless \
        `dangerous` is true, which puts them to the user with the danger shown; only do that when the \
        user asked for it. The MCP server does that on a connection that is not of a production type; on \
        a production one it refuses them and the user has to run the statement themselves in the tablex app."
    };
}

macro_rules! execute_batch_text {
    () => {
        "Run several SQL statements in order on one connection, in one call. Needs access `read` or \
        more; the script is split into statements and each is judged like execute_query: if one \
        changes data the connection needs `read_write` and the user is asked ONCE to approve all the \
        changes of the script together, before anything runs. A script with a statement that is not \
        allowed (a dangerous one, which a script never runs: DROP, TRUNCATE, ALTER ... DROP, DELETE or \
        UPDATE without WHERE ...; adding structure such as CREATE TABLE or ALTER TABLE ... ADD COLUMN is an \
        ordinary change; a blocked function; BEGIN, COMMIT, SET ...) is refused as a whole before anything runs, and so is one \
        the user cannot review. A refusal or a decline is final for this request: do not retry it or \
        reword it to get around it, tell the user instead. At most 50 statements. The result has one \
        entry per statement that ran (rows, affected rows or the error, with the limits of \
        execute_query and a total size limit for all rows); it stops at the first statement that \
        fails unless `continue_on_error` is true, and `skipped` counts the ones not run. Statements \
        that ran before a failure stay applied."
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
                    ("language", string_enum(&["sql", "redis", "mongo", "other"], "Statement language: `sql` runs with execute_query, `redis` with execute_redis_command, `mongo` with mongo_find, mongo_aggregate and mongo_count; `other` is browse only.")),
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
        ToolName::ExecuteBatch => {
            let mut properties = vec![
                connection.clone(),
                ("script", string("The statements, separated by `;`.")),
                database("Database to run in."),
                ("row_limit", integer("Rows to return per statement (default 200, at most 1000).")),
                ("continue_on_error", boolean("Go on with the next statement after one fails (default: stop at the first failure).")),
            ];
            if transactions {
                properties.push((
                    "use_transaction",
                    boolean("Run the whole script in a transaction of its own: committed when every statement succeeds, rolled back when one fails. Not with `continue_on_error` or `transaction_id`."),
                ));
                properties.push((
                    "transaction_id",
                    string("Run the script inside this open transaction (from begin_transaction) instead of committing by itself; omit `database` then. Not with `use_transaction` or `continue_on_error`."),
                ));
            }
            ToolSpec {
                name: tool.as_str(),
                description: if transactions {
                    concat!(
                        execute_batch_text!(),
                        " With `use_transaction` the script is one transaction on a PostgreSQL or SQLite \
                        connection: it is committed when every statement succeeded, and when one fails \
                        everything is rolled back (`transaction` is `rolled_back`, and the failed \
                        statement is named); the user approves the changes once, with the script. With \
                        `transaction_id` it runs inside that open transaction and is not committed until \
                        commit_transaction."
                    )
                } else {
                    execute_batch_text!()
                },
                input_schema: object(properties, &["connection_id", "script"]),
                output_schema: object(
                    vec![
                        ("statement_count", integer("Statements in the script.")),
                        (
                            "statements",
                            array(
                                object(
                                    vec![
                                        ("index", integer("Position in the script, from 1.")),
                                        ("statement", string("The start of the statement.")),
                                        ("ok", boolean("The statement ran without an error.")),
                                        ("result", statement_output()),
                                        (
                                            "error",
                                            object(
                                                vec![
                                                    ("code", string("Error code.")),
                                                    ("message", string("What went wrong.")),
                                                ],
                                                &["code", "message"],
                                            ),
                                        ),
                                    ],
                                    &["index", "statement", "ok"],
                                ),
                                "One entry for each statement that ran, in order; the last is the one that failed, when one did.",
                            ),
                        ),
                        ("skipped", integer("Statements that were not run because an earlier one failed.")),
                        ("ok", boolean("Every statement ran and none failed.")),
                        (
                            "transaction",
                            string_enum(&["committed", "rolled_back"], "How the transaction of `use_transaction` ended."),
                        ),
                        ("changes", integer("Write statements that took effect (none when the transaction was rolled back).")),
                        ("affected_rows", integer("Rows changed by those statements; present when `changes` is above zero.")),
                        ("elapsed_ms", integer("Time the script took.")),
                    ],
                    &["statement_count", "statements", "skipped", "ok", "changes", "elapsed_ms"],
                ),
                example: json!({"connection_id": "c1", "script": "INSERT INTO items (name) VALUES ('a'); UPDATE orders SET status = 'paid' WHERE id = 1", "continue_on_error": false}),
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
        ToolName::MongoInsert => ToolSpec {
            name: tool.as_str(),
            description: "Insert documents into a collection of a MongoDB connection. Needs access \
                `read_write` and the user's permission to change data through this front end; the user is \
                asked to approve the exact command before anything is sent. A refusal or a decline is \
                final for this request: do not retry it or reword it to get around it, tell the user \
                instead. At least 1 and at most 1000 documents, inserted in order (the first failure stops \
                the rest). `documents` are JSON documents as in mongo_find; typed values use Extended \
                JSON. Not available inside a transaction. The result has the number inserted (`n`) and \
                any `write_errors`.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("documents", array(document(MONGO_JSON), "The documents to insert (1 to 1000).")),
                ],
                &["connection_id", "database", "collection", "documents"],
            ),
            output_schema: mongo_write_output(),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "items", "documents": [{"sku": "a-1", "stock": 4}]}),
        },
        ToolName::MongoUpdate => ToolSpec {
            name: tool.as_str(),
            description: "Update documents of a collection of a MongoDB connection. Needs access \
                `read_write` and the user's permission to change data through this front end; the user is \
                asked to approve the exact command before anything is sent. A refusal or a decline is \
                final for this request: do not retry it or reword it to get around it, tell the user \
                instead. `filter` selects the documents (required; an empty filter is refused unless \
                `allow_all` is true, and the MCP server refuses it then on a production connection). Give `update` (update operators such as $set) or `pipeline` \
                (an update pipeline), not both. Only the first match is updated unless `many` is true; \
                `upsert` inserts a document when nothing matches. Not available inside a transaction. The \
                result has `n` (matched), `n_modified`, `upserted` and any `write_errors`.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("filter", document(MONGO_JSON)),
                    ("update", document("Update operators, e.g. {\"$set\": {\"status\": \"paid\"}}.")),
                    ("pipeline", array(document("One stage, e.g. {\"$set\": {\"total\": {\"$sum\": \"$lines\"}}}."), "An update pipeline, instead of `update`.")),
                    ("many", boolean("Update every matching document, not only the first (default false).")),
                    ("upsert", boolean("Insert a document when nothing matches (default false).")),
                    ("allow_all", boolean("Allow an empty `filter`, which matches every document.")),
                ],
                &["connection_id", "database", "collection", "filter"],
            ),
            output_schema: mongo_write_output(),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "orders", "filter": {"status": "open"}, "update": {"$set": {"status": "paid"}}, "many": true, "upsert": false}),
        },
        ToolName::MongoDelete => ToolSpec {
            name: tool.as_str(),
            description: "Delete documents from a collection of a MongoDB connection. Needs access \
                `read_write` and the user's permission to change data through this front end; the user is \
                asked to approve the exact command before anything is sent. A refusal or a decline is \
                final for this request: do not retry it or reword it to get around it, tell the user \
                instead. `filter` selects the documents (required; an empty filter is refused unless \
                `allow_all` is true, and the MCP server refuses it then on a production connection). Only the first match is deleted unless `many` is true. Not \
                available inside a transaction. The result has the number deleted (`n`) and any \
                `write_errors`.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("filter", document(MONGO_JSON)),
                    ("many", boolean("Delete every matching document, not only the first (default false).")),
                    ("allow_all", boolean("Allow an empty `filter`, which matches every document.")),
                ],
                &["connection_id", "database", "collection", "filter"],
            ),
            output_schema: mongo_write_output(),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "orders", "filter": {"status": "cancelled"}, "many": true}),
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
        ToolName::OpenTable => ToolSpec {
            name: tool.as_str(),
            description: "Show a table, view or MongoDB collection to the user: tablex reveals it in its \
                sidebar and opens it in a data tab, as a double click does. Nothing is read for you and \
                no data is returned: use it when the user should look at the data. Needs access \
                `schema_only` or more, and tablex has to be running; the connection is opened by tablex \
                if needed. The answer means tablex took the request up, not that the table has \
                loaded.",
            input_schema: object(
                vec![
                    connection.clone(),
                    database("Database name (for MongoDB, the database of the collection)."),
                    ("schema", string("Schema name; leave out for a database without schemas (MySQL, MariaDB, SQLite, MongoDB).")),
                    ("object", string("Table, view or collection name.")),
                ],
                &["connection_id", "database", "object"],
            ),
            output_schema: object(
                vec![
                    connection.clone(),
                    ("object", string("The table, view or collection.")),
                    ("requested", boolean("tablex took the request up; the table may still be loading.")),
                ],
                &["connection_id", "object", "requested"],
            ),
            example: json!({"connection_id": "c1", "database": "shop", "schema": "public", "object": "orders"}),
        },
        ToolName::ExecuteAndShow => ToolSpec {
            name: tool.as_str(),
            description: "Put a query in a new editor tab of tablex, where the user sees it, and run it \
                there so the user sees the result. Needs access `read` or more, and tablex has to be \
                running. A query that is run (`run` true, the default) must be a single read; it runs \
                under tablex's own rules, which may ask the user to confirm first or refuse (production \
                or read-only connections), and the user answers in tablex. A query that changes data, a \
                script with several statements or a MongoDB script is not run from here: call with \
                `run` false to only put it in the editor for the user to run, or use execute_query (a \
                change is put to the user for approval). No rows are returned: use execute_query when \
                you need the data yourself. The answer means tablex took the request up, not that the \
                query has finished.",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("query", string("The query text for the editor (SQL, a Redis command or a MongoDB script).")),
                    database("Database to open the tab on; leave out for the connection's own database."),
                    ("run", boolean("Run it now (default true); false only puts it in the editor.")),
                ],
                &["connection_id", "query"],
            ),
            output_schema: object(
                vec![
                    connection.clone(),
                    ("shown", boolean("tablex opened the tab with the query in it.")),
                    ("run", boolean("tablex was asked to run it (it may ask the user first).")),
                ],
                &["connection_id", "shown", "run"],
            ),
            example: json!({"connection_id": "c1", "query": "SELECT * FROM orders ORDER BY id DESC LIMIT 20", "database": "shop", "run": true}),
        },
        ToolName::AddConnection => ToolSpec {
            name: tool.as_str(),
            description: "Add a saved connection to tablex. The user is asked to approve every call \
                and sees the name, kind, address and connection type (never the password); a refusal \
                or a decline is final: do not retry it, tell the user. Give either `uri` or the \
                fields (`host`, `port`, `database`, `user`; SQLite: `path`, an absolute file path). \
                The `uri` must not hold a password and only takes routing and TLS options (e.g. \
                sslmode, authSource); give the password as `password`, which is saved in the \
                credential store (without it the connection has none saved and tablex asks for it \
                when connecting). SSH tunnels, proxies and certificate files cannot be set up here. \
                `connection_type` is the id or name of one of the user's connection types (default \
                development). The new connection has no access setting of its own: what you may do on \
                it is what the user's settings give its type (see `agent_access` in the answer, often \
                `off`). The name must not be in use (case-insensitive). tablex does not need to be \
                running; a running one reloads its connections. The driver for the kind need not be \
                installed yet.",
            input_schema: object(
                vec![
                    ("name", string("Display name, at most 128 characters, one line; not used by another connection.")),
                    ("kind", string_enum(&KINDS, "Database engine.")),
                    ("uri", string("Connection URI without a password, instead of the fields (e.g. postgresql://app@db.local:5432/shop).")),
                    ("host", string("Host name or address (not SQLite).")),
                    ("port", integer("Port, 1 to 65535; leave out for the engine's default.")),
                    ("database", string("Database name (Redis: the database number).")),
                    ("user", string("User name.")),
                    ("path", string("SQLite: absolute path of the database file.")),
                    ("password", string("Password to save in the credential store; never shown to the user or logged.")),
                    ("connection_type", string("Id or name of a connection type; default development.")),
                    (
                        "options",
                        object(
                            vec![(
                                "tls",
                                string_enum(
                                    &["disable", "require", "verify_full"],
                                    "TLS mode (not SQLite). MongoDB takes only disable and verify_full.",
                                ),
                            )],
                            &[],
                        ),
                    ),
                ],
                &["name", "kind"],
            ),
            output_schema: connection_change_output(),
            example: json!({"name": "local-pg", "kind": "postgresql", "host": "localhost", "port": 5432, "database": "shop", "user": "app", "password": "s3cret", "connection_type": "development", "options": {"tls": "require"}}),
        },
        ToolName::DuplicateConnection => ToolSpec {
            name: tool.as_str(),
            description: "Copy a saved connection under a new name. The user is asked to approve every \
                call; a refusal or a decline is final. The copy has the same address, options and \
                type and its saved password (when the original has one); it does not get the \
                original's own access setting, so what you may do on it follows its connection type, \
                and the call is refused when that would be more than you may do on the original. The \
                copy's SSH tunnel secrets are not copied. The name must not be in use \
                (case-insensitive).",
            input_schema: object(
                vec![
                    connection.clone(),
                    ("name", string("Display name of the copy, at most 128 characters, one line.")),
                ],
                &["connection_id", "name"],
            ),
            output_schema: connection_change_output(),
            example: json!({"connection_id": "c1", "name": "local-pg copy"}),
        },
        ToolName::RemoveConnection => ToolSpec {
            name: tool.as_str(),
            description: "Remove a saved connection from tablex, with its saved passwords and pinned \
                objects, and close your session on it. The user is asked to approve every call, and \
                the question says when the connection is a production one; a refusal or a decline is \
                final. It cannot be undone.",
            input_schema: object(vec![connection.clone()], &["connection_id"]),
            output_schema: connection_change_output(),
            example: json!({"connection_id": "c1"}),
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
        ToolName::MongoFind => ToolSpec {
            name: tool.as_str(),
            description: "Query a collection of a MongoDB connection (find). Read-only: it never changes data. \
                Needs access `read` or more. `database` and `collection` are required (list_namespaces and \
                list_tables name them). `filter`, `projection` and `sort` are JSON documents as in the MongoDB \
                shell; typed values use Extended JSON, e.g. {\"_id\": {\"$oid\": \"...\"}} or \
                {\"created\": {\"$gte\": {\"$date\": \"2024-01-31T00:00:00Z\"}}}. The key order of `sort` is \
                kept. Documents come back as relaxed Extended JSON. At most 1000 documents (default 200) and a \
                total size limit are returned; check `truncated`, and narrow the filter or the projection rather \
                than reading everything.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("filter", document(MONGO_JSON)),
                    ("projection", document("Fields to return, e.g. {\"name\": 1, \"_id\": 0}.")),
                    ("sort", document("Sort order, e.g. {\"created_at\": -1}.")),
                    ("skip", integer("Documents to skip (default 0).")),
                    ("limit", integer("Documents to return (at least 1; default 200, at most 1000).")),
                ],
                &["connection_id", "database", "collection"],
            ),
            output_schema: documents_output(),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "orders", "filter": {"status": "paid"}, "projection": {"customer": 1, "total": 1}, "sort": {"created_at": -1}, "skip": 0, "limit": 20}),
        },
        ToolName::MongoAggregate => ToolSpec {
            name: tool.as_str(),
            description: "Run an aggregation pipeline on a collection of a MongoDB connection. Read-only: a \
                pipeline with a $out or $merge stage is refused, and nothing is ever written. Needs access \
                `read` or more. `pipeline` is the stages in order, as JSON documents (typed values use \
                Extended JSON, as in mongo_find). A final $limit is added, so at most 1000 documents (default \
                200) and a total size limit are returned; check `truncated`.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("pipeline", array(document("One stage, e.g. {\"$match\": {\"status\": \"paid\"}}."), "The pipeline stages, in order.")),
                    ("limit", integer("Documents to return (at least 1; default 200, at most 1000).")),
                ],
                &["connection_id", "database", "collection", "pipeline"],
            ),
            output_schema: documents_output(),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "orders", "pipeline": [{"$match": {"status": "paid"}}, {"$group": {"_id": "$customer", "total": {"$sum": "$total"}}}, {"$sort": {"total": -1}}], "limit": 10}),
        },
        ToolName::MongoCount => ToolSpec {
            name: tool.as_str(),
            description: "Count the documents of a collection of a MongoDB connection that match `filter` (all of \
                them when it is omitted). Read-only. Needs access `read` or more. The filter is a JSON \
                document, as in mongo_find.",
            input_schema: object(
                vec![
                    connection,
                    database("Database name."),
                    ("collection", string("Collection name.")),
                    ("filter", document(MONGO_JSON)),
                ],
                &["connection_id", "database", "collection"],
            ),
            output_schema: object(
                vec![
                    ("count", integer("Documents that match.")),
                    ("elapsed_ms", integer("Time the query took.")),
                ],
                &["count", "elapsed_ms"],
            ),
            example: json!({"connection_id": "c3", "database": "shop", "collection": "orders", "filter": {"status": "paid"}}),
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
                    // A MongoDB document has no fixed keys: only the MongoDB tools take or return
                    // one, and they are never sent as strict tools (see `strict_tools`).
                    let free_form = node["type"] == "object"
                        && node.get("properties").is_none()
                        && spec.name.starts_with("mongo_");
                    if free_form {
                        assert!(node.get("additionalProperties").is_none(), "{} {label} {path}", spec.name);
                    } else if node["type"] == "object" {
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
                "execute_batch",
                "explain_query",
                "execute_redis_command",
                "mongo_find",
                "mongo_aggregate",
                "mongo_count",
                "mongo_insert",
                "mongo_update",
                "mongo_delete",
                "list_routines",
                "get_routine_source",
                "begin_transaction",
                "commit_transaction",
                "rollback_transaction",
                "open_table",
                "execute_and_show",
                "add_connection",
                "duplicate_connection",
                "remove_connection",
            ]
        );
        // Off by default: neither the tools nor `transaction_id` exist for the model.
        let plain = tool_specs_for(false, false, false);
        assert_eq!(plain.len(), 19);
        assert_eq!(tool_specs_for(true, false, false).len(), 22);
        let with_app: Vec<_> = tool_specs_for(false, true, false).iter().map(|spec| spec.name).collect();
        assert_eq!(with_app.len(), 21);
        assert_eq!(&with_app[19..], ["open_table", "execute_and_show"]);
        let with_admin: Vec<_> = tool_specs_for(false, false, true).iter().map(|spec| spec.name).collect();
        assert_eq!(with_admin.len(), 22);
        assert_eq!(&with_admin[19..], ["add_connection", "duplicate_connection", "remove_connection"]);
        assert_eq!(tool_specs_for(true, true, true).len(), 27);
        let execute = plain.iter().find(|spec| spec.name == "execute_query").unwrap();
        assert!(execute.input_schema["properties"].get("transaction_id").is_none());
        assert!(!execute.description.contains("transaction"));
        // The batch tool has no transaction parameters without them either.
        let batch = plain.iter().find(|spec| spec.name == "execute_batch").unwrap();
        for name in ["use_transaction", "transaction_id"] {
            assert!(batch.input_schema["properties"].get(name).is_none(), "{name}");
        }
        assert!(!batch.description.contains("transaction"));
        for name in ["use_transaction", "transaction_id"] {
            assert!(spec(ToolName::ExecuteBatch).input_schema["properties"].get(name).is_some(), "{name}");
        }
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
                ToolName::ExecuteBatch => parses::<ExecuteBatchRequest>(example),
                ToolName::ExplainQuery => parses::<ExplainQueryRequest>(example),
                ToolName::ExecuteRedisCommand => parses::<ExecuteRedisCommandRequest>(example),
                ToolName::MongoFind => parses::<MongoFindRequest>(example),
                ToolName::MongoAggregate => parses::<MongoAggregateRequest>(example),
                ToolName::MongoCount => parses::<MongoCountRequest>(example),
                ToolName::MongoInsert => parses::<MongoInsertRequest>(example),
                ToolName::MongoUpdate => parses::<MongoUpdateRequest>(example),
                ToolName::MongoDelete => parses::<MongoDeleteRequest>(example),
                ToolName::ListRoutines => parses::<ListRoutinesRequest>(example),
                ToolName::GetRoutineSource => parses::<GetRoutineSourceRequest>(example),
                ToolName::BeginTransaction => parses::<ConnectionRequest>(example),
                ToolName::CommitTransaction | ToolName::RollbackTransaction => {
                    parses::<TransactionRequest>(example)
                }
                ToolName::OpenTable => parses::<OpenTableRequest>(example),
                ToolName::ExecuteAndShow => parses::<ExecuteAndShowRequest>(example),
                ToolName::AddConnection => parses::<AddConnectionRequest>(example),
                ToolName::DuplicateConnection => parses::<DuplicateConnectionRequest>(example),
                ToolName::RemoveConnection => parses::<ConnectionRequest>(example),
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
                ToolName::ExecuteBatch,
                serde_json::to_value(ExecuteBatchRequest {
                    connection_id: "c".into(),
                    script: "s".into(),
                    database: Some("d".into()),
                    row_limit: Some(1),
                    continue_on_error: Some(false),
                    use_transaction: Some(false),
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
            (
                ToolName::MongoFind,
                serde_json::to_value(MongoFindRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    filter: Some(Map::new()),
                    projection: Some(Map::new()),
                    sort: Some(Map::new()),
                    skip: Some(1),
                    limit: Some(1),
                })
                .unwrap(),
            ),
            (
                ToolName::MongoAggregate,
                serde_json::to_value(MongoAggregateRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    pipeline: vec![Map::new()],
                    limit: Some(1),
                })
                .unwrap(),
            ),
            (
                ToolName::MongoCount,
                serde_json::to_value(MongoCountRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    filter: Some(Map::new()),
                })
                .unwrap(),
            ),
            (
                ToolName::MongoInsert,
                serde_json::to_value(MongoInsertRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "items".into(),
                    documents: vec![Map::new()],
                })
                .unwrap(),
            ),
            (
                ToolName::MongoUpdate,
                serde_json::to_value(MongoUpdateRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    filter: Map::new(),
                    update: Some(Map::new()),
                    pipeline: Some(vec![Map::new()]),
                    many: Some(true),
                    upsert: Some(true),
                    allow_all: Some(true),
                })
                .unwrap(),
            ),
            (
                ToolName::MongoDelete,
                serde_json::to_value(MongoDeleteRequest {
                    connection_id: "c".into(),
                    database: "shop".into(),
                    collection: "orders".into(),
                    filter: Map::new(),
                    many: Some(true),
                    allow_all: Some(true),
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
