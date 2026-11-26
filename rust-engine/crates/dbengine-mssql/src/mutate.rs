//! Mutations, using the shared SQL driver contract (`engine_driver_api::sql`): row changes by key
//! inside a transaction, and `create_database`.
//!
//! | kind              | database | object | identity                              | payload |
//! |-------------------|----------|--------|---------------------------------------|---------|
//! | `insert`          | database | table  | `SqlTableIdentity` (`schema`, default `dbo`) | one `SqlRowChange` with `op: "insert"` |
//! | `update`          | database | table  | same                                  | one `SqlRowChange`, `op: "update"`: `key` + changed `values` |
//! | `delete`          | database | table  | same                                  | one `SqlRowChange`, `op: "delete"`: `key` |
//! | `batch`           | database | table  | same                                  | `SqlBatch`: ordered changes, one transaction |
//! | `create_database` | name     | -      | ignored                               | absent or `{}` |
//!
//! A mutation is its own transaction (`BEGIN TRANSACTION` .. `COMMIT`); `batch` is
//! all-or-nothing. When the session is inside a transaction the *user* opened
//! (`@@TRANCOUNT > 0`) it joins it through `SAVE TRANSACTION` instead and rolls back to that
//! savepoint on failure, never committing or rolling back the user's work. If a rollback itself
//! fails the connection is closed: the session may hold half a change.
//!
//! Each change reports its row count from `@@ROWCOUNT` (so a session with `SET NOCOUNT ON` is
//! no different); zero rows is `mutation.no_rows_affected`, more than one
//! `mutation.identity_not_unique`, and both roll everything back. A `null` key value matches
//! SQL NULL (`IS NULL`).
//!
//! Values are JSON scalars bound as statement parameters (never spliced into SQL): booleans are
//! `bit`, integers `bigint`, strings `nvarchar` (the server converts them to the column type, so
//! DECIMAL and temporal values travel as strings; write dates as `YYYY-MM-DDThh:mm:ss`, the form
//! that no `SET LANGUAGE` reads differently), other numbers their decimal text, arrays and
//! objects JSON text. The columns' types are read first because binary columns take
//! `0x` + hex digits (bound as the real bytes, anything else is `mutation.invalid_value`) and CLR
//! columns (`geometry`, `geography`, `hierarchyid`) cannot be written through text
//! (`mutation.value_not_editable`).

use crate::catalog::{self, resolve_database, ColumnInfo};
use crate::conn::{
    block_on, cancellable, map_error, use_database, Area, Client, MsConnection, Stopped,
};
use crate::sql::{push_ident, push_qualified};
use engine_driver_kit::api::sql::{codes, SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
use engine_driver_kit::api::MutationRequest;
use engine_driver_kit::{BackendError, MutationOutcome, OperationGuard};
use serde_json::Value as Json;
use std::fmt::Write;
use tiberius::Query;

const SAVEPOINT: &str = "tablex_mutation";

fn bad_request(message: impl Into<String>) -> BackendError {
    BackendError::with_code(codes::MUTATION_BAD_REQUEST, message)
}

fn invalid_value(column: &str, message: impl std::fmt::Display) -> BackendError {
    BackendError::with_code(codes::MUTATION_INVALID_VALUE, format!("column `{column}`: {message}"))
}

/// One bound parameter.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Param {
    Bool(bool),
    Int(i64),
    Text(String),
    Bytes(Vec<u8>),
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X"))?;
    if digits.len() % 2 != 0 || !digits.is_ascii() {
        return None;
    }
    digits
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some((high * 16 + low) as u8)
        })
        .collect()
}

/// The parameter for `value` written to `column` (`None` when the table's columns are unknown).
fn to_param(value: &Json, name: &str, column: Option<&ColumnInfo>) -> Result<Param, BackendError> {
    let base = column.map_or("", |column| column.base_type.as_str());
    if matches!(base, "geometry" | "geography" | "hierarchyid") {
        return Err(BackendError::with_code(
            codes::MUTATION_VALUE_NOT_EDITABLE,
            format!("column `{name}` ({base}) cannot be written through text"),
        ));
    }
    if matches!(base, "binary" | "varbinary" | "image") {
        let Json::String(text) = value else {
            return Err(invalid_value(name, "a binary column takes `0x` followed by hex digits"));
        };
        return parse_hex(text)
            .map(Param::Bytes)
            .ok_or_else(|| invalid_value(name, "a binary column takes `0x` followed by an even number of hex digits"));
    }
    Ok(match value {
        Json::Bool(flag) => Param::Bool(*flag),
        Json::Number(number) => match number.as_i64() {
            Some(int) => Param::Int(int),
            // Decimal text, so a DECIMAL column receives the digits the caller wrote.
            None => Param::Text(number.to_string()),
        },
        Json::String(text) => Param::Text(text.clone()),
        other => Param::Text(other.to_string()),
    })
}

fn bind(query: &mut Query<'_>, params: Vec<Param>) {
    for param in params {
        match param {
            Param::Bool(value) => query.bind(value),
            Param::Int(value) => query.bind(value),
            Param::Text(value) => query.bind(value),
            Param::Bytes(value) => query.bind(value),
        }
    }
}

/// One statement of a mutation: its text, parameters and what it must touch.
#[derive(Debug, PartialEq)]
pub(crate) struct Statement {
    pub sql: String,
    pub params: Vec<Param>,
    pub op: SqlRowOp,
}

fn find<'a>(columns: &'a [ColumnInfo], name: &str) -> Result<Option<&'a ColumnInfo>, BackendError> {
    if columns.is_empty() {
        return Ok(None);
    }
    columns
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
        .map(Some)
        .ok_or_else(|| bad_request(format!("the table has no column `{name}`")))
}

/// The `WHERE` of a keyed change; a `null` key value is `IS NULL`.
fn push_where(
    sql: &mut String,
    params: &mut Vec<Param>,
    key: &serde_json::Map<String, Json>,
    columns: &[ColumnInfo],
) -> Result<(), BackendError> {
    sql.push_str(" WHERE ");
    for (index, (name, value)) in key.iter().enumerate() {
        if index > 0 {
            sql.push_str(" AND ");
        }
        let column = find(columns, name)?;
        push_ident(sql, name);
        if value.is_null() {
            sql.push_str(" IS NULL");
        } else {
            params.push(to_param(value, name, column)?);
            let _ = write!(sql, " = @P{}", params.len());
        }
    }
    Ok(())
}

pub(crate) fn statement(
    change: &SqlRowChange,
    table: &str,
    columns: &[ColumnInfo],
) -> Result<Statement, BackendError> {
    let mut sql = String::new();
    let mut params = Vec::new();
    match change.op {
        SqlRowOp::Insert => {
            sql.push_str("INSERT INTO ");
            sql.push_str(table);
            if change.values.is_empty() {
                sql.push_str(" DEFAULT VALUES");
            } else {
                sql.push_str(" (");
                let mut values = String::new();
                for (index, (name, value)) in change.values.iter().enumerate() {
                    let column = find(columns, name)?;
                    if index > 0 {
                        sql.push_str(", ");
                        values.push_str(", ");
                    }
                    push_ident(&mut sql, name);
                    if value.is_null() {
                        values.push_str("NULL");
                    } else {
                        params.push(to_param(value, name, column)?);
                        let _ = write!(values, "@P{}", params.len());
                    }
                }
                let _ = write!(sql, ") VALUES ({values})");
            }
        }
        SqlRowOp::Update => {
            if change.key.is_empty() {
                return Err(bad_request("an update needs the row's key"));
            }
            if change.values.is_empty() {
                return Err(bad_request("an update needs at least one changed value"));
            }
            sql.push_str("UPDATE ");
            sql.push_str(table);
            sql.push_str(" SET ");
            for (index, (name, value)) in change.values.iter().enumerate() {
                let column = find(columns, name)?;
                if index > 0 {
                    sql.push_str(", ");
                }
                push_ident(&mut sql, name);
                if value.is_null() {
                    sql.push_str(" = NULL");
                } else {
                    params.push(to_param(value, name, column)?);
                    let _ = write!(sql, " = @P{}", params.len());
                }
            }
            push_where(&mut sql, &mut params, &change.key, columns)?;
        }
        SqlRowOp::Delete => {
            if change.key.is_empty() {
                return Err(bad_request("a delete needs the row's key"));
            }
            sql.push_str("DELETE FROM ");
            sql.push_str(table);
            push_where(&mut sql, &mut params, &change.key, columns)?;
        }
    }
    Ok(Statement { sql, params, op: change.op })
}

/// How one change's row count is judged: `None` when it is as the contract wants.
fn refusal(op: SqlRowOp, affected: u64) -> Option<(&'static str, &'static str)> {
    match (op, affected) {
        (_, 0) => Some((
            codes::MUTATION_NO_ROWS_AFFECTED,
            "the change affected no row (the row is gone, or the key matches none)",
        )),
        (SqlRowOp::Update | SqlRowOp::Delete, 2..) => Some((
            codes::MUTATION_IDENTITY_NOT_UNIQUE,
            "the key matches more than one row; nothing was changed",
        )),
        _ => None,
    }
}

/// Where the mutation's transaction stands, kept outside the cancellable future so a cancel can
/// still undo it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tx {
    None,
    /// The mutation opened a transaction.
    Own,
    /// The user's transaction is open; a savepoint was set.
    Joined,
}

pub(crate) fn mutate(
    connection: &MsConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<MutationOutcome, BackendError> {
    if request.kind == "create_database" {
        return create_database(connection, request, operation);
    }
    let table = request
        .object
        .as_deref()
        .filter(|table| !table.is_empty())
        .ok_or_else(|| bad_request("a row change needs the table (`object`)"))?
        .to_owned();
    let schema = if request.identity.is_null() {
        None
    } else {
        serde_json::from_value::<SqlTableIdentity>(request.identity.clone())
            .map_err(|error| bad_request(format!("`identity` must be {{\"schema\": ..}}: {error}")))?
            .schema
    }
    .filter(|schema| !schema.is_empty())
    .unwrap_or_else(|| "dbo".to_owned());
    let changes: Vec<SqlRowChange> = match request.kind.as_str() {
        "insert" | "update" | "delete" => {
            let change: SqlRowChange = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("payload is not a row change: {error}")))?;
            if change.op.kind() != request.kind {
                return Err(bad_request(format!(
                    "the change is a `{}` but the request kind is `{}`",
                    change.op.kind(),
                    request.kind
                )));
            }
            vec![change]
        }
        "batch" => {
            let batch: SqlBatch = serde_json::from_value(request.payload.clone())
                .map_err(|error| bad_request(format!("payload is not a batch: {error}")))?;
            if batch.changes.is_empty() {
                return Err(bad_request("a batch needs at least one change"));
            }
            batch.changes
        }
        other => {
            return Err(BackendError::with_code(
                codes::MUTATION_UNSUPPORTED,
                format!("unknown mutation kind `{other}`"),
            ))
        }
    };

    let guard = connection.lock(Area::Mutation)?;
    let mut active = operation.activate(guard)?;
    let mut client = active
        .client
        .take()
        .ok_or_else(|| BackendError::with_code(Area::Mutation.not_connected(), "connection closed"))?;
    let mut current_db = active.current_db.take();
    let configured = connection.configured_database().map(str::to_owned);
    let mut tx = Tx::None;
    let mut commit_sent = false;
    let result = block_on(cancellable(
        &mut client,
        connection.shared(),
        &|| active.cancel_requested(),
        async |client: &mut Client| {
            run(
                client,
                &mut current_db,
                configured.as_deref(),
                request.database.as_deref(),
                &schema,
                &table,
                &changes,
                &mut tx,
                &mut commit_sent,
            )
            .await
        },
    ));
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(Stopped::Lost(why)) => {
            active.mark_broken();
            return Err(Stopped::Lost(why).error(Area::Mutation));
        }
        Err(Stopped::Cancelled) => {
            // The attention stopped the statement; our transaction is still open.
            let undone = block_on(undo(&mut client, tx));
            return Err(finish_failure(
                &mut active,
                client,
                current_db,
                undone,
                Stopped::Cancelled.error(Area::Mutation),
            ));
        }
    };
    match outcome {
        Ok(affected) => {
            active.client = Some(client);
            active.current_db = current_db;
            Ok(MutationOutcome::new(affected))
        }
        Err(error) if error.code.as_deref() == Some(Area::Mutation.not_connected()) => {
            // The connection is gone; whether a COMMIT got through is unknown.
            active.mark_broken();
            if commit_sent {
                return Err(BackendError::with_code(
                    codes::MUTATION_COMMIT_UNKNOWN,
                    "the connection was lost while committing; the change may or may not have been applied",
                ));
            }
            Err(error)
        }
        Err(error) => {
            let undone = block_on(undo(&mut client, tx));
            Err(finish_failure(&mut active, client, current_db, undone, error))
        }
    }
}

/// Puts the session back after a failed mutation, or closes it when the rollback failed.
fn finish_failure<G>(
    active: &mut engine_driver_kit::ActiveOperation<'_, G>,
    client: Client,
    current_db: Option<String>,
    undone: bool,
    error: BackendError,
) -> BackendError
where
    G: std::ops::DerefMut<Target = crate::conn::ConnState>,
{
    if undone {
        active.client = Some(client);
        active.current_db = current_db;
        error
    } else {
        // The session may still hold half a change: do not reuse it.
        active.mark_broken();
        BackendError::with_code(
            Area::Mutation.not_connected(),
            format!("{}; the rollback failed, so the connection was closed", error.message),
        )
    }
}

/// Rolls back what the mutation started; `false` when that failed.
async fn undo(client: &mut Client, tx: Tx) -> bool {
    let statement = match tx {
        Tx::None => return true,
        // A trigger that ended the transaction itself leaves nothing to roll back.
        Tx::Own => "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION".to_owned(),
        Tx::Joined => format!("ROLLBACK TRANSACTION {SAVEPOINT}"),
    };
    matches!(
        tokio::time::timeout(crate::conn::CANCEL_GRACE, control(client, &statement)).await,
        Ok(Ok(()))
    )
}

async fn control(client: &mut Client, statement: &str) -> Result<(), tiberius::error::Error> {
    client.simple_query(statement).await?.into_results().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run(
    client: &mut Client,
    current_db: &mut Option<String>,
    configured: Option<&str>,
    requested_db: Option<&str>,
    schema: &str,
    table: &str,
    changes: &[SqlRowChange],
    tx: &mut Tx,
    commit_sent: &mut bool,
) -> Result<u64, BackendError> {
    let area = Area::Mutation;
    use_database(client, current_db, area, requested_db).await?;
    let database = resolve_database(client, area, current_db.as_deref().or(configured), None).await?;
    let columns = catalog::columns(client, area, &database, schema, table).await?;
    if columns.is_empty() {
        return Err(BackendError::with_code(
            codes::MUTATION_FAILED,
            format!("table `{schema}.{table}` does not exist in `{database}`"),
        ));
    }
    let mut qualified = String::new();
    push_qualified(&mut qualified, None, Some(schema), table);
    let statements = changes
        .iter()
        .map(|change| statement(change, &qualified, &columns))
        .collect::<Result<Vec<_>, _>>()?;

    let in_user_transaction = {
        let rows = client
            .simple_query("SELECT CAST(@@TRANCOUNT AS int)")
            .await
            .map_err(|error| map_error(area, &error))?
            .into_first_result()
            .await
            .map_err(|error| map_error(area, &error))?;
        rows.first()
            .map(|row| catalog::int(row, 0))
            .ok_or_else(|| BackendError::with_code(area.failed(), "the server did not report the transaction state"))?
            > 0
    };
    let (begin, started) = if in_user_transaction {
        (format!("SAVE TRANSACTION {SAVEPOINT}"), Tx::Joined)
    } else {
        ("BEGIN TRANSACTION".to_owned(), Tx::Own)
    };
    control(client, &begin).await.map_err(|error| map_error(area, &error))?;
    *tx = started;

    let mut affected_total = 0_u64;
    for statement in statements {
        // `SET NOCOUNT ON` keeps the per-statement counts off the wire; `@@ROWCOUNT` is
        // the one count read, whatever the session's own NOCOUNT setting is.
        let text = format!(
            "SET NOCOUNT ON; {}; SELECT CAST(@@ROWCOUNT AS bigint)",
            statement.sql
        );
        let mut query = Query::new(text);
        bind(&mut query, statement.params);
        let rows = query
            .query(client)
            .await
            .map_err(|error| map_error(area, &error))?
            .into_first_result()
            .await
            .map_err(|error| map_error(area, &error))?;
        let affected = rows
            .first()
            .map_or(0, |row| u64::try_from(catalog::int(row, 0)).unwrap_or(0));
        if let Some((code, message)) = refusal(statement.op, affected) {
            return Err(BackendError::with_code(code, message));
        }
        affected_total += affected;
    }
    if started == Tx::Own {
        *commit_sent = true;
        control(client, "COMMIT TRANSACTION")
            .await
            .map_err(|error| map_error(area, &error))?;
        *commit_sent = false;
        *tx = Tx::None;
    }
    Ok(affected_total)
}

fn create_database(
    connection: &MsConnection,
    request: &MutationRequest,
    operation: &mut OperationGuard<'_>,
) -> Result<MutationOutcome, BackendError> {
    let name = request
        .database
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| bad_request("create_database needs the new database's name"))?;
    let empty = match &request.payload {
        Json::Null => true,
        Json::Object(map) => map.is_empty(),
        _ => false,
    };
    if !empty {
        return Err(bad_request("create_database takes no options"));
    }
    let mut statement = String::from("CREATE DATABASE ");
    push_ident(&mut statement, name);
    let guard = connection.lock(Area::Mutation)?;
    let mut active = operation.activate(guard)?;
    let mut client = active
        .client
        .take()
        .ok_or_else(|| BackendError::with_code(Area::Mutation.not_connected(), "connection closed"))?;
    let result = block_on(cancellable(
        &mut client,
        connection.shared(),
        &|| active.cancel_requested(),
        async |client: &mut Client| control(client, &statement).await,
    ));
    match result {
        Ok(Ok(())) => {
            active.client = Some(client);
            Ok(MutationOutcome::new(0).keep_pages())
        }
        Ok(Err(error)) => {
            let lost = crate::conn::is_connection_lost(&error);
            if lost {
                active.mark_broken();
                return Err(map_error(Area::Mutation, &error));
            }
            active.client = Some(client);
            Err(BackendError::with_code(
                codes::CREATE_DATABASE_REJECTED,
                match &error {
                    tiberius::error::Error::Server(token) => crate::conn::server_message(token),
                    other => other.to_string(),
                },
            ))
        }
        Err(Stopped::Cancelled) => {
            active.client = Some(client);
            Err(Stopped::Cancelled.error(Area::Mutation))
        }
        Err(lost) => {
            active.mark_broken();
            Err(lost.error(Area::Mutation))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn column(name: &str, base: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            data_type: base.into(),
            base_type: base.into(),
            nullable: true,
            primary_key: false,
            identity: false,
            computed: false,
            default: None,
            comment: None,
        }
    }

    fn change(value: serde_json::Value) -> SqlRowChange {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn an_insert_binds_values_and_writes_null_as_a_literal() {
        let columns = [column("a", "int"), column("b", "nvarchar")];
        let built = statement(
            &change(json!({"op": "insert", "values": {"a": 1, "b": null}})),
            "[dbo].[t]",
            &columns,
        )
        .unwrap();
        assert_eq!(built.sql, "INSERT INTO [dbo].[t] ([a], [b]) VALUES (@P1, NULL)");
        assert_eq!(built.params, [Param::Int(1)]);
        let empty = statement(&change(json!({"op": "insert"})), "[dbo].[t]", &columns).unwrap();
        assert_eq!(empty.sql, "INSERT INTO [dbo].[t] DEFAULT VALUES");
    }

    #[test]
    fn an_update_keys_on_every_column_and_a_null_key_is_is_null() {
        let columns = [column("tenant", "int"), column("id", "int"), column("name", "nvarchar")];
        let built = statement(
            &change(json!({"op": "update", "key": {"tenant": 1, "id": null}, "values": {"name": "x"}})),
            "[dbo].[items]",
            &columns,
        )
        .unwrap();
        // The key's order is the JSON map's (sorted, or insertion order when another crate of the build
        // turns on `preserve_order`); the placeholders follow the SQL either way.
        assert!(built.sql.starts_with("UPDATE [dbo].[items] SET [name] = @P1 WHERE "), "{}", built.sql);
        assert!(built.sql.contains("[id] IS NULL") && built.sql.contains("[tenant] = @P2"), "{}", built.sql);
        assert_eq!(built.params, [Param::Text("x".into()), Param::Int(1)]);
        let delete = statement(&change(json!({"op": "delete", "key": {"id": 7}})), "[dbo].[items]", &columns).unwrap();
        assert_eq!(delete.sql, "DELETE FROM [dbo].[items] WHERE [id] = @P1");
    }

    #[test]
    fn malformed_changes_are_refused_before_anything_runs() {
        let columns = [column("a", "int")];
        for (value, expect) in [
            (json!({"op": "update", "values": {"a": 1}}), "key"),
            (json!({"op": "update", "key": {"a": 1}}), "changed value"),
            (json!({"op": "delete"}), "key"),
            (json!({"op": "delete", "key": {"nope": 1}}), "no column"),
        ] {
            let error = statement(&change(value), "[t]", &columns).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.bad_request"));
            assert!(error.message.contains(expect), "{}", error.message);
        }
    }

    #[test]
    fn values_become_the_parameter_their_column_needs() {
        let int = column("n", "int");
        assert_eq!(to_param(&json!(true), "n", Some(&int)).unwrap(), Param::Bool(true));
        assert_eq!(to_param(&json!(-5), "n", Some(&int)).unwrap(), Param::Int(-5));
        assert_eq!(to_param(&json!(1.25), "n", Some(&int)).unwrap(), Param::Text("1.25".into()));
        assert_eq!(to_param(&json!([1, 2]), "n", Some(&int)).unwrap(), Param::Text("[1,2]".into()));
        assert_eq!(to_param(&json!({"k": 1}), "n", None).unwrap(), Param::Text("{\"k\":1}".into()));
        let binary = column("b", "varbinary");
        assert_eq!(to_param(&json!("0x00ff"), "b", Some(&binary)).unwrap(), Param::Bytes(vec![0, 255]));
        assert_eq!(to_param(&json!("0x"), "b", Some(&binary)).unwrap(), Param::Bytes(vec![]));
        for bad in [json!("00ff"), json!("0xf"), json!("0xzz"), json!(5)] {
            let error = to_param(&bad, "b", Some(&binary)).unwrap_err();
            assert_eq!(error.code.as_deref(), Some("mutation.invalid_value"), "{bad}");
        }
        let geography = column("g", "geography");
        let error = to_param(&json!("POINT(1 1)"), "g", Some(&geography)).unwrap_err();
        assert_eq!(error.code.as_deref(), Some("mutation.value_not_editable"));
    }

    #[test]
    fn the_row_count_is_judged_by_the_contract() {
        assert_eq!(refusal(SqlRowOp::Update, 1), None);
        assert_eq!(refusal(SqlRowOp::Insert, 1), None);
        assert_eq!(refusal(SqlRowOp::Insert, 0).unwrap().0, "mutation.no_rows_affected");
        assert_eq!(refusal(SqlRowOp::Delete, 0).unwrap().0, "mutation.no_rows_affected");
        assert_eq!(refusal(SqlRowOp::Update, 2).unwrap().0, "mutation.identity_not_unique");
    }
}
