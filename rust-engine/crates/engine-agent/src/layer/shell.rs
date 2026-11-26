//! `execute_query` on a MongoDB connection: the statement is a mongosh-style command (see
//! [`crate::mongo_shell`]). It is read into a command, judged (a read runs; a change needs
//! `read_write` and the user's yes; a dangerous one also needs `dangerous` and a front end that
//! runs them; `runCommand` and `createUser` never run), turned into the server commands it means
//! and run through the same session, limits, audit and approval as the `mongo_*` tools. Also
//! `describe_table` on a collection: its indexes.

use super::*;
use crate::mongo_shell::{self, Class, Command, FindSpec};
use crate::result::{QueryResult, ResultColumn};
use bson::{doc, Bson, Document};
use serde_json::json;

/// Indexes one collection may have, at most (the server's own limit is 64).
const MAX_INDEXES: usize = 100;

/// How the reply of the commands is read.
enum Kind {
    /// `use`: nothing is sent.
    Local,
    /// A cursor (`find`, `aggregate`, `listIndexes`, `listCollections`).
    Cursor,
    Count,
    /// One reply document, or the one field of it the shell method returns.
    Reply(Option<&'static str>),
    /// The documents in an array field of the reply (`distinct`, `listDatabases`).
    List(&'static str),
    /// `insert`, `update` and `delete` commands: the counts of their replies add up.
    Writes { ordered: bool },
}

/// What a command means for the server.
struct Prepared {
    /// Where the commands run (`admin` for `buildInfo`, `listDatabases` and `renameCollection`).
    run_database: String,
    /// The collection, for the approval card; empty for a command of the database.
    collection: String,
    /// The namespace a `getMore` and a `killCursors` name.
    cursor_collection: String,
    steps: Vec<Value>,
    kind: Kind,
    /// Documents to read from a cursor.
    rows: usize,
    write: bool,
}

fn relaxed(document: &Document) -> Value {
    Bson::Document(document.clone()).into_relaxed_extjson()
}

fn find_document(spec: &FindSpec, limit: Option<u64>, batch: Option<u64>) -> Document {
    let mut command = doc! { "find": &spec.collection, "filter": spec.filter.clone() };
    if let Some(projection) = &spec.projection {
        command.insert("projection", projection.clone());
    }
    if let Some(sort) = &spec.sort {
        command.insert("sort", sort.clone());
    }
    if let Some(collation) = &spec.collation {
        command.insert("collation", collation.clone());
    }
    if spec.skip > 0 {
        command.insert("skip", i64::try_from(spec.skip).unwrap_or(i64::MAX));
    }
    if let Some(limit) = limit {
        command.insert("limit", i64::try_from(limit).unwrap_or(i64::MAX));
    }
    if let Some(batch) = batch {
        command.insert("batchSize", i64::try_from(batch).unwrap_or(i64::MAX));
    }
    command
}

/// The commands of `command` in `database`, for at most `rows` documents.
fn prepare(command: &Command, database: &str, rows: usize) -> Prepared {
    let in_database = |collection: &str, steps: Vec<Value>, kind: Kind, rows: usize, write: bool| Prepared {
        run_database: database.to_owned(),
        collection: collection.to_owned(),
        cursor_collection: collection.to_owned(),
        steps,
        kind,
        rows,
        write,
    };
    let cursor_size = i64::try_from(rows + 1).unwrap_or(i64::MAX);
    match command {
        Command::Use(_) | Command::Refused(_) => in_database("", Vec::new(), Kind::Local, rows, false),
        Command::Version => Prepared {
            run_database: "admin".into(),
            ..in_database("", vec![json!({"buildInfo": 1})], Kind::Reply(Some("version")), rows, false)
        },
        Command::ShowDatabases => Prepared {
            run_database: "admin".into(),
            ..in_database(
                "",
                vec![json!({"listDatabases": 1, "nameOnly": true})],
                Kind::List("databases"),
                rows,
                false,
            )
        },
        Command::ShowCollections => Prepared {
            cursor_collection: "$cmd.listCollections".into(),
            ..in_database(
                "",
                vec![json!({"listCollections": 1, "nameOnly": true, "cursor": {"batchSize": cursor_size}})],
                Kind::Cursor,
                rows,
                false,
            )
        },
        Command::DbStats => in_database("", vec![json!({"dbStats": 1})], Kind::Reply(None), rows, false),
        Command::Find { spec, explain, .. } => {
            // A limit the model wrote that fits the row limit is the number of documents asked for;
            // otherwise one more than the row limit is, so a cut is seen.
            let (rows, limit) = match spec.limit {
                Some(limit) if usize::try_from(limit).is_ok_and(|limit| limit <= rows) => {
                    (limit as usize, limit)
                }
                _ => (rows, rows as u64 + 1),
            };
            if let Some(verbosity) = explain {
                let find = find_document(spec, spec.limit, None);
                return in_database(
                    &spec.collection,
                    vec![json!({"explain": relaxed(&find), "verbosity": verbosity})],
                    Kind::Reply(None),
                    rows,
                    false,
                );
            }
            let find = find_document(spec, Some(limit), Some(limit));
            in_database(&spec.collection, vec![relaxed(&find)], Kind::Cursor, rows, false)
        }
        Command::Count { collection, filter } => {
            let mut count = doc! { "count": collection };
            if let Some(filter) = filter {
                count.insert("query", filter.clone());
            }
            in_database(collection, vec![relaxed(&count)], Kind::Count, rows, false)
        }
        Command::Aggregate {
            collection,
            pipeline,
            options,
        } => {
            let writes = mongo_shell::pipeline_writes(pipeline);
            let mut stages: Vec<Bson> = pipeline.iter().cloned().map(Bson::Document).collect();
            let mut command = doc! { "aggregate": collection };
            if writes {
                // The server runs it to the end: no `$limit` after a `$out`.
                command.insert("pipeline", stages);
                command.insert("cursor", doc! {});
            } else {
                stages.push(Bson::Document(doc! { "$limit": cursor_size }));
                command.insert("pipeline", stages);
                command.insert("cursor", doc! { "batchSize": cursor_size });
            }
            command.extend(options.clone());
            let kind = if writes { Kind::Reply(None) } else { Kind::Cursor };
            in_database(collection, vec![relaxed(&command)], kind, rows, writes)
        }
        Command::Distinct {
            collection,
            field,
            filter,
        } => {
            let mut distinct = doc! { "distinct": collection, "key": field };
            if let Some(filter) = filter {
                distinct.insert("query", filter.clone());
            }
            in_database(collection, vec![relaxed(&distinct)], Kind::List("values"), rows, false)
        }
        Command::GetIndexes { collection } => in_database(
            collection,
            vec![json!({"listIndexes": collection, "cursor": {"batchSize": cursor_size}})],
            Kind::Cursor,
            rows,
            false,
        ),
        Command::CollectionStats {
            collection,
            metric,
            scale,
        } => {
            let mut stats = doc! { "collStats": collection };
            if let Some(scale) = scale {
                stats.insert("scale", *scale);
            }
            in_database(collection, vec![relaxed(&stats)], Kind::Reply(*metric), rows, false)
        }
        Command::Write {
            collection,
            steps,
            ordered,
            ..
        } => {
            let counted = steps
                .first()
                .and_then(|step| step.keys().next())
                .is_some_and(|name| matches!(name.as_str(), "insert" | "update" | "delete"));
            let kind = if counted {
                Kind::Writes { ordered: *ordered }
            } else {
                Kind::Reply(None)
            };
            in_database(collection, steps.iter().map(relaxed).collect(), kind, rows, true)
        }
        Command::Rename { collection, new_name } => Prepared {
            run_database: "admin".into(),
            ..in_database(
                collection,
                vec![json!({
                    "renameCollection": format!("{database}.{collection}"),
                    "to": format!("{database}.{new_name}"),
                })],
                Kind::Reply(None),
                rows,
                true,
            )
        },
    }
}

fn document_column() -> Vec<ResultColumn> {
    vec![ResultColumn {
        name: "document".into(),
        data_type: "json".into(),
    }]
}

/// A result of documents, one per row, as relaxed Extended JSON text.
fn documents_result(found: MongoDocumentsResult, read_only: bool, affected_rows: Option<u64>) -> QueryResult {
    QueryResult {
        columns: document_column(),
        rows: found.documents.iter().map(|document| vec![document.to_string()]).collect(),
        row_count: found.document_count,
        truncated: found.truncated,
        truncation: found.truncation,
        cells_truncated: found.cells_truncated,
        affected_rows,
        row_limit: found.limit,
        read_only_enforced: read_only,
        elapsed_ms: found.elapsed_ms,
    }
}

/// The reply of a command without the bookkeeping the server adds to it.
fn tidy(mut reply: Document) -> Document {
    reply.remove("ok");
    reply.remove("operationTime");
    let bookkeeping: Vec<String> = reply.keys().filter(|key| key.starts_with('$')).cloned().collect();
    for key in bookkeeping {
        reply.remove(&key);
    }
    reply
}

fn malformed() -> AgentError {
    AgentError::driver(
        "query.malformed",
        "the MongoDB driver's reply is not shaped like a command reply",
        false,
    )
}

fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs the commands and reads their replies.
fn execute(target: &mongo::Target<'_>, prepared: &Prepared) -> Result<QueryResult, AgentError> {
    let target = mongo::Target {
        database: &prepared.run_database,
        collection: &prepared.cursor_collection,
        ..*target
    };
    let target = &target;
    let limits = target.limits;
    let started = Instant::now();
    let Some(first) = prepared.steps.first() else {
        return Err(AgentError::Internal("a MongoDB command without a step".into()));
    };
    match &prepared.kind {
        Kind::Local => Err(AgentError::Internal("a local command was sent".into())),
        Kind::Cursor => {
            let found = mongo::documents(target, first, prepared.rows)?;
            Ok(documents_result(found, true, None))
        }
        Kind::Count => {
            let counted = mongo::count(target, first)?;
            Ok(QueryResult {
                columns: vec![ResultColumn {
                    name: "count".into(),
                    data_type: "long".into(),
                }],
                rows: vec![vec![counted.count.to_string()]],
                row_count: 1,
                truncated: false,
                truncation: None,
                cells_truncated: 0,
                affected_rows: None,
                row_limit: 1,
                read_only_enforced: true,
                elapsed_ms: counted.elapsed_ms,
            })
        }
        Kind::Reply(pick) => {
            let reply = tidy(mongo::run(target, first)?);
            let shown = match pick {
                Some(field) => {
                    let value = reply.get(field).cloned().ok_or_else(malformed)?;
                    doc! { *field: value }
                }
                None => reply,
            };
            let mut found = mongo::from_values(limits, 1, [Bson::Document(shown)]);
            found.elapsed_ms = millis(started);
            Ok(documents_result(found, !prepared.write, None))
        }
        Kind::List(field) => {
            let mut reply = mongo::run(target, first)?;
            let Some(Bson::Array(items)) = reply.remove(*field) else {
                return Err(malformed());
            };
            let mut found = mongo::from_values(limits, prepared.rows, items);
            found.elapsed_ms = millis(started);
            Ok(documents_result(found, true, None))
        }
        Kind::Writes { ordered } => {
            let mut total = MongoWriteResult {
                n: 0,
                n_modified: None,
                upserted: Vec::new(),
                write_errors: Vec::new(),
                elapsed_ms: 0,
            };
            for step in &prepared.steps {
                let part = mongo::write(target, step)?;
                total.n += part.n;
                total.n_modified = match (total.n_modified, part.n_modified) {
                    (None, None) => None,
                    (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
                };
                total.upserted.extend(part.upserted);
                let failed = !part.write_errors.is_empty();
                total.write_errors.extend(part.write_errors);
                if failed && *ordered {
                    break;
                }
            }
            let mut summary = serde_json::Map::new();
            summary.insert("n".into(), json!(total.n));
            if let Some(modified) = total.n_modified {
                summary.insert("nModified".into(), json!(modified));
            }
            if !total.upserted.is_empty() {
                summary.insert("upserted".into(), Value::Array(total.upserted));
            }
            if !total.write_errors.is_empty() {
                summary.insert("writeErrors".into(), Value::Array(total.write_errors));
            }
            let affected = total.n_modified.unwrap_or(total.n);
            let summary = Bson::try_from(Value::Object(summary)).map_err(|_| malformed())?;
            let mut found = mongo::from_values(limits, 1, [summary]);
            found.elapsed_ms = millis(started);
            Ok(documents_result(found, false, Some(affected)))
        }
    }
}

impl ToolLayer {
    /// `execute_query` for a MongoDB session.
    pub(super) fn execute_mongo_shell(
        &self,
        request: &ExecuteQueryRequest,
        info: &ConnectionInfo,
        access: AgentAccess,
        slot: &OpenSession,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<StatementResult, AgentError> {
        Self::check_not_cancelled(cancel)?;
        self.check_length(&request.statement)?;
        trace.statement = Some(request.statement.clone());
        let parsed = mongo_shell::parse(&request.statement).map_err(|why| {
            AgentError::invalid(format!("not a MongoDB shell command this tool can run: {why}"))
        })?;
        let command = &parsed.command;
        // The policy: a read runs; a change needs `read_write`; a dangerous one also needs the flag.
        let dangerous = match command.class() {
            Class::Read => None,
            Class::Refused(name) => {
                return Err(AgentError::DangerousRefused(format!(
                    "`{name}` is never run by agents (it can do anything the connection's user can, including changing users, roles and server settings); do not retry it or reword it, ask the user to run it themselves in the tablex app"
                )))
            }
            Class::Write(_) if access < AgentAccess::ReadWrite => {
                return Err(AgentError::WriteRefused(format!(
                    "this MongoDB command (`{}`) changes data and the agent access is `{access}`; writes need `read_write`, which only the user can set on the connection",
                    command.name()
                )))
            }
            Class::Write(reason) => {
                // A front end that never runs dangerous commands says so at once (in the approval
                // step), not by asking the model to call again with the flag.
                let flagged = request.dangerous.unwrap_or(false) || !self.approver.allows_dangerous(info);
                match reason {
                    Some(reason) if !flagged => {
                        return Err(AgentError::DangerousRefused(format!(
                            "this MongoDB command is dangerous: {reason}. Do not retry unless the user asked for it; then call again with dangerous=true and the user will be asked to approve"
                        )))
                    }
                    reason => reason.map(str::to_owned),
                }
            }
        };
        if let Command::Use(name) = command {
            let started = Instant::now();
            let note = json!({
                "database": clean_name(name, MAX_NAME_CHARS),
                "note": "each call is self-contained and nothing is switched: pass `database` (or use db.getSiblingDB(name)) to run in a database",
            });
            let note = Bson::try_from(note).map_err(|_| malformed())?;
            let mut result = documents_result(mongo::from_values(&self.limits, 1, [note]), true, None);
            result.elapsed_ms = millis(started);
            return Ok(result);
        }
        let database = match (&parsed.database, &request.database) {
            (Some(name), _) | (None, Some(name)) => name.clone(),
            (None, None) => match self.resolve_namespace(slot, None, None)? {
                (Some(only), _) => only,
                _ => return Err(AgentError::invalid("`database` is required")),
            },
        };
        if database.is_empty() {
            return Err(AgentError::invalid(
                "`database` must not be empty; call list_namespaces for the names",
            ));
        }
        Self::check_database(Some(&database))?;
        // `db.getSiblingDB(name)` in the text wins over the `database` argument: the scope of the
        // connection (checked on the argument before the call) must hold for the one that runs.
        Self::check_allowed_database(info, &database)?;
        // A `$out` / `$merge` target and a `$lookup` / `$unionWith` source can be another database
        // (`{db, coll}`): each must be one the agent may use too.
        if let Command::Aggregate { pipeline, .. } = command {
            for target in mongo_shell::pipeline_databases(pipeline) {
                Self::check_allowed_database(info, &target)?;
            }
        }
        if let Some(collection) = command.collection() {
            if collection.is_empty() {
                return Err(AgentError::invalid("the collection name is empty"));
            }
            Self::check_collection(collection)?;
        }
        if let Command::Rename { new_name, .. } = command {
            Self::check_collection(new_name)?;
        }
        let rows = self.limits.rows_for(request.row_limit);
        let prepared = prepare(command, &database, rows);
        let shown = match prepared.steps.as_slice() {
            [only] => only.clone(),
            many => Value::Array(many.to_vec()),
        };
        let call = MongoCall {
            tool: ToolName::ExecuteQuery,
            connection_id: &info.id,
            database: &database,
            collection: &prepared.collection,
            command: &shown,
            dangerous: dangerous.as_deref(),
        };
        let result = self.run_mongo_slot(&call, prepared.write, info, slot, cancel, trace, |target| {
            execute(target, &prepared)
        })?;
        trace.rows = Some(result.affected_rows.unwrap_or(result.row_count as u64));
        Ok(result)
    }

    /// `describe_table` for a collection: its indexes, from `listIndexes` (which every role that
    /// can see the collection may run). A collection has no columns; `_id` is its key.
    pub(super) fn describe_mongo_collection(
        &self,
        request: DescribeTableRequest,
        info: &ConnectionInfo,
        slot: &OpenSession,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<DescribeTableResult, AgentError> {
        let (database, _) = self.resolve_namespace(slot, request.database, request.schema)?;
        let database = database.unwrap_or_default();
        Self::check_database(Some(&database))?;
        if request.table.is_empty() {
            return Err(AgentError::invalid("`table` is the collection name"));
        }
        Self::check_collection(&request.table)?;
        let command = json!({"listIndexes": request.table, "cursor": {"batchSize": MAX_INDEXES + 1}});
        let call = MongoCall {
            tool: ToolName::DescribeTable,
            connection_id: &info.id,
            database: &database,
            collection: &request.table,
            command: &command,
            dangerous: None,
        };
        let found = self.run_mongo_slot(&call, false, info, slot, cancel, trace, |target| {
            mongo::documents(target, &command, MAX_INDEXES)
        })?;
        let mut description = collection_description(&request.table, &database, &found.documents);
        description.sanitize_with(&slot.masks);
        description.cap_ddl(self.limits.max_ddl_bytes);
        Ok(description)
    }
}

/// Documents read from each collection to see which fields it has.
const SAMPLE_DOCUMENTS: i64 = 20;
/// Collections sampled by one `get_schema_context` at most (each is a round trip).
const MAX_SAMPLED_COLLECTIONS: usize = 20;
/// Fields described for one collection at most, and how deep a document is followed.
const MAX_SAMPLED_FIELDS: usize = 100;
const MAX_SAMPLED_DEPTH: usize = 3;

/// The names of the BSON types a field was seen with, for the digest.
fn bson_type_name(value: &Bson) -> String {
    match value {
        Bson::Double(_) => "double".into(),
        Bson::String(_) => "string".into(),
        Bson::Array(items) => {
            let mut inner: Vec<String> = items.iter().map(bson_type_name).collect();
            inner.sort();
            inner.dedup();
            if inner.is_empty() {
                "array".into()
            } else {
                format!("array<{}>", inner.join("|"))
            }
        }
        Bson::Document(_) => "object".into(),
        Bson::Boolean(_) => "bool".into(),
        Bson::Null | Bson::Undefined => "null".into(),
        Bson::RegularExpression(_) => "regex".into(),
        Bson::Int32(_) => "int".into(),
        Bson::Int64(_) => "long".into(),
        Bson::Timestamp(_) => "timestamp".into(),
        Bson::Binary(_) => "binData".into(),
        Bson::ObjectId(_) => "objectId".into(),
        Bson::DateTime(_) => "date".into(),
        Bson::Decimal128(_) => "decimal".into(),
        _ => "other".into(),
    }
}

/// Records, for every field of `document` (nested ones as `a.b`), the types seen.
fn note_fields(document: &Document, prefix: &str, depth: usize, seen: &mut Vec<(String, Vec<String>, usize)>) {
    for (key, value) in document {
        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        let kind = bson_type_name(value);
        if let Some((_, kinds, count)) = seen.iter_mut().find(|(name, _, _)| *name == path) {
            *count += 1;
            if !kinds.contains(&kind) {
                kinds.push(kind);
            }
        } else if seen.len() < MAX_SAMPLED_FIELDS {
            seen.push((path.clone(), vec![kind], 1));
        }
        if let (Bson::Document(inner), true) = (value, depth + 1 < MAX_SAMPLED_DEPTH) {
            note_fields(inner, &path, depth + 1, seen);
        }
    }
}

/// A description of a collection from a sample of its documents: a field is a column, its type the
/// BSON types seen, nullable when some document lacks it or holds null.
fn describe_sample(collection: &str, database: &str, documents: &[Bson]) -> DescribeTableResult {
    use crate::table::{ColumnDescription, TableDescription};
    let mut seen: Vec<(String, Vec<String>, usize)> = Vec::new();
    let mut sampled = 0usize;
    for document in documents {
        if let Bson::Document(document) = document {
            sampled += 1;
            note_fields(document, "", 0, &mut seen);
        }
    }
    let columns = seen
        .into_iter()
        .map(|(name, mut kinds, count)| {
            kinds.sort();
            let nullable = count < sampled || kinds.iter().any(|kind| kind == "null");
            let primary_key = name == "_id";
            ColumnDescription {
                data_type: kinds.iter().filter(|kind| *kind != "null").cloned().collect::<Vec<_>>().join("|"),
                name,
                nullable,
                primary_key,
                default: None,
                comment: None,
            }
        })
        .map(|mut column| {
            if column.data_type.is_empty() {
                column.data_type = "null".into();
            }
            column
        })
        .collect();
    TableDescription {
        name: collection.to_owned(),
        database: Some(database.to_owned()),
        schema: None,
        columns,
        primary_key: vec!["_id".to_owned()],
        unique_columns: Vec::new(),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        ddl: None,
        ddl_truncated: false,
    }
}

impl ToolLayer {
    /// `get_schema_context` on a MongoDB connection: the fields of each collection (at most
    /// [`MAX_SAMPLED_COLLECTIONS`] of the `names`, and `cap`) from a sample of its documents.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn mongo_schema_context(
        &self,
        info: &ConnectionInfo,
        slot: &OpenSession,
        database: &str,
        label: &str,
        names: Vec<String>,
        cap: usize,
        budget: usize,
        cancel: &CancelToken,
        trace: &mut Trace,
    ) -> Result<GetSchemaContextResult, AgentError> {
        Self::check_database(Some(database))?;
        let cap = cap.min(MAX_SAMPLED_COLLECTIONS);
        let (sampled_names, rest) = names.split_at(names.len().min(cap));
        let mut undescribed: Vec<String> = rest.to_vec();
        let command = json!({"find": "", "limit": SAMPLE_DOCUMENTS});
        let call = MongoCall {
            tool: ToolName::GetSchemaContext,
            connection_id: &info.id,
            database,
            collection: sampled_names.first().map_or("", String::as_str),
            command: &command,
            dangerous: None,
        };
        let mut described = Vec::new();
        let found = self.run_mongo_slot(&call, false, info, slot, cancel, trace, |target| {
            let mut found = Vec::new();
            for name in sampled_names {
                if target.cancel.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                let one = mongo::Target {
                    collection: name,
                    ..*target
                };
                let sample = json!({
                    "find": name,
                    "limit": SAMPLE_DOCUMENTS,
                    "batchSize": SAMPLE_DOCUMENTS,
                    "singleBatch": true,
                });
                match mongo::run(&one, &sample) {
                    Ok(mut reply) => {
                        let documents = match reply.remove("cursor") {
                            Some(Bson::Document(mut cursor)) => match cursor.remove("firstBatch") {
                                Some(Bson::Array(documents)) => documents,
                                _ => Vec::new(),
                            },
                            _ => Vec::new(),
                        };
                        found.push((name.clone(), Some(documents)));
                    }
                    Err(AgentError::Cancelled) => return Err(AgentError::Cancelled),
                    // One collection that cannot be read (a view without rights, say) does not
                    // sink the digest.
                    Err(_) => found.push((name.clone(), None)),
                }
            }
            Ok(found)
        })?;
        for (name, documents) in found {
            match documents {
                Some(documents) => {
                    let mut description = describe_sample(&name, database, &documents);
                    description.sanitize_with(&slot.masks);
                    described.push(description);
                }
                None => undescribed.push(name),
            }
        }
        undescribed.sort();
        Ok(build_schema_context(
            &ContextInput {
                label,
                dialect: slot.info.kind.driver_id(),
                tables: &described,
                undescribed: &undescribed,
                budget,
            },
            self.estimator.as_ref(),
        ))
    }
}

/// The indexes of `listIndexes` as a description: names, key fields, uniqueness, and the
/// `createIndex` calls that rebuild them (options such as `expireAfterSeconds` or a partial filter
/// have no other place).
fn collection_description(name: &str, database: &str, indexes: &[Value]) -> DescribeTableResult {
    use crate::table::{IndexDescription, TableDescription};
    let mut described = Vec::new();
    let mut unique_columns = Vec::new();
    let mut ddl = Vec::new();
    for index in indexes {
        let Some(spec) = index.as_object() else { continue };
        let index_name = spec.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        let columns: Vec<String> = spec
            .get("key")
            .and_then(Value::as_object)
            .map(|key| key.keys().cloned().collect())
            .unwrap_or_default();
        let unique = spec.get("unique").and_then(Value::as_bool).unwrap_or(false);
        if unique && columns.len() == 1 && columns[0] != "_id" {
            unique_columns.push(columns[0].clone());
        }
        if index_name != "_id_" {
            let mut options = spec.clone();
            for key in ["key", "v", "ns"] {
                options.remove(key);
            }
            ddl.push(format!(
                "db.getCollection({}).createIndex({}, {})",
                Value::String(name.to_owned()),
                spec.get("key").cloned().unwrap_or(Value::Null),
                Value::Object(options)
            ));
        }
        described.push(IndexDescription {
            name: index_name,
            columns,
            unique,
        });
    }
    TableDescription {
        name: name.to_owned(),
        database: Some(database.to_owned()),
        schema: None,
        columns: Vec::new(),
        primary_key: vec!["_id".to_owned()],
        unique_columns,
        indexes: described,
        foreign_keys: Vec::new(),
        ddl: (!ddl.is_empty()).then(|| ddl.join(";\n")),
        ddl_truncated: false,
    }
}
