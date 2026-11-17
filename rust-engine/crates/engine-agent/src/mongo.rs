//! `mongo_find`, `mongo_aggregate` and `mongo_count`: the commands they send, the reading of the
//! cursor they get back, and the limits on what is returned.
//!
//! Everything goes through [`Session::mongo_command`], one command at a time, and only the read
//! commands `find`, `aggregate`, `count`, `getMore` and `killCursors` are ever built here (the
//! driver refuses a writing command or pipeline again before it sends it). At most one more
//! document than the limit is fetched, to know whether the result was cut; a cursor that is still
//! open afterwards is closed.

use crate::cancel::CancelToken;
use crate::error::AgentError;
use crate::limits::{truncate_text, Limits};
use crate::ports::Session;
use crate::result::Truncation;
use crate::tools::{
    MongoAggregateRequest, MongoCountRequest, MongoCountResult, MongoDocumentsResult,
    MongoFindRequest,
};
use bson::{Bson, Document};
use serde_json::{json, Map, Value};
use std::ops::ControlFlow;
use std::time::{Duration, Instant};

/// How long closing a cursor may take: it runs after a cancel or a timeout too.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
/// Empty batches from a cursor that is still open, in a row, before the read stops.
const MAX_EMPTY_BATCHES: usize = 3;

/// Where a call runs.
pub(crate) struct Target<'a> {
    pub session: &'a dyn Session,
    pub database: &'a str,
    pub collection: &'a str,
    pub limits: &'a Limits,
    /// The statement timeout: the server's `maxTimeMS` and the wait for its answer.
    pub timeout: Duration,
    pub cancel: &'a CancelToken,
}

/// `limit` is at least 1 when given (the rest is capped by the layer's row limit).
pub(crate) fn check_limit(limit: Option<usize>) -> Result<(), AgentError> {
    match limit {
        Some(0) => Err(AgentError::invalid("`limit` must be at least 1")),
        _ => Ok(()),
    }
}

/// The `find` command for at most `rows` documents (one more is asked for, to see whether the
/// result is cut).
pub(crate) fn find_command(request: &MongoFindRequest, rows: usize) -> Value {
    let mut command = Map::new();
    command.insert("find".into(), json!(request.collection));
    for (key, document) in [
        ("filter", &request.filter),
        ("projection", &request.projection),
        ("sort", &request.sort),
    ] {
        if let Some(document) = document {
            command.insert(key.into(), Value::Object(document.clone()));
        }
    }
    if let Some(skip) = request.skip.filter(|skip| *skip > 0) {
        command.insert("skip".into(), json!(skip));
    }
    command.insert("limit".into(), json!(rows + 1));
    command.insert("batchSize".into(), json!(rows + 1));
    Value::Object(command)
}

/// The `aggregate` command: the pipeline, then a `$limit` of one more than `rows`, so the server
/// stops there.
pub(crate) fn aggregate_command(request: &MongoAggregateRequest, rows: usize) -> Value {
    let mut pipeline: Vec<Value> = request
        .pipeline
        .iter()
        .map(|stage| Value::Object(stage.clone()))
        .collect();
    pipeline.push(json!({"$limit": rows + 1}));
    json!({
        "aggregate": request.collection,
        "pipeline": pipeline,
        "cursor": {"batchSize": rows + 1},
    })
}

pub(crate) fn count_command(request: &MongoCountRequest) -> Value {
    let mut command = Map::new();
    command.insert("count".into(), json!(request.collection));
    if let Some(filter) = &request.filter {
        command.insert("query".into(), Value::Object(filter.clone()));
    }
    Value::Object(command)
}

fn malformed() -> AgentError {
    AgentError::driver(
        "query.malformed",
        "the MongoDB driver's reply is not shaped like a command reply",
        false,
    )
}

/// Runs one command and returns the reply document, or the error the server reported in it.
fn run(target: &Target<'_>, command: &Value) -> Result<Document, AgentError> {
    if target.cancel.is_cancelled() {
        return Err(AgentError::Cancelled);
    }
    let reply = target
        .session
        .mongo_command(target.database, command, target.timeout, target.cancel)?;
    let Ok(Bson::Document(reply)) = Bson::try_from(reply) else {
        return Err(malformed());
    };
    let succeeded = match reply.get("ok") {
        Some(Bson::Double(ok)) => *ok == 1.0,
        Some(Bson::Int32(ok)) => *ok == 1,
        Some(Bson::Int64(ok)) => *ok == 1,
        Some(Bson::Boolean(ok)) => *ok,
        _ => false,
    };
    if succeeded {
        return Ok(reply);
    }
    let code_name = reply.get_str("codeName").unwrap_or_default();
    let message = reply
        .get_str("errmsg")
        .unwrap_or("the server rejected the command");
    if code_name == "MaxTimeMSExpired" {
        return Err(AgentError::Limit(format!(
            "the server stopped the query after the {} s statement timeout",
            target.timeout.as_secs()
        )));
    }
    let message = if code_name.is_empty() {
        message.to_owned()
    } else {
        format!("{code_name}: {message}")
    };
    Err(AgentError::driver("query.failed", message, false))
}

/// The cursor id and the documents of a `firstBatch` or `nextBatch` reply.
fn batch(mut reply: Document, key: &str) -> Result<(i64, Vec<Bson>), AgentError> {
    let Some(Bson::Document(mut cursor)) = reply.remove("cursor") else {
        return Err(malformed());
    };
    let id = match cursor.get("id") {
        Some(Bson::Int64(id)) => *id,
        Some(Bson::Int32(id)) => i64::from(*id),
        _ => return Err(malformed()),
    };
    let Some(Bson::Array(documents)) = cursor.remove(key) else {
        return Err(malformed());
    };
    Ok((id, documents))
}

/// Builds the documents of a result within the limits.
struct Collector<'a> {
    documents: Vec<Value>,
    row_limit: usize,
    limits: &'a Limits,
    bytes: usize,
    cells_truncated: usize,
    stopped: Option<Truncation>,
}

impl<'a> Collector<'a> {
    fn new(row_limit: usize, limits: &'a Limits) -> Self {
        Self {
            documents: Vec::with_capacity(row_limit.min(256)),
            row_limit,
            limits,
            bytes: 0,
            cells_truncated: 0,
            stopped: None,
        }
    }

    /// Adds one document. `Break`: it was not kept, nor will any later one be.
    fn push(&mut self, document: Bson) -> ControlFlow<()> {
        if self.stopped.is_some() {
            return ControlFlow::Break(());
        }
        if self.documents.len() >= self.row_limit {
            self.stopped = Some(Truncation::RowLimit);
            return ControlFlow::Break(());
        }
        let mut value = document.into_relaxed_extjson();
        let mut cut = cut_strings(&mut value, self.limits.max_cell_bytes);
        let mut size = value.to_string().len();
        // A document that alone is over the byte cap is replaced by the start of its text, so the
        // first one is never what blows the cap.
        if size > self.limits.max_result_bytes {
            let text = value.to_string();
            let shown = truncate_text(&text, self.limits.max_cell_bytes).unwrap_or(text);
            value = json!({ "$truncated": shown });
            size = value.to_string().len();
            cut += 1;
        }
        if !self.documents.is_empty() && self.bytes + size > self.limits.max_result_bytes {
            self.stopped = Some(Truncation::ByteLimit);
            return ControlFlow::Break(());
        }
        self.bytes += size;
        self.cells_truncated += cut;
        self.documents.push(value);
        ControlFlow::Continue(())
    }
}

/// Cuts every string longer than `max` bytes (see [`truncate_text`]); returns how many were cut.
fn cut_strings(value: &mut Value, max: usize) -> usize {
    match value {
        Value::String(text) => match truncate_text(text, max) {
            Some(cut) => {
                *text = cut;
                1
            }
            None => 0,
        },
        Value::Array(items) => items.iter_mut().map(|item| cut_strings(item, max)).sum(),
        Value::Object(fields) => fields
            .values_mut()
            .map(|field| cut_strings(field, max))
            .sum(),
        _ => 0,
    }
}

/// Runs a `find` or `aggregate` command (built for `rows` documents) and reads its cursor.
pub(crate) fn documents(
    target: &Target<'_>,
    command: &Value,
    rows: usize,
) -> Result<MongoDocumentsResult, AgentError> {
    let started = Instant::now();
    let mut collector = Collector::new(rows, target.limits);
    let mut open = None;
    let read = read_cursor(target, command, rows, &mut collector, &mut open);
    // Whatever way the read ended, a cursor the server still holds is closed.
    if let Some(id) = open {
        close_cursor(target, id);
    }
    read?;
    Ok(MongoDocumentsResult {
        document_count: collector.documents.len(),
        documents: collector.documents,
        truncated: collector.stopped.is_some(),
        truncation: collector.stopped,
        cells_truncated: collector.cells_truncated,
        limit: rows,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// The first batch, then `getMore` while the limits leave room. `open` holds the id of the cursor
/// while the server has it open.
fn read_cursor(
    target: &Target<'_>,
    command: &Value,
    rows: usize,
    collector: &mut Collector<'_>,
    open: &mut Option<i64>,
) -> Result<(), AgentError> {
    let mut reply = run(target, command)?;
    let mut key = "firstBatch";
    let mut empty = 0;
    loop {
        let (id, documents) = batch(reply, key)?;
        *open = (id != 0).then_some(id);
        empty = if documents.is_empty() { empty + 1 } else { 0 };
        for document in documents {
            if collector.push(document).is_break() {
                return Ok(());
            }
        }
        let Some(id) = *open else {
            return Ok(());
        };
        if empty >= MAX_EMPTY_BATCHES {
            return Ok(());
        }
        // Room for the documents still wanted: `rows` and the one more that shows a cut.
        let wanted = rows + 1 - collector.documents.len();
        reply = run(
            target,
            &json!({
                "getMore": {"$numberLong": id.to_string()},
                "collection": target.collection,
                "batchSize": wanted,
            }),
        )?;
        key = "nextBatch";
    }
}

/// Best effort, and with a token of its own: the call may have been cancelled, which is why the
/// cursor is still open.
fn close_cursor(target: &Target<'_>, id: i64) {
    let _ = target.session.mongo_command(
        target.database,
        &json!({
            "killCursors": target.collection,
            "cursors": [{"$numberLong": id.to_string()}],
        }),
        CLEANUP_TIMEOUT,
        &CancelToken::new(),
    );
}

/// Runs a `count` command.
pub(crate) fn count(target: &Target<'_>, command: &Value) -> Result<MongoCountResult, AgentError> {
    let started = Instant::now();
    let reply = run(target, command)?;
    let count = match reply.get("n") {
        Some(Bson::Int32(n)) => u64::try_from(*n).ok(),
        Some(Bson::Int64(n)) => u64::try_from(*n).ok(),
        Some(Bson::Double(n)) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
        _ => None,
    }
    .ok_or_else(malformed)?;
    Ok(MongoCountResult {
        count,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}
