use bson::Document;
use engine_driver_api::{MutationRequest, NavigatorNode, NavigatorPage, NavigatorRequest};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);
const CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub(crate) enum CreateDatabaseError {
    Rejected(String),
    Unknown(String),
}

impl CreateDatabaseError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Rejected(message) | Self::Unknown(message) => message,
        }
    }
}

/// Why a document write (`insert` / `update` / `delete`) was not applied.
#[derive(Debug)]
pub(crate) enum MutateError {
    /// The request is malformed (identity or payload); nothing was sent.
    BadRequest(String),
    /// The write matched no document: it was deleted, or its `_id` changed,
    /// since the page was read.
    NoRowsAffected(String),
    Failed(String),
}

impl MutateError {
    pub(crate) fn code(&self) -> &'static str {
        use engine_driver_api::sql::codes;
        match self {
            Self::BadRequest(_) => codes::MUTATION_BAD_REQUEST,
            Self::NoRowsAffected(_) => codes::MUTATION_NO_ROWS_AFFECTED,
            Self::Failed(_) => codes::MUTATION_FAILED,
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::BadRequest(message) | Self::NoRowsAffected(message) | Self::Failed(message) => {
                message
            }
        }
    }
}

impl std::fmt::Display for MutateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

impl From<String> for MutateError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

pub(crate) trait MongoBackend: Send + Sync {
    fn list_database_names(&self) -> Result<Vec<String>, String>;
    fn create_database(&self, name: &str) -> Result<(), CreateDatabaseError>;
    fn list_collection_names(&self, database: &str) -> Result<Vec<String>, String>;
    fn find_documents(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        sort: Document,
        projection: Option<Document>,
        offset: u64,
        limit: u64,
    ) -> Result<FindDocuments, String>;
    fn insert_document(
        &self,
        database: &str,
        collection: &str,
        document: Document,
    ) -> Result<u64, String>;
    fn update_document(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        update: Document,
    ) -> Result<u64, String>;
    fn delete_document(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
    ) -> Result<u64, String>;
    /// Collection / index / database administration (see `admin`). Backends
    /// that do not implement it refuse instead of silently succeeding.
    fn admin_mutate(&self, _operation: &crate::admin::AdminMutation) -> Result<u64, String> {
        Err("this MongoDB backend does not support administration".into())
    }
    fn list_indexes(&self, _database: &str, _collection: &str) -> Result<Vec<Document>, String> {
        Err("this MongoDB backend does not support listing indexes".into())
    }
    /// Runs an aggregation to completion. Cursor errors fail the call (a
    /// partial result is never returned as if it were complete).
    fn aggregate_documents(
        &self,
        _database: &str,
        _collection: &str,
        _pipeline: Vec<Document>,
        _options: &AggOpts,
    ) -> Result<Vec<Document>, String> {
        Err("this MongoDB backend does not support aggregation".into())
    }
    /// Streams an aggregation through **one** cursor into `sink`, document by
    /// document (Export Data). Cursor and sink errors fail the call; the
    /// cancel token ends it early with [`CANCELLED_MESSAGE`].
    fn aggregate_stream(
        &self,
        _database: &str,
        _collection: &str,
        _pipeline: Vec<Document>,
        _options: &AggOpts,
        _sink: &mut (dyn FnMut(Document) -> Result<(), String> + Send),
    ) -> Result<(), String> {
        Err("this MongoDB backend does not support aggregation".into())
    }
    /// `explain` of an aggregation as the server's raw reply.
    fn explain_aggregate(
        &self,
        _database: &str,
        _collection: &str,
        _pipeline: Vec<Document>,
        _verbosity: crate::aggregate::ExplainVerbosity,
        _options: &AggOpts,
    ) -> Result<Document, String> {
        Err("this MongoDB backend does not support explain".into())
    }
    /// Best-effort server-side kill of the operation tagged with `comment`
    /// (see [`AggOpts::comment`]). Never blocks the caller.
    fn kill_operation(&self, _comment: &str) {}
    /// One raw `runCommand` (see [`crate::mongo_command`]). Cancelling
    /// `control` ends the call; `deadline` bounds the wait for the server.
    fn run_command(
        &self,
        _database: &str,
        _command: Document,
        _control: &AggControl,
        _deadline: Option<std::time::Duration>,
    ) -> Result<Document, crate::mongo_command::CommandError> {
        Err(crate::mongo_command::CommandError::Unsupported)
    }
    /// The server's `maxWireVersion` (9 = MongoDB 4.4), asked once; `None`
    /// when unknown.
    fn max_wire_version(&self, _control: &AggControl) -> Option<i32> {
        None
    }
}

/// Message of the error a cancelled operation returns.
pub(crate) const CANCELLED_MESSAGE: &str = "operation cancelled";
/// Time the client waits beyond the server-side `maxTimeMS` before giving up.
const CLIENT_DEADLINE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Cooperative per-operation cancellation shared by the FFI registry and the
/// running future (`tokio::select!` drops the query when it fires).
#[derive(Clone, Default)]
pub(crate) struct CancelToken(std::sync::Arc<CancelInner>);

#[derive(Default)]
struct CancelInner {
    flag: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    pub(crate) fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::SeqCst)
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CancelToken({})", self.is_cancelled())
    }
}

/// Per-call options of [`MongoBackend::aggregate_documents`] / `explain_aggregate`.
#[derive(Debug, Clone, Default)]
pub(crate) struct AggOpts {
    pub max_time_ms: Option<u64>,
    pub allow_disk_use: bool,
    pub collation: Option<Document>,
    pub batch_size: Option<u32>,
    /// Tags the operation in `currentOp` so [`MongoBackend::kill_operation`]
    /// can find it.
    pub comment: Option<String>,
    pub cancel: Option<CancelToken>,
}

/// Collects a cursor. Every item error is returned (the old code dropped
/// them with `filter_map(Result::ok)`, silently truncating results), and the
/// cancel token is checked between documents.
pub(crate) async fn collect_cursor<S, E>(
    mut stream: S,
    cancel: Option<&CancelToken>,
) -> Result<Vec<Document>, String>
where
    S: futures::Stream<Item = Result<Document, E>> + Unpin,
    E: std::fmt::Display,
{
    use futures::StreamExt;
    let mut documents = Vec::new();
    while let Some(item) = stream.next().await {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return Err(CANCELLED_MESSAGE.into());
        }
        documents.push(item.map_err(|error| format!("cursor error: {error}"))?);
    }
    Ok(documents)
}

/// Runs `future` until it finishes, the token fires, or `deadline` passes.
pub(crate) async fn run_controlled<T>(
    future: impl std::future::Future<Output = Result<T, String>>,
    cancel: Option<&CancelToken>,
    deadline: Option<std::time::Duration>,
) -> Result<T, String> {
    let cancelled = async {
        match cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending::<()>().await,
        }
    };
    let timed_out = async {
        match deadline {
            Some(duration) => tokio::time::sleep(duration).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        result = future => result,
        _ = cancelled => Err(CANCELLED_MESSAGE.into()),
        _ = timed_out => Err("operation timed out waiting for the server".into()),
    }
}

fn driver_aggregate_options(
    options: &AggOpts,
) -> Result<mongodb::options::AggregateOptions, String> {
    let mut driver_options = mongodb::options::AggregateOptions::default();
    driver_options.max_time = options.max_time_ms.map(std::time::Duration::from_millis);
    driver_options.allow_disk_use = options.allow_disk_use.then_some(true);
    driver_options.batch_size = options.batch_size;
    driver_options.comment = options.comment.clone().map(bson::Bson::String);
    if let Some(collation) = &options.collation {
        driver_options.collation = Some(
            bson::from_document(collation.clone())
                .map_err(|error| format!("invalid collation: {error}"))?,
        );
    }
    Ok(driver_options)
}

fn client_deadline(options: &AggOpts) -> Option<std::time::Duration> {
    options
        .max_time_ms
        .map(|ms| std::time::Duration::from_millis(ms) + CLIENT_DEADLINE_GRACE)
}

#[derive(Debug, Clone)]
pub(crate) struct FindDocuments {
    pub documents: Vec<Document>,
    pub total_count: u64,
    pub total_is_exact: bool,
}

#[derive(Debug)]
pub(crate) struct MongoClientBackend {
    client: mongodb::Client,
    /// The one logical session raw commands run in: a cursor opened by one
    /// command is bound to its session, so every `getMore` must reuse it
    /// (implicit sessions rotate between concurrent operations).
    command_session: tokio::sync::Mutex<Option<mongodb::ClientSession>>,
    /// The server's `maxWireVersion`, 0 until a `hello` has succeeded (a failed
    /// one is never remembered).
    wire_version: std::sync::atomic::AtomicI32,
}

impl MongoBackend for MongoClientBackend {
    fn list_database_names(&self) -> Result<Vec<String>, String> {
        crate::RUNTIME.block_on(async {
            self.client
                .list_database_names()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn create_database(&self, name: &str) -> Result<(), CreateDatabaseError> {
        crate::RUNTIME.block_on(async {
            self.client
                .database(name)
                .create_collection("tablex_init")
                .await
                .map_err(|error| {
                    use mongodb::error::ErrorKind;
                    let rejected = matches!(error.kind.as_ref(),
                        ErrorKind::Command(command) if matches!(command.code, 13 | 48 | 73))
                        || matches!(
                            error.kind.as_ref(),
                            ErrorKind::InvalidArgument { .. } | ErrorKind::Authentication { .. }
                        );
                    if rejected {
                        CreateDatabaseError::Rejected(error.to_string())
                    } else {
                        CreateDatabaseError::Unknown(error.to_string())
                    }
                })
        })
    }

    fn list_collection_names(&self, database: &str) -> Result<Vec<String>, String> {
        crate::RUNTIME.block_on(async {
            self.client
                .database(database)
                .list_collection_names()
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn find_documents(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        sort: Document,
        projection: Option<Document>,
        offset: u64,
        limit: u64,
    ) -> Result<FindDocuments, String> {
        crate::RUNTIME.block_on(async {
            let collection = self
                .client
                .database(database)
                .collection::<Document>(collection);
            let mut options = mongodb::options::FindOptions::builder()
                .sort(sort)
                .skip(offset)
                .limit(limit as i64)
                .build();
            options.projection = projection;
            let (cursor, total_count) = tokio::try_join!(
                collection.find(filter.clone()).with_options(options),
                collection.count_documents(filter),
            )
            .map_err(|error| error.to_string())?;
            let documents = futures::TryStreamExt::try_collect(cursor)
                .await
                .map_err(|error| error.to_string())?;
            Ok(FindDocuments {
                documents,
                total_count,
                total_is_exact: true,
            })
        })
    }

    fn insert_document(
        &self,
        database: &str,
        collection: &str,
        document: Document,
    ) -> Result<u64, String> {
        crate::RUNTIME.block_on(async {
            self.client
                .database(database)
                .collection::<Document>(collection)
                .insert_one(document)
                .await
                .map(|_| 1)
                .map_err(|error| error.to_string())
        })
    }

    fn update_document(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        update: Document,
    ) -> Result<u64, String> {
        crate::RUNTIME.block_on(async {
            self.client
                .database(database)
                .collection::<Document>(collection)
                .update_one(filter, update)
                .await
                .map(|result| result.matched_count)
                .map_err(|error| error.to_string())
        })
    }

    fn delete_document(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
    ) -> Result<u64, String> {
        crate::RUNTIME.block_on(async {
            self.client
                .database(database)
                .collection::<Document>(collection)
                .delete_one(filter)
                .await
                .map(|result| result.deleted_count)
                .map_err(|error| error.to_string())
        })
    }

    fn admin_mutate(&self, operation: &crate::admin::AdminMutation) -> Result<u64, String> {
        crate::RUNTIME.block_on(crate::admin::run_admin_mutation(&self.client, operation))
    }

    fn list_indexes(&self, database: &str, collection: &str) -> Result<Vec<Document>, String> {
        crate::RUNTIME.block_on(crate::admin::list_indexes_raw(
            &self.client,
            database,
            collection,
        ))
    }

    fn aggregate_documents(
        &self,
        database: &str,
        collection: &str,
        pipeline: Vec<Document>,
        options: &AggOpts,
    ) -> Result<Vec<Document>, String> {
        crate::RUNTIME.block_on(run_controlled(
            async {
                let driver_options = driver_aggregate_options(options)?;
                let cursor = self
                    .client
                    .database(database)
                    .collection::<Document>(collection)
                    .aggregate(pipeline)
                    .with_options(driver_options)
                    .await
                    .map_err(|error| error.to_string())?;
                collect_cursor(cursor, options.cancel.as_ref()).await
            },
            options.cancel.as_ref(),
            client_deadline(options),
        ))
    }

    fn aggregate_stream(
        &self,
        database: &str,
        collection: &str,
        pipeline: Vec<Document>,
        options: &AggOpts,
        sink: &mut (dyn FnMut(Document) -> Result<(), String> + Send),
    ) -> Result<(), String> {
        use futures::StreamExt;
        // No client deadline: an export may legitimately run long; the user
        // cancels it, and `maxTimeMS` still bounds the server-side work.
        crate::RUNTIME.block_on(run_controlled(
            async {
                let driver_options = driver_aggregate_options(options)?;
                let mut cursor = self
                    .client
                    .database(database)
                    .collection::<Document>(collection)
                    .aggregate(pipeline)
                    .with_options(driver_options)
                    .await
                    .map_err(|error| error.to_string())?;
                while let Some(item) = cursor.next().await {
                    if options.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
                        return Err(CANCELLED_MESSAGE.to_string());
                    }
                    sink(item.map_err(|error| format!("cursor error: {error}"))?)?;
                }
                Ok(())
            },
            options.cancel.as_ref(),
            None,
        ))
    }

    fn explain_aggregate(
        &self,
        database: &str,
        collection: &str,
        pipeline: Vec<Document>,
        verbosity: crate::aggregate::ExplainVerbosity,
        options: &AggOpts,
    ) -> Result<Document, String> {
        let command = crate::aggregate::explain_command(
            collection,
            pipeline,
            verbosity,
            options.max_time_ms,
            options.allow_disk_use,
            options.collation.as_ref(),
        );
        crate::RUNTIME.block_on(run_controlled(
            async {
                self.client
                    .database(database)
                    .run_command(command)
                    .await
                    .map_err(|error| error.to_string())
            },
            options.cancel.as_ref(),
            client_deadline(options),
        ))
    }

    fn run_command(
        &self,
        database: &str,
        command: Document,
        control: &AggControl,
        deadline: Option<std::time::Duration>,
    ) -> Result<Document, crate::mongo_command::CommandError> {
        use crate::mongo_command::CommandError;
        crate::RUNTIME.block_on(async {
            let cancelled = || async {
                match &control.cancel {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            let deadline_at = deadline.map(|duration| tokio::time::Instant::now() + duration);
            let timed_out = || async {
                match deadline_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let timeout_error =
                || CommandError::Failed("operation timed out waiting for the server".into());
            // Wait for the connection's one command session. A run that is
            // stopped while it waits holds nothing and kills nothing.
            let mut guard = tokio::select! {
                guard = self.command_session.lock() => guard,
                _ = cancelled() => return Err(CommandError::Cancelled),
                _ = timed_out() => return Err(timeout_error()),
            };
            if guard.is_none() {
                // The first raw command of a connection starts the session; it
                // may wait out server selection, so Stop and the deadline apply.
                let started = tokio::select! {
                    started = self.client.start_session() => started,
                    _ = cancelled() => return Err(CommandError::Cancelled),
                    _ = timed_out() => return Err(timeout_error()),
                };
                *guard = Some(started.map_err(|error| CommandError::Failed(error.to_string()))?);
            }
            let session = guard.as_mut().expect("session started above");
            let session_id = session.id().clone();
            let database = self.client.database(database);
            let stopped = {
                let run = async {
                    // The driver refuses `killCursors` in an explicit session and
                    // sends it without one, which the server ignores for a cursor a
                    // session opened: name the session in the command itself.
                    if crate::mongo_command::command_name(&command)
                        .is_some_and(|name| name.eq_ignore_ascii_case("killCursors"))
                    {
                        let mut command = command;
                        command.insert("lsid", session_id.clone());
                        database.run_command(command).await
                    } else {
                        database.run_command(command).session(session).await
                    }
                    .map_err(|error| match error.kind.as_ref() {
                        mongodb::error::ErrorKind::Command(failure) => CommandError::Server {
                            code: failure.code,
                            code_name: failure.code_name.clone(),
                            message: failure.message.clone(),
                        },
                        _ => CommandError::Failed(error.to_string()),
                    })
                };
                tokio::pin!(run);
                tokio::select! {
                    result = &mut run => return result,
                    _ = cancelled() => CommandError::Cancelled,
                    _ = timed_out() => timeout_error(),
                }
            };
            // Stopped while this command held the session: whatever the session is
            // running is this command (even when the script gave it its own
            // `comment`), and no other command can have started, because the
            // session guard is still held. Kill it before letting the next one in.
            kill_session_operations(&self.client, &session_id).await;
            // (the guard is released as this block ends, after the kill)
            Err(stopped)
        })
    }

    fn max_wire_version(&self, control: &AggControl) -> Option<i32> {
        use std::sync::atomic::Ordering;
        match self.wire_version.load(Ordering::Relaxed) {
            0 => {}
            known => return Some(known),
        }
        let asked = crate::RUNTIME.block_on(async {
            let cancelled = async {
                match &control.cancel {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            let admin = self.client.database("admin");
            let hello = admin.run_command(bson::doc! { "hello": 1 });
            tokio::select! {
                reply = hello => reply.ok()?.get_i32("maxWireVersion").ok(),
                _ = cancelled => None,
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => None,
            }
        });
        if let Some(version) = asked {
            self.wire_version.store(version, Ordering::Relaxed);
        }
        asked
    }

    fn kill_operation(&self, comment: &str) {
        let client = self.client.clone();
        let comment = comment.to_string();
        // Detached: cancel must return immediately. Failures (missing
        // privileges, op already finished) are irrelevant: the client side
        // already stopped waiting and dropped its connection.
        crate::RUNTIME.spawn(async move {
            let admin = client.database("admin");
            let Ok(cursor) = admin
                .aggregate(vec![
                    bson::doc! { "$currentOp": { "allUsers": false } },
                    bson::doc! { "$match": { "command.comment": &comment } },
                    bson::doc! { "$project": { "opid": 1 } },
                ])
                .await
            else {
                return;
            };
            let Ok(ops) = collect_cursor(cursor, None).await else {
                return;
            };
            for op in ops {
                if let Some(opid) = op.get("opid") {
                    let _ = admin
                        .run_command(bson::doc! { "killOp": 1, "op": opid.clone() })
                        .await;
                }
            }
        });
    }
}

/// Kills what the logical session `session_id` is running (`$currentOp` by
/// `lsid`, then `killOp`), waiting at most a few seconds. The caller must hold
/// the session, so the only operation it can find is the one being cancelled.
async fn kill_session_operations(client: &mongodb::Client, session_id: &Document) {
    let Some(id) = session_id.get("id").cloned() else {
        return;
    };
    let work = async {
        let admin = client.database("admin");
        let Ok(cursor) = admin
            .aggregate(vec![
                bson::doc! { "$currentOp": { "allUsers": false } },
                bson::doc! { "$match": { "lsid.id": id } },
                bson::doc! { "$project": { "opid": 1 } },
            ])
            .await
        else {
            return;
        };
        let Ok(ops) = collect_cursor(cursor, None).await else {
            return;
        };
        for op in ops {
            if let Some(opid) = op.get("opid") {
                let _ = admin
                    .run_command(bson::doc! { "killOp": 1, "op": opid.clone() })
                    .await;
            }
        }
    };
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), work).await;
}

/// Result of one `mongo_aggregate` page.
#[derive(Debug, Clone)]
pub(crate) struct AggregateOutput {
    pub documents: Vec<Document>,
    pub total_count: u64,
    pub total_is_exact: bool,
}

#[derive(Debug)]
pub(crate) enum AggregateError {
    /// A final `$out` / `$merge` needs `confirm_write: true`.
    NeedsConfirmation(crate::aggregate::WriteStage),
    Cancelled,
    Failed(String),
}

impl AggregateError {
    fn from_backend(message: String) -> Self {
        if message == CANCELLED_MESSAGE {
            Self::Cancelled
        } else {
            Self::Failed(message)
        }
    }
}

/// Per-operation extras the FFI layer owns: the kill-op tag and the token.
#[derive(Debug, Clone, Default)]
pub(crate) struct AggControl {
    pub comment: Option<String>,
    pub cancel: Option<CancelToken>,
}

impl AggControl {
    /// Cancelled before it started (a cancel that raced the registration).
    fn already_cancelled(&self) -> Result<(), AggregateError> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            Err(AggregateError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn options(&self, request: &crate::aggregate::AggregateRequest) -> AggOpts {
        AggOpts {
            max_time_ms: request.max_time_ms,
            allow_disk_use: request.allow_disk_use,
            collation: request.collation.clone(),
            batch_size: None,
            comment: self.comment.clone(),
            cancel: self.cancel.clone(),
        }
    }
}

fn require_target(database: &str, collection: &str) -> Result<(), AggregateError> {
    if database.is_empty() {
        return Err(AggregateError::Failed(
            "MongoDB aggregation requires a database".into(),
        ));
    }
    if collection.is_empty() {
        return Err(AggregateError::Failed(
            "MongoDB aggregation requires a collection".into(),
        ));
    }
    Ok(())
}

pub(crate) struct MongoDriver {
    backend: Box<dyn MongoBackend>,
}

impl MongoDriver {
    pub(crate) fn from_backend(backend: Box<dyn MongoBackend>) -> Self {
        Self { backend }
    }

    pub(crate) fn connect(uri: &str) -> Result<(u64, Self), String> {
        crate::validate_mongo_uri(uri)?;
        let client = crate::RUNTIME
            .block_on(async {
                tokio::time::timeout(CONNECTION_TIMEOUT, async {
                    let client = mongodb::Client::with_uri_str(uri).await?;
                    client
                        .database("admin")
                        .run_command(bson::doc! { "ping": 1 })
                        .await?;
                    Ok::<_, mongodb::error::Error>(client)
                })
                .await
            })
            .map_err(|_| "MongoDB connection timed out".to_string())?
            .map_err(|error| error.to_string())?;
        let id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
        Ok((
            id,
            Self {
                backend: Box::new(MongoClientBackend {
                    client,
                    command_session: tokio::sync::Mutex::new(None),
                    wire_version: std::sync::atomic::AtomicI32::new(0),
                }),
            },
        ))
    }

    pub(crate) fn list_nav_nodes(
        &self,
        request: &NavigatorRequest,
    ) -> Result<NavigatorPage, String> {
        match (&request.parent, &request.database) {
            (None, None) => {
                let names = self.backend.list_database_names()?;
                let nodes = names
                    .into_iter()
                    .map(|name| NavigatorNode {
                        id: name.clone(),
                        kind: "database".into(),
                        label: name.clone(),
                        subtitle: None,
                        icon: None,
                        has_children: true,
                        payload: serde_json::json!({"database": name}),
                        actions: vec!["open".into()],
                    })
                    .collect();
                Ok(NavigatorPage {
                    nodes,
                    next_cursor: None,
                })
            }
            (_, Some(database)) => {
                let names = self.backend.list_collection_names(database)?;
                let nodes = names
                    .into_iter()
                    .map(|name| NavigatorNode {
                        id: name.clone(),
                        kind: "collection".into(),
                        label: name.clone(),
                        subtitle: Some(database.clone()),
                        icon: None,
                        has_children: false,
                        payload: serde_json::json!({
                            "database": database,
                            "collection": name,
                        }),
                        actions: vec!["open".into()],
                    })
                    .collect();
                Ok(NavigatorPage {
                    nodes,
                    next_cursor: None,
                })
            }
            _ => Err("navigator request must specify parent=null or database".into()),
        }
    }

    pub(crate) fn mongo_find(
        &self,
        database: &str,
        collection: &str,
        filter_json: &str,
        sort_json: &str,
        projection_json: &str,
        offset: u64,
        limit: u64,
    ) -> Result<FindDocuments, String> {
        if database.is_empty() {
            return Err("MongoDB query requires a database".into());
        }
        if collection.is_empty() {
            return Err("MongoDB query requires a collection".into());
        }
        let filter = crate::query_parser::parse_query(filter_json)
            .map_err(|error| format!("invalid MongoDB filter: {error}"))?;
        let sort = if sort_json.trim().is_empty() {
            None
        } else {
            Some(
                crate::query_parser::parse_query(sort_json)
                    .map_err(|error| format!("invalid MongoDB sort: {error}"))?,
            )
        };
        let projection = parse_mongo_projection(projection_json)?;
        self.mongo_find_documents(
            database, collection, filter, sort, projection, offset, limit,
        )
    }

    /// A find whose filter, sort and projection are already BSON (a parsed
    /// script): typed values reach the server exactly as parsed, with no text
    /// round trip in between.
    pub(crate) fn mongo_find_documents(
        &self,
        database: &str,
        collection: &str,
        filter: Document,
        sort: Option<Document>,
        projection: Option<Document>,
        offset: u64,
        limit: u64,
    ) -> Result<FindDocuments, String> {
        if database.is_empty() {
            return Err("MongoDB query requires a database".into());
        }
        if collection.is_empty() {
            return Err("MongoDB query requires a collection".into());
        }
        self.backend.find_documents(
            database,
            collection,
            filter,
            sort.unwrap_or_else(|| bson::doc! { "_id": 1 }),
            projection,
            offset,
            limit.clamp(1, 200),
        )
    }

    /// Runs one page of `mongo_aggregate` (see [`crate::aggregate`] for the
    /// preview / full semantics and the write-stage rules).
    pub(crate) fn mongo_aggregate(
        &self,
        database: &str,
        collection: &str,
        request: &crate::aggregate::AggregateRequest,
        offset: u64,
        limit: u64,
        control: &AggControl,
    ) -> Result<AggregateOutput, AggregateError> {
        use crate::aggregate::{full_pipeline, preview_pipeline, RunMode};
        require_target(database, collection)?;
        control.already_cancelled()?;
        let mut options = control.options(request);
        match request.mode {
            RunMode::Preview => {
                let pipeline =
                    preview_pipeline(&request.stages, request.stage_index, request.sample_size);
                options.batch_size = Some(request.sample_size.min(1000) as u32);
                let documents = self
                    .backend
                    .aggregate_documents(database, collection, pipeline, &options)
                    .map_err(AggregateError::from_backend)?;
                Ok(AggregateOutput {
                    total_count: documents.len() as u64,
                    // A preview is a sample, never the full count.
                    total_is_exact: false,
                    documents,
                })
            }
            RunMode::Full => {
                let limit = limit.clamp(1, 200);
                let plan = full_pipeline(&request.stages, offset, limit);
                if let Some(write) = &plan.write {
                    if !request.confirm_write {
                        return Err(AggregateError::NeedsConfirmation(write.clone()));
                    }
                }
                options.batch_size = Some(limit.saturating_add(1).min(1000) as u32);
                let mut documents = self
                    .backend
                    .aggregate_documents(database, collection, plan.pipeline, &options)
                    .map_err(AggregateError::from_backend)?;
                if let Some(write) = plan.write {
                    // `$out` / `$merge` return no documents; report what ran.
                    return Ok(AggregateOutput {
                        documents: vec![bson::doc! {
                            "stage": write.operator,
                            "target": write.target,
                            "acknowledged": true,
                        }],
                        total_count: 1,
                        total_is_exact: true,
                    });
                }
                let has_more = documents.len() as u64 > plan.page_limit;
                documents.truncate(plan.page_limit as usize);
                let total_count = offset
                    .saturating_add(documents.len() as u64)
                    .saturating_add(u64::from(has_more));
                Ok(AggregateOutput {
                    documents,
                    total_count,
                    total_is_exact: !has_more,
                })
            }
        }
    }

    /// `mongo_aggregate_explain`: the server's explain document.
    pub(crate) fn mongo_aggregate_explain(
        &self,
        database: &str,
        collection: &str,
        request: &crate::aggregate::AggregateRequest,
        verbosity: crate::aggregate::ExplainVerbosity,
        control: &AggControl,
    ) -> Result<Document, AggregateError> {
        require_target(database, collection)?;
        control.already_cancelled()?;
        let pipeline = crate::aggregate::explain_pipeline(&request.stages, verbosity);
        self.backend
            .explain_aggregate(
                database,
                collection,
                pipeline,
                verbosity,
                &control.options(request),
            )
            .map_err(AggregateError::from_backend)
    }

    /// `mongo_command`: validates the request, then runs the raw command and
    /// returns the server's reply. A server-side refusal (`ok: 0`) is the
    /// reply, as on the wire.
    pub(crate) fn mongo_command(
        &self,
        request: crate::mongo_command::CommandRequest,
        control: &AggControl,
    ) -> Result<Document, crate::mongo_command::CommandError> {
        use crate::mongo_command::{
            apply_controls, check_read_only, command_name, needs_wire_version, parse_command,
            CommandError,
        };
        if request.database.trim().is_empty() {
            return Err(CommandError::BadRequest(
                "select a database before running a command".into(),
            ));
        }
        let mut command = parse_command(request.command)?;
        if request.read_only {
            check_read_only(&command)?;
        }
        if control.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            return Err(CommandError::Cancelled);
        }
        let wire_version = command_name(&command)
            .filter(|name| needs_wire_version(name))
            .and_then(|_| self.backend.max_wire_version(control));
        apply_controls(&mut command, control.comment.as_deref(), request.max_time_ms, wire_version);
        let deadline = request
            .max_time_ms
            .map(|ms| std::time::Duration::from_millis(ms) + CLIENT_DEADLINE_GRACE);
        match self
            .backend
            .run_command(&request.database, command, control, deadline)
        {
            Err(CommandError::Server {
                code,
                code_name,
                message,
            }) => Ok(CommandError::server_reply(code, &code_name, &message)),
            other => other,
        }
    }

    pub(crate) fn kill_operation(&self, comment: &str) {
        self.backend.kill_operation(comment);
    }

    /// Export Data: the whole aggregation through one cursor into `destination`.
    /// A pipeline that writes (`$out` / `$merge`) is refused: exporting must
    /// never modify the database.
    pub(crate) fn export_aggregate(
        &self,
        database: &str,
        collection: &str,
        request: &crate::aggregate::AggregateRequest,
        format: crate::export::ExportFormat,
        destination: &std::path::Path,
        control: &AggControl,
    ) -> Result<crate::aggregate_export::ExportSummary, AggregateError> {
        require_target(database, collection)?;
        control.already_cancelled()?;
        if let Some(write) = crate::aggregate::final_write_stage(&request.stages) {
            return Err(AggregateError::Failed(format!(
                "Export Data cannot run a pipeline that writes ({} to {}).",
                write.operator, write.target
            )));
        }
        let mut options = control.options(request);
        options.batch_size = Some(1000);
        let stages = request.stages.clone();
        crate::aggregate_export::export_documents(format, destination, collection, |sink| {
            self.backend
                .aggregate_stream(database, collection, stages, &options, sink)
        })
        .map_err(AggregateError::from_backend)
    }

    /// Validates and runs one admin kind (`crate::admin::ADMIN_MUTATION_KINDS`).
    pub(crate) fn admin_mutate(
        &self,
        operation: &crate::admin::AdminMutation,
    ) -> Result<u64, String> {
        self.backend.admin_mutate(operation)
    }

    /// Raw `listIndexes` documents for `mongo_list_indexes`.
    pub(crate) fn list_indexes(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Vec<Document>, String> {
        self.backend
            .list_indexes(database, collection)
            .map(|specs| {
                specs
                    .into_iter()
                    .map(crate::admin::annotate_index_spec)
                    .collect()
            })
    }

    pub(crate) fn mutate(&self, request: &MutationRequest) -> Result<u64, MutateError> {
        if crate::admin::is_admin_mutation_kind(&request.kind) {
            let operation = crate::admin::parse_admin_mutation(request)
                .map_err(MutateError::BadRequest)?;
            return Ok(self.backend.admin_mutate(&operation)?);
        }
        if request.kind == "create_database" {
            self.create_database(request)
                .map_err(|error| MutateError::Failed(error.message().to_string()))?;
            return Ok(0);
        }
        let database = request
            .database
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MutateError::BadRequest("MongoDB mutation requires a database".into())
            })?;
        let collection = request
            .object
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                MutateError::BadRequest("MongoDB mutation requires a collection".into())
            })?;

        match request.kind.as_str() {
            "insert" => {
                let document = json_document(&request.payload, "insert document")
                    .map_err(MutateError::BadRequest)?;
                Ok(self.backend.insert_document(database, collection, document)?)
            }
            "update" => {
                let filter = mutation_identity(&request.identity)
                    .map_err(MutateError::BadRequest)?;
                let update = json_document(&request.payload, "update document")
                    .map_err(MutateError::BadRequest)?;
                match self
                    .backend
                    .update_document(database, collection, filter, update)?
                {
                    0 => Err(MutateError::NoRowsAffected(NO_DOCUMENT_MATCHED.into())),
                    matched => Ok(matched),
                }
            }
            "delete" => {
                let filter = mutation_identity(&request.identity)
                    .map_err(MutateError::BadRequest)?;
                match self.backend.delete_document(database, collection, filter)? {
                    0 => Err(MutateError::NoRowsAffected(NO_DOCUMENT_MATCHED.into())),
                    deleted => Ok(deleted),
                }
            }
            kind => Err(MutateError::BadRequest(format!(
                "unsupported MongoDB mutation kind: {kind}"
            ))),
        }
    }

    pub(crate) fn create_database(
        &self,
        request: &MutationRequest,
    ) -> Result<(), CreateDatabaseError> {
        let name = request
            .database
            .as_deref()
            .ok_or_else(|| CreateDatabaseError::Rejected("Database name is required".into()))?;
        validate_database_name(name).map_err(CreateDatabaseError::Rejected)?;
        if request.object.is_some() || !request.identity.is_null() || !request.payload.is_null() {
            return Err(CreateDatabaseError::Rejected(
                "Create database request has unexpected fields".into(),
            ));
        }
        self.backend.create_database(name)
    }
}

fn validate_database_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 63
        || name.trim() != name
        || matches!(name, "admin" | "local" | "config")
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/\\.\"$*<>:|?".contains(c))
    {
        return Err("Invalid MongoDB database name".into());
    }
    Ok(())
}

fn parse_mongo_projection(projection_json: &str) -> Result<Option<Document>, String> {
    if projection_json.trim().is_empty() {
        return Ok(None);
    }
    crate::query_parser::parse_query(projection_json)
        .map(Some)
        .map_err(|error| format!("invalid MongoDB projection: {error}"))
}

const NO_DOCUMENT_MATCHED: &str = "No document matched this _id: it was deleted, or its _id changed, since the page was read. Reload the results; the staged change was not applied.";

/// A document from Extended JSON v2 (canonical on the app seam). Every typed
/// value keeps its BSON type; a plain JSON number is an Int32, Int64 or Double
/// by its form.
fn json_document(value: &serde_json::Value, label: &str) -> Result<Document, String> {
    match bson::Bson::try_from(value.clone()) {
        Ok(bson::Bson::Document(document)) => Ok(document),
        Ok(_) => Err(format!("invalid {label}: expected a document")),
        Err(error) => Err(format!("invalid {label}: {error}")),
    }
}

/// The filter of a single-document write: exactly `{ "_id": <the document's
/// _id> }`. Anything else (another field, a query operator, a regex) could
/// address a different document and is refused.
fn mutation_identity(value: &serde_json::Value) -> Result<Document, String> {
    let document = json_document(value, "mutation identity")?;
    if document.is_empty() {
        return Err("MongoDB mutation identity cannot be empty".into());
    }
    let Some(id) = document.get("_id").filter(|_| document.len() == 1) else {
        return Err("MongoDB mutation identity must be exactly { _id: <the document's _id> }".into());
    };
    if !crate::models::id_can_identify(id) {
        return Err("this _id cannot identify a single document".into());
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockBackend {
        databases: Vec<String>,
        created: std::sync::Arc<Mutex<Vec<String>>>,
        collections: HashMap<String, Vec<String>>,
        find_result: Vec<Document>,
        find_calls: std::sync::Arc<
            Mutex<
                Vec<(
                    String,
                    String,
                    Document,
                    Document,
                    u64,
                    u64,
                    Option<Document>,
                )>,
            >,
        >,
        aggregate_result: Result<Vec<Document>, String>,
        explain_result: Result<Document, String>,
        /// Rows an update / delete reports (the defaults are 2 and 3).
        affected: Option<u64>,
        /// `(filter, update-or-document)` of every write.
        write_calls: std::sync::Arc<Mutex<Vec<(Document, Document)>>>,
        aggregate_calls: AggregateCalls,
        explain_calls: ExplainCalls,
    }

    type AggregateCalls = std::sync::Arc<Mutex<Vec<(String, String, Vec<Document>, AggOpts)>>>;
    type ExplainCalls =
        std::sync::Arc<Mutex<Vec<(Vec<Document>, crate::aggregate::ExplainVerbosity)>>>;

    impl MockBackend {
        fn new(databases: Vec<String>, collections: Vec<(&str, Vec<String>)>) -> Self {
            Self {
                databases,
                created: std::sync::Arc::new(Mutex::new(Vec::new())),
                collections: collections
                    .into_iter()
                    .map(|(k, v)| (k.into(), v))
                    .collect(),
                find_result: Vec::new(),
                find_calls: std::sync::Arc::new(Mutex::new(Vec::new())),
                aggregate_result: Ok(Vec::new()),
                explain_result: Ok(Document::new()),
                affected: None,
                write_calls: std::sync::Arc::new(Mutex::new(Vec::new())),
                aggregate_calls: std::sync::Arc::new(Mutex::new(Vec::new())),
                explain_calls: std::sync::Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_aggregate_result(mut self, result: Result<Vec<Document>, String>) -> Self {
            self.aggregate_result = result;
            self
        }

        fn with_affected(mut self, affected: u64) -> Self {
            self.affected = Some(affected);
            self
        }

        fn with_find_result(mut self, result: Vec<Document>) -> Self {
            self.find_result = result;
            self
        }
    }

    impl MongoBackend for MockBackend {
        fn list_database_names(&self) -> Result<Vec<String>, String> {
            Ok(self.databases.clone())
        }
        fn create_database(&self, name: &str) -> Result<(), CreateDatabaseError> {
            self.created.lock().unwrap().push(name.into());
            Ok(())
        }
        fn list_collection_names(&self, database: &str) -> Result<Vec<String>, String> {
            Ok(self.collections.get(database).cloned().unwrap_or_default())
        }

        fn find_documents(
            &self,
            database: &str,
            collection: &str,
            filter: Document,
            sort: Document,
            projection: Option<Document>,
            offset: u64,
            limit: u64,
        ) -> Result<FindDocuments, String> {
            self.find_calls.lock().expect("find calls lock").push((
                database.into(),
                collection.into(),
                filter,
                sort,
                offset,
                limit,
                projection,
            ));
            Ok(FindDocuments {
                documents: self.find_result.clone(),
                total_count: self.find_result.len() as u64,
                total_is_exact: true,
            })
        }

        fn aggregate_documents(
            &self,
            database: &str,
            collection: &str,
            pipeline: Vec<Document>,
            options: &AggOpts,
        ) -> Result<Vec<Document>, String> {
            self.aggregate_calls.lock().unwrap().push((
                database.into(),
                collection.into(),
                pipeline,
                options.clone(),
            ));
            self.aggregate_result.clone()
        }

        fn explain_aggregate(
            &self,
            _database: &str,
            _collection: &str,
            pipeline: Vec<Document>,
            verbosity: crate::aggregate::ExplainVerbosity,
            _options: &AggOpts,
        ) -> Result<Document, String> {
            self.explain_calls.lock().unwrap().push((pipeline, verbosity));
            self.explain_result.clone()
        }

        fn insert_document(
            &self,
            _database: &str,
            _collection: &str,
            document: Document,
        ) -> Result<u64, String> {
            self.write_calls
                .lock()
                .unwrap()
                .push((Document::new(), document));
            Ok(1)
        }

        fn update_document(
            &self,
            _database: &str,
            _collection: &str,
            filter: Document,
            update: Document,
        ) -> Result<u64, String> {
            self.write_calls.lock().unwrap().push((filter, update));
            Ok(self.affected.unwrap_or(2))
        }

        fn delete_document(
            &self,
            _database: &str,
            _collection: &str,
            filter: Document,
        ) -> Result<u64, String> {
            self.write_calls
                .lock()
                .unwrap()
                .push((filter, Document::new()));
            Ok(self.affected.unwrap_or(3))
        }
    }

    fn root_request() -> NavigatorRequest {
        NavigatorRequest {
            parent: None,
            database: None,
            payload: None,
            cursor: None,
            limit: 100,
        }
    }

    fn db_request(database: &str) -> NavigatorRequest {
        NavigatorRequest {
            parent: Some(format!("database:{database}")),
            database: Some(database.into()),
            payload: None,
            cursor: None,
            limit: 100,
        }
    }

    #[test]
    fn connect_rejects_non_mongo_uri() {
        let result = MongoDriver::connect("postgresql://localhost/app");
        assert!(result.is_err());
        assert!(result.err().unwrap().contains("mongodb"));
    }

    #[test]
    fn connect_rejects_an_unreachable_mongodb_server() {
        let result = MongoDriver::connect(
            "mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=10&connectTimeoutMS=10",
        );
        assert!(result.is_err(), "unreachable MongoDB server was accepted");
    }

    #[test]
    fn validate_uri_accepts_both_mongo_schemes() {
        assert!(crate::validate_mongo_uri("mongodb://localhost:27017").is_ok());
        assert!(crate::validate_mongo_uri("mongodb+srv://cluster.example.com").is_ok());
    }

    #[test]
    fn list_nav_nodes_at_root_returns_databases() {
        let backend = Box::new(MockBackend::new(vec!["app".into(), "auth".into()], vec![]));
        let driver = MongoDriver::from_backend(backend);

        let page = driver.list_nav_nodes(&root_request()).expect("root nav");

        assert_eq!(page.nodes.len(), 2);
        assert_eq!(page.nodes[0].kind, "database");
        assert_eq!(page.nodes[0].label, "app");
        assert!(page.nodes[0].has_children);
        assert_eq!(
            page.nodes[0].payload,
            serde_json::json!({"database": "app"})
        );
        assert!(page.next_cursor.is_none());
    }

    #[test]
    fn mutates_using_extended_json_identity() {
        let backend = Box::new(MockBackend::new(vec![], vec![]));
        let driver = MongoDriver::from_backend(backend);
        let request = MutationRequest {
            operation_id: engine_driver_api::OperationId(1),
            kind: "update".into(),
            database: Some("app".into()),
            object: Some("invoices".into()),
            identity: serde_json::json!({"_id": {"$oid": "66d4f84d0000000000000003"}}),
            payload: serde_json::json!({"$set": {"name": "updated"}}),
        };

        assert_eq!(driver.mutate(&request).expect("update mutation"), 2);
    }

    fn write(kind: &str, identity: serde_json::Value, payload: serde_json::Value) -> MutationRequest {
        MutationRequest {
            operation_id: engine_driver_api::OperationId(1),
            kind: kind.into(),
            database: Some("app".into()),
            object: Some("invoices".into()),
            identity,
            payload,
        }
    }

    #[test]
    fn a_write_that_matches_nothing_is_a_typed_error() {
        let backend = MockBackend::new(vec![], vec![]).with_affected(0);
        let driver = MongoDriver::from_backend(Box::new(backend));
        let identity = serde_json::json!({"_id": {"$numberInt": "1"}});
        for request in [
            write("update", identity.clone(), serde_json::json!({"$set": {"a": 1}})),
            write("delete", identity.clone(), serde_json::json!({})),
        ] {
            let error = driver.mutate(&request).expect_err("nothing matched");
            assert!(matches!(error, MutateError::NoRowsAffected(_)), "{error:?}");
            assert_eq!(error.code(), "mutation.no_rows_affected");
            assert!(error.message().contains("staged change was not applied"));
        }
    }

    #[test]
    fn the_write_filter_is_only_ever_the_documents_id() {
        let backend = MockBackend::new(vec![], vec![]);
        let writes = backend.write_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let payload = serde_json::json!({"$set": {"a": 1}});
        for identity in [
            serde_json::json!({"rowIndex": 3}),
            serde_json::json!({"_id": 1, "name": "x"}),
            serde_json::json!({"name": "x"}),
            serde_json::json!({"_id": {"$ne": 1}}),
            serde_json::json!({"_id": {"$regularExpression": {"pattern": ".*", "options": ""}}}),
            serde_json::json!({"_id": [1]}),
            serde_json::Value::Null,
        ] {
            for kind in ["update", "delete"] {
                let error = driver
                    .mutate(&write(kind, identity.clone(), payload.clone()))
                    .expect_err(&format!("{kind} {identity}"));
                assert_eq!(error.code(), "mutation.bad_request", "{identity}");
            }
        }
        assert!(writes.lock().unwrap().is_empty(), "nothing reached the backend");
    }

    #[test]
    fn canonical_values_reach_the_backend_with_their_bson_types() {
        let backend = MockBackend::new(vec![], vec![]);
        let writes = backend.write_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let identity = serde_json::json!({"_id": {"$numberDouble": "NaN"}});
        let value = serde_json::json!({
            "long": {"$numberLong": "5"}, "neg": {"$numberInt": "-5"}, "nan": {"$numberDouble": "NaN"},
            "date": {"$date": {"$numberLong": "5"}}, "nested": {"k": {"$numberLong": "1"}, "a": [{"$numberDouble": "2.0"}]},
        });
        driver
            .mutate(&write("update", identity, serde_json::json!({"$set": {"f": value.clone()}})))
            .unwrap();
        driver.mutate(&write("insert", serde_json::Value::Null, value)).unwrap();
        let writes = writes.lock().unwrap();
        let id = writes[0].0.get_f64("_id").unwrap();
        assert!(id.is_nan(), "_id NaN stays NaN, not null");
        let set = writes[0].1.get_document("$set").unwrap().get_document("f").unwrap();
        for document in [set, &writes[1].1] {
            assert_eq!(document.get("long"), Some(&bson::Bson::Int64(5)));
            assert_eq!(document.get("neg"), Some(&bson::Bson::Int32(-5)));
            assert!(document.get_f64("nan").unwrap().is_nan());
            assert!(matches!(document.get("date"), Some(bson::Bson::DateTime(_))));
            let nested = document.get_document("nested").unwrap();
            assert_eq!(nested.get("k"), Some(&bson::Bson::Int64(1)));
            assert_eq!(nested.get_array("a").unwrap()[0], bson::Bson::Double(2.0));
        }
    }

    #[test]
    fn script_find_documents_reach_the_backend_without_a_text_round_trip() {
        let backend = MockBackend::new(vec!["app".into()], vec![]);
        let calls = backend.find_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let filter = bson::doc! { "at": { "$gte": bson::DateTime::from_millis(1_725_148_800_000) } };
        driver
            .mongo_find_documents("app", "users", filter.clone(), None, None, 0, 20)
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].2, filter);
        assert_eq!(calls[0].3, bson::doc! { "_id": 1 });
    }

    #[test]
    fn create_database_is_validated_and_uses_existing_backend() {
        let backend = Box::new(MockBackend::new(vec![], vec![]));
        let calls = backend.created.clone();
        let driver = MongoDriver::from_backend(backend);
        let request = MutationRequest {
            operation_id: engine_driver_api::OperationId(7),
            kind: "create_database".into(),
            database: Some("reports".into()),
            object: None,
            identity: serde_json::Value::Null,
            payload: serde_json::Value::Null,
        };
        assert_eq!(driver.mutate(&request).unwrap(), 0);
        assert_eq!(*calls.lock().unwrap(), vec!["reports".to_string()]);
        for name in [
            "",
            "admin",
            "local",
            "config",
            "bad/name",
            "bad\0name",
            "x.y",
            "bad name",
        ] {
            let mut bad = request.clone();
            bad.database = Some(name.into());
            assert!(driver.mutate(&bad).is_err());
        }
        assert_eq!(calls.lock().unwrap().len(), 1);
        let mut unknown = request;
        unknown.kind = "new_future_operation".into();
        assert!(driver.mutate(&unknown).is_err());
    }

    #[test]
    fn rejects_empty_identity_for_destructive_mutations() {
        let backend = Box::new(MockBackend::new(vec![], vec![]));
        let driver = MongoDriver::from_backend(backend);
        let request = MutationRequest {
            operation_id: engine_driver_api::OperationId(1),
            kind: "delete".into(),
            database: Some("app".into()),
            object: Some("invoices".into()),
            identity: serde_json::json!({}),
            payload: serde_json::json!({}),
        };

        assert!(driver.mutate(&request).is_err());
    }

    #[test]
    fn list_nav_nodes_for_database_returns_collections() {
        let backend = Box::new(MockBackend::new(
            vec!["app".into()],
            vec![("app", vec!["users".into(), "invoices".into()])],
        ));
        let driver = MongoDriver::from_backend(backend);

        let page = driver
            .list_nav_nodes(&db_request("app"))
            .expect("database nav");

        assert_eq!(page.nodes.len(), 2);
        assert_eq!(page.nodes[0].kind, "collection");
        assert_eq!(page.nodes[0].label, "users");
        assert_eq!(page.nodes[0].subtitle.as_deref(), Some("app"));
        assert_eq!(
            page.nodes[0].payload,
            serde_json::json!({"database": "app", "collection": "users"})
        );
        assert_eq!(page.nodes[0].actions, vec!["open"]);
        assert!(!page.nodes[0].has_children);
    }

    #[test]
    fn list_nav_nodes_for_empty_database_returns_no_nodes() {
        let backend = Box::new(MockBackend::new(vec!["app".into()], vec![]));
        let driver = MongoDriver::from_backend(backend);

        let page = driver
            .list_nav_nodes(&db_request("app"))
            .expect("empty database nav");

        assert!(page.nodes.is_empty());
    }

    #[test]
    fn mongo_find_parses_filter_and_clamps_page_limit() {
        let backend = Box::new(
            MockBackend::new(vec!["app".into()], vec![])
                .with_find_result(vec![bson::doc! { "name": "Ada" }]),
        );
        let calls = backend.find_calls.clone();
        let driver = MongoDriver::from_backend(backend);

        let page = driver
            .mongo_find("app", "users", r#"{active: true}"#, "", "", 17, 999)
            .expect("find page");

        assert_eq!(page.documents.len(), 1);
        assert_eq!(page.total_count, 1);
        let calls = calls.lock().expect("find calls lock");
        assert_eq!(calls[0].2, bson::doc! { "active": true });
        assert_eq!(calls[0].3, bson::doc! { "_id": 1 });
        assert_eq!(calls[0].4, 17);
        assert_eq!(calls[0].5, 200);
    }

    #[test]
    fn mongo_find_parses_explicit_sort() {
        let backend = Box::new(MockBackend::new(vec!["app".into()], vec![]));
        let calls = backend.find_calls.clone();
        let driver = MongoDriver::from_backend(backend);

        driver
            .mongo_find("app", "users", "{}", r#"{createdAt: -1}"#, "", 0, 20)
            .expect("find page");

        let calls = calls.lock().expect("find calls lock");
        assert_eq!(calls[0].3, bson::doc! { "createdAt": -1 });
    }

    #[test]
    fn mongo_find_passes_explicit_projection() {
        let backend = Box::new(MockBackend::new(vec!["app".into()], vec![]));
        let calls = backend.find_calls.clone();
        let driver = MongoDriver::from_backend(backend);

        driver
            .mongo_find("app", "users", "{}", "", r#"{name: 1}"#, 0, 20)
            .expect("find page");

        let calls = calls.lock().expect("find calls lock");
        assert_eq!(calls[0].6, Some(bson::doc! { "name": 1 }));
    }

    #[test]
    fn mongo_find_rejects_invalid_sort() {
        let backend = Box::new(MockBackend::new(vec!["app".into()], vec![]));
        let driver = MongoDriver::from_backend(backend);

        let error = driver
            .mongo_find("app", "users", "{}", "not-json", "", 0, 20)
            .expect_err("invalid sort");
        assert!(error.contains("invalid MongoDB sort"));
    }

    #[test]
    fn mongo_find_rejects_invalid_projection() {
        let driver =
            MongoDriver::from_backend(Box::new(MockBackend::new(vec!["app".into()], vec![])));

        let error = driver
            .mongo_find("app", "users", "{}", "", "not-json", 0, 20)
            .expect_err("invalid projection");

        assert!(error.contains("invalid MongoDB projection"));
    }

    #[test]
    fn mongo_find_rejects_invalid_filter_and_missing_target() {
        let driver =
            MongoDriver::from_backend(Box::new(MockBackend::new(vec!["app".into()], vec![])));

        assert!(driver
            .mongo_find("app", "users", "not-json", "", "", 0, 20)
            .expect_err("invalid filter")
            .contains("invalid MongoDB filter"));
        assert!(driver
            .mongo_find("", "users", "{}", "", "", 0, 20)
            .expect_err("missing database")
            .contains("requires a database"));
        assert!(driver
            .mongo_find("app", "", "{}", "", "", 0, 20)
            .expect_err("missing collection")
            .contains("requires a collection"));
    }

    // ---- aggregation -------------------------------------------------

    use crate::aggregate::{parse_request_payload, ExplainVerbosity};
    use serde_json::json;

    fn agg_request(payload: serde_json::Value) -> crate::aggregate::AggregateRequest {
        parse_request_payload(&payload).expect("valid aggregate payload")
    }

    fn numbered(count: u64) -> Vec<Document> {
        (0..count).map(|index| bson::doc! { "_id": index as i64 }).collect()
    }

    #[test]
    fn aggregate_preview_caps_strips_writes_and_is_a_single_inexact_page() {
        let backend = MockBackend::new(vec![], vec![]).with_aggregate_result(Ok(numbered(3)));
        let calls = backend.aggregate_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let request = agg_request(json!({
            "pipeline": "[{ $match: { a: 1 } }, { $limit: 1000000 }, { $out: 'copy' }]",
            "mode": "preview", "sample_size": 5,
        }));
        let page = driver
            .mongo_aggregate("app", "orders", &request, 999, 50, &AggControl::default())
            .expect("preview");
        assert_eq!(page.documents.len(), 3);
        assert!(!page.total_is_exact, "a sample never claims an exact total");
        assert_eq!(page.total_count, 3);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0].2,
            vec![
                bson::doc! { "$match": { "a": 1 } },
                bson::doc! { "$limit": 1_000_000 },
                bson::doc! { "$limit": 5_i64 },
            ],
            "$out stripped, preview cap always appended even after a user $limit"
        );
        assert_eq!(calls[0].3.max_time_ms, Some(crate::aggregate::DEFAULT_PREVIEW_MAX_TIME_MS));
        assert_eq!(calls[0].3.batch_size, Some(5));
    }

    #[test]
    fn aggregate_per_stage_preview_truncates_and_never_writes() {
        let backend = MockBackend::new(vec![], vec![]);
        let calls = backend.aggregate_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let request = agg_request(json!({
            "pipeline": "[{ $match: {} }, { $sort: { a: 1 } }, { $merge: { into: 'x' } }]",
            "mode": "preview", "stage_index": 2, "sample_size": 20,
        }));
        driver
            .mongo_aggregate("app", "orders", &request, 0, 50, &AggControl::default())
            .unwrap();
        let pipeline = calls.lock().unwrap()[0].2.clone();
        assert_eq!(pipeline.len(), 3, "{pipeline:?}");
        assert!(pipeline.iter().all(|stage| stage.keys().next().unwrap() != "$merge"));
    }

    #[test]
    fn aggregate_full_pages_with_skip_limit_and_detects_the_next_page() {
        let backend = MockBackend::new(vec![], vec![]).with_aggregate_result(Ok(numbered(21)));
        let calls = backend.aggregate_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let request = agg_request(json!({ "pipeline": "[{ $match: {} }]" }));
        let page = driver
            .mongo_aggregate("app", "orders", &request, 40, 20, &AggControl::default())
            .unwrap();
        assert_eq!(page.documents.len(), 20, "extra probe row is dropped");
        assert_eq!((page.total_count, page.total_is_exact), (61, false));
        assert_eq!(
            calls.lock().unwrap()[0].2[1..],
            [bson::doc! { "$skip": 40_i64 }, bson::doc! { "$limit": 21_i64 }]
        );

        let backend = MockBackend::new(vec![], vec![]).with_aggregate_result(Ok(numbered(5)));
        let driver = MongoDriver::from_backend(Box::new(backend));
        let last = driver
            .mongo_aggregate("app", "orders", &request, 40, 20, &AggControl::default())
            .unwrap();
        assert_eq!((last.total_count, last.total_is_exact), (45, true));
    }

    #[test]
    fn aggregate_full_write_pipeline_requires_confirmation_and_gets_no_window() {
        let backend = MockBackend::new(vec![], vec![]);
        let calls = backend.aggregate_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let text = "[{ $match: {} }, { $out: 'copy' }]";
        let unconfirmed = driver
            .mongo_aggregate(
                "app", "orders", &agg_request(json!({ "pipeline": text })), 0, 20,
                &AggControl::default(),
            )
            .unwrap_err();
        assert!(
            matches!(&unconfirmed, AggregateError::NeedsConfirmation(write) if write.operator == "$out" && write.target == "copy"),
            "{unconfirmed:?}"
        );
        assert!(calls.lock().unwrap().is_empty(), "nothing may reach the server unconfirmed");
        let page = driver
            .mongo_aggregate(
                "app", "orders", &agg_request(json!({ "pipeline": text, "confirm_write": true })),
                60, 20, &AggControl::default(),
            )
            .unwrap();
        assert_eq!(calls.lock().unwrap()[0].2.len(), 2, "no $skip/$limit after $out");
        assert_eq!(page.documents[0].get_str("stage").unwrap(), "$out");
        assert_eq!(page.documents[0].get_str("target").unwrap(), "copy");
        assert!(page.total_is_exact);
    }

    #[test]
    fn aggregate_errors_propagate_instead_of_returning_partial_results() {
        let driver = MongoDriver::from_backend(Box::new(
            MockBackend::new(vec![], vec![])
                .with_aggregate_result(Err("cursor error: connection reset".into())),
        ));
        let request = agg_request(json!({ "pipeline": "[]" }));
        let error = driver
            .mongo_aggregate("app", "orders", &request, 0, 20, &AggControl::default())
            .unwrap_err();
        assert!(matches!(&error, AggregateError::Failed(message) if message.contains("connection reset")), "{error:?}");
        let cancelled = MongoDriver::from_backend(Box::new(
            MockBackend::new(vec![], vec![])
                .with_aggregate_result(Err(CANCELLED_MESSAGE.into())),
        ))
        .mongo_aggregate("app", "orders", &request, 0, 20, &AggControl::default())
        .unwrap_err();
        assert!(matches!(cancelled, AggregateError::Cancelled));
        assert!(matches!(
            driver.mongo_aggregate("", "orders", &request, 0, 20, &AggControl::default()),
            Err(AggregateError::Failed(_))
        ));
        assert!(matches!(
            driver.mongo_aggregate("app", "", &request, 0, 20, &AggControl::default()),
            Err(AggregateError::Failed(_))
        ));
    }

    #[test]
    fn aggregate_explain_strips_write_stages_only_when_executing() {
        let mut backend = MockBackend::new(vec![], vec![]);
        backend.explain_result = Ok(bson::doc! { "queryPlanner": { "winningPlan": 1 } });
        let calls = backend.explain_calls.clone();
        let driver = MongoDriver::from_backend(Box::new(backend));
        let request = agg_request(json!({ "pipeline": "[{ $match: {} }, { $out: 'copy' }]" }));
        let plan = driver
            .mongo_aggregate_explain(
                "app", "orders", &request, ExplainVerbosity::QueryPlanner, &AggControl::default(),
            )
            .unwrap();
        assert!(plan.contains_key("queryPlanner"));
        driver
            .mongo_aggregate_explain(
                "app", "orders", &request, ExplainVerbosity::ExecutionStats, &AggControl::default(),
            )
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls[0].0.len(), 2, "queryPlanner never runs $out, keep it in the plan");
        assert_eq!(calls[1].0, vec![bson::doc! { "$match": {} }], "executionStats must not run $out");
    }

    #[test]
    fn collect_cursor_fails_on_a_mid_stream_error_and_checks_cancellation() {
        let ok = |id: i32| Ok::<Document, String>(bson::doc! { "_id": id });
        let all_good = crate::RUNTIME
            .block_on(collect_cursor(futures::stream::iter(vec![ok(1), ok(2)]), None))
            .unwrap();
        assert_eq!(all_good.len(), 2);

        // The old `.filter_map(Result::ok)` returned [1, 3] here.
        let error = crate::RUNTIME
            .block_on(collect_cursor(
                futures::stream::iter(vec![ok(1), Err("boom: network".to_string()), ok(3)]),
                None,
            ))
            .unwrap_err();
        assert!(error.contains("boom: network"), "{error}");

        let token = CancelToken::default();
        token.cancel();
        let cancelled = crate::RUNTIME
            .block_on(collect_cursor(futures::stream::iter(vec![ok(1)]), Some(&token)))
            .unwrap_err();
        assert_eq!(cancelled, CANCELLED_MESSAGE);
    }

    #[test]
    fn run_controlled_honours_cancel_and_deadline() {
        let token = CancelToken::default();
        let waiter = token.clone();
        let result = crate::RUNTIME.block_on(async {
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                waiter.cancel();
            });
            let result = run_controlled(
                std::future::pending::<Result<(), String>>(),
                Some(&token),
                None,
            )
            .await;
            canceller.await.unwrap();
            result
        });
        assert_eq!(result.unwrap_err(), CANCELLED_MESSAGE);

        let timed_out = crate::RUNTIME.block_on(run_controlled(
            std::future::pending::<Result<(), String>>(),
            None,
            Some(std::time::Duration::from_millis(10)),
        ));
        assert!(timed_out.unwrap_err().contains("timed out"));

        let done = crate::RUNTIME.block_on(run_controlled(async { Ok(7) }, None, None));
        assert_eq!(done.unwrap(), 7);

        // A token cancelled before the call must not be lost.
        let pre = CancelToken::default();
        pre.cancel();
        let early = crate::RUNTIME.block_on(run_controlled(
            std::future::pending::<Result<(), String>>(),
            Some(&pre),
            None,
        ));
        assert_eq!(early.unwrap_err(), CANCELLED_MESSAGE);
    }

    fn target_exists_named(driver: &MongoDriver, database: &str, name: &str) -> bool {
        driver
            .backend
            .list_collection_names(database)
            .unwrap()
            .iter()
            .any(|existing| existing == name)
    }

    /// End-to-end against a real server. Opt-in: set `TABLEX_TEST_MONGO_URI`
    /// (for example `mongodb://127.0.0.1:27999`); without it the test is a
    /// no-op so `cargo test` stays hermetic.
    #[test]
    fn live_server_aggregate_roundtrip() {
        let Ok(uri) = std::env::var("TABLEX_TEST_MONGO_URI") else {
            eprintln!("live_server_aggregate_roundtrip: TABLEX_TEST_MONGO_URI not set, skipped");
            return;
        };
        let (_, driver) = MongoDriver::connect(&uri).expect("connect to test server");
        let db = format!("tablex_agg_it_{}", std::process::id());
        let coll = "items";
        let insert = |value: serde_json::Value| {
            driver
                .mutate(&MutationRequest {
                    operation_id: engine_driver_api::OperationId(1),
                    kind: "insert".into(),
                    database: Some(db.clone()),
                    object: Some(coll.into()),
                    identity: serde_json::Value::Null,
                    payload: value,
                })
                .expect("insert");
        };
        for id in 0..50 {
            insert(json!({ "_id": id, "grp": id % 5, "name": format!("n{id}") }));
        }
        let request = |payload: serde_json::Value| agg_request(payload);
        let control = AggControl::default();

        // Fix 2: a user $limit mid-pipeline must not disable the preview cap.
        let preview = driver
            .mongo_aggregate(
                &db, coll,
                &request(json!({ "pipeline": "[{ $sort: { _id: 1 } }, { $limit: 1000000 }]", "mode": "preview", "sample_size": 5 })),
                0, 50, &control,
            )
            .expect("preview");
        assert_eq!(preview.documents.len(), 5);

        // Compass-style paging.
        let pipeline = "[{ $sort: { _id: 1 } }]";
        let first = driver
            .mongo_aggregate(&db, coll, &request(json!({ "pipeline": pipeline })), 0, 20, &control)
            .unwrap();
        assert_eq!((first.documents.len(), first.total_count, first.total_is_exact), (20, 21, false));
        let last = driver
            .mongo_aggregate(&db, coll, &request(json!({ "pipeline": pipeline })), 40, 20, &control)
            .unwrap();
        assert_eq!((last.documents.len(), last.total_count, last.total_is_exact), (10, 50, true));
        assert_eq!(last.documents[0].get_i32("_id").unwrap(), 40);

        // Fix 1: a cursor error after the first batch must fail the call.
        let failing = driver.backend.aggregate_documents(
            &db, coll,
            vec![
                bson::doc! { "$sort": { "_id": 1 } },
                bson::doc! { "$project": { "v": { "$divide": ["$_id", { "$subtract": ["$_id", 10] }] } } },
            ],
            &AggOpts { batch_size: Some(1), ..AggOpts::default() },
        );
        let message = failing.expect_err("divide-by-zero mid-stream must not be swallowed");
        assert!(message.to_lowercase().contains("zero") || message.contains("cursor"), "{message}");

        // Fix 3: previews / executionStats never write; a full run needs confirmation.
        let out_pipeline = "[{ $sort: { _id: 1 } }, { $out: 'out_copy' }]";
        let target_exists = || {
            driver
                .backend
                .list_collection_names(&db)
                .unwrap()
                .iter()
                .any(|name| name == "out_copy")
        };
        driver
            .mongo_aggregate(&db, coll, &request(json!({ "pipeline": out_pipeline, "mode": "preview" })), 0, 20, &control)
            .unwrap();
        driver
            .mongo_aggregate_explain(
                &db, coll, &request(json!({ "pipeline": out_pipeline })),
                ExplainVerbosity::ExecutionStats, &control,
            )
            .unwrap();
        assert!(!target_exists(), "preview / executionStats explain must not create the $out target");
        let plan = driver
            .mongo_aggregate_explain(
                &db, coll, &request(json!({ "pipeline": out_pipeline })),
                ExplainVerbosity::QueryPlanner, &control,
            )
            .unwrap();
        assert!(plan.contains_key("stages") || plan.contains_key("queryPlanner"), "{plan:?}");
        assert!(!target_exists(), "queryPlanner explain does not run $out either");
        assert!(matches!(
            driver.mongo_aggregate(&db, coll, &request(json!({ "pipeline": out_pipeline })), 0, 20, &control),
            Err(AggregateError::NeedsConfirmation(_))
        ));
        assert!(!target_exists());
        driver
            .mongo_aggregate(&db, coll, &request(json!({ "pipeline": out_pipeline, "confirm_write": true })), 0, 20, &control)
            .unwrap();
        assert!(target_exists(), "confirmed run writes");
        let copied = driver
            .mongo_aggregate(&db, "out_copy", &request(json!({ "pipeline": "[]" })), 0, 200, &control)
            .unwrap();
        assert_eq!(copied.documents.len(), 50);

        // Export Data: one cursor, every document, errors surfaced.
        let dir = std::env::temp_dir().join(format!("tablex-live-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let csv = dir.join("live.csv");
        let summary = driver
            .export_aggregate(
                &db, coll,
                &request(json!({ "pipeline": "[{ $sort: { _id: 1 } }]" })),
                crate::export::ExportFormat::Csv, &csv, &control,
            )
            .expect("export");
        assert_eq!(summary.rows, 50);
        let text = std::fs::read_to_string(&csv).unwrap();
        assert_eq!(text.lines().count(), 51);
        assert_eq!(text.lines().next().unwrap(), "_id,grp,name");
        let broken = dir.join("broken.csv");
        let failure = driver.export_aggregate(
            &db, coll,
            &request(json!({ "pipeline": "[{ $project: { v: { $divide: [1, { $subtract: ['$_id', '$_id'] }] } } }]" })),
            crate::export::ExportFormat::Csv, &broken, &control,
        );
        assert!(matches!(failure, Err(AggregateError::Failed(_))), "{failure:?}");
        assert!(!broken.exists(), "a failed export leaves no file");
        assert!(matches!(
            driver.export_aggregate(
                &db, coll, &request(json!({ "pipeline": "[{ $out: 'nope' }]" })),
                crate::export::ExportFormat::Csv, &dir.join("w.csv"), &control,
            ),
            Err(AggregateError::Failed(_))
        ));
        assert!(!target_exists_named(&driver, &db, "nope"), "export never writes");
        let _ = std::fs::remove_dir_all(&dir);

        // Per-stage preview truncates before the group.
        let grouped = driver
            .mongo_aggregate(
                &db, coll,
                &request(json!({ "pipeline": "[{ $group: { _id: '$grp', n: { $sum: 1 } } }, { $sort: { _id: 1 } }]", "mode": "preview", "stage_index": 0 })),
                0, 20, &control,
            )
            .unwrap();
        assert_eq!(grouped.documents.len(), 5);

        // Cancellation: a slow server-side pipeline stops within a second and
        // the tagged operation is killed on the server.
        let token = CancelToken::default();
        let comment = format!("tablex:it:{}", std::process::id());
        let slow_control = AggControl { comment: Some(comment.clone()), cancel: Some(token.clone()) };
        let slow = request(json!({
            "pipeline": "[{ $addFields: { s: { $function: { body: 'function() { sleep(2000); return 1 }', args: [], lang: 'js' } } } }]",
            "mode": "preview", "sample_size": 50,
        }));
        let started = std::time::Instant::now();
        let canceller = {
            let token = token.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(400));
                token.cancel();
            })
        };
        let result = driver.mongo_aggregate(&db, coll, &slow, 0, 20, &slow_control);
        canceller.join().unwrap();
        assert!(matches!(result, Err(AggregateError::Cancelled)), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_millis(1500), "{:?}", started.elapsed());
        driver.kill_operation(&comment);
        let client = crate::RUNTIME
            .block_on(mongodb::Client::with_uri_str(&uri))
            .expect("second client");
        let still_running = || {
            crate::RUNTIME
                .block_on(async {
                    client
                        .database("admin")
                        .run_command(bson::doc! { "currentOp": 1, "command.comment": &comment })
                        .await
                })
                .map(|reply| reply.get_array("inprog").map(|ops| ops.len()).unwrap_or(0))
                .unwrap_or(0)
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while still_running() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert_eq!(still_running(), 0, "the cancelled operation must not keep running on the server");

        driver
            .admin_mutate(&crate::admin::AdminMutation::DropDatabase { database: db })
            .expect("cleanup");
    }

    #[test]
    fn parses_mongo_projection_document() {
        let projection = parse_mongo_projection(r#"{name: 1, email: 1}"#)
            .expect("projection should parse")
            .expect("projection should be present");

        assert_eq!(projection, bson::doc! { "name": 1, "email": 1 });
    }
}
