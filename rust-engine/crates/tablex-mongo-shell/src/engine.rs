//! One script run: a fresh QuickJS runtime, the JavaScript shell surface on
//! top of it, and the natives that reach the [`CommandPort`].

use crate::output::Output;
use crate::port::{CommandError, CommandPort};
use engine_driver_api::mongo_shell::{
    FailureKind, ShellFailure, ShellReply, ShellRequest, ShellResult,
};
use rquickjs::context::EvalOptions;
use rquickjs::function::Func;
use rquickjs::{Coerced, Context, Ctx, FromJs, Function, Object, Runtime, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// QuickJS heap limit for one run.
pub const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// QuickJS stack limit. The executable runs scripts on a thread with a much
/// larger stack; 1 MiB also fits the 2 MiB threads the tests use.
pub const MAX_STACK_BYTES: usize = 1024 * 1024;
/// `toArray()` refuses to hold more documents than this ...
const MAX_ARRAY_DOCUMENTS: u32 = 100_000;
/// ... or more bytes than this (a result must fit one 64 MiB protocol frame).
const MAX_ARRAY_BYTES: u32 = 48 * 1024 * 1024;
/// `sleep` wakes this often to look at the cancel flag.
const SLEEP_SLICE: Duration = Duration::from_millis(10);

const PRELUDE: &str = concat!(
    "(function (host) {\n",
    include_str!("js/01_core.js"),
    include_str!("js/02_bson.js"),
    include_str!("js/03_commands.js"),
    include_str!("js/04_cursor.js"),
    include_str!("js/05_collection.js"),
    include_str!("js/06_database.js"),
    "\n})"
);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Abort {
    Cancelled,
    TimedOut,
}

/// The borrowed port with its lifetime erased so JavaScript natives (which
/// QuickJS requires to be `'static`) can hold it.
#[derive(Clone, Copy)]
struct PortRef(*const (dyn CommandPort + 'static));

impl PortRef {
    /// # Safety
    /// Every holder must be gone before `port` is: [`execute`] keeps the whole
    /// QuickJS runtime (the only holder) inside one call that borrows `port`.
    unsafe fn new(port: &dyn CommandPort) -> Self {
        // SAFETY: only the lifetime bound changes; see the contract above.
        let erased: &'static (dyn CommandPort + 'static) = unsafe { std::mem::transmute(port) };
        Self(erased)
    }

    fn get(&self) -> &dyn CommandPort {
        // SAFETY: valid for the whole run by the contract of `new`; the run is
        // single-threaded.
        unsafe { &*self.0 }
    }
}

struct RunState {
    port: PortRef,
    allow_writes: bool,
    deadline: Option<Instant>,
    abort: Cell<Option<Abort>>,
    /// Set while the shell's own setup and result handling run: nothing
    /// interrupts those, only the user's script.
    paused: Cell<bool>,
    output: RefCell<Output>,
}

impl RunState {
    fn poll_abort(&self) -> Option<Abort> {
        if self.paused.get() {
            return None;
        }
        if self.abort.get().is_none() {
            if self.port.get().is_cancelled() {
                self.abort.set(Some(Abort::Cancelled));
            } else if self.deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                self.abort.set(Some(Abort::TimedOut));
            }
        }
        self.abort.get()
    }
}

/// What one run produced, before it becomes a [`ShellReply`].
struct Outcome {
    result: ShellResult,
    database_after: Option<String>,
    error: Option<ShellFailure>,
}

fn failure(kind: FailureKind, message: impl Into<String>) -> ShellFailure {
    ShellFailure {
        kind,
        message: message.into(),
        line: None,
        column: None,
        code: None,
        code_name: None,
    }
}

/// Runs `request.script` against `port` and reports what it printed, what it
/// produced and how it ended. Never panics on a bad script; every failure is
/// in [`ShellReply::error`].
pub fn run_script(request: &ShellRequest, port: &dyn CommandPort) -> ShellReply {
    run_script_with_stack(request, port, MAX_STACK_BYTES)
}

/// [`run_script`] with an explicit QuickJS stack limit; the calling thread's
/// stack must be comfortably larger than `max_stack_bytes`.
pub fn run_script_with_stack(
    request: &ShellRequest,
    port: &dyn CommandPort,
    max_stack_bytes: usize,
) -> ShellReply {
    let state = Rc::new(RunState {
        // SAFETY: `execute` below is the only user and returns before `port` does.
        port: unsafe { PortRef::new(port) },
        allow_writes: request.allow_writes,
        deadline: request
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms)),
        abort: Cell::new(None),
        paused: Cell::new(true),
        output: RefCell::new(Output::default()),
    });
    let outcome = execute(request, &state, max_stack_bytes);
    let output = state.output.take();
    ShellReply {
        output_truncated: output.is_truncated(),
        output: output.into_lines(),
        result: outcome.result,
        database_after: outcome
            .database_after
            .unwrap_or_else(|| request.database.clone()),
        error: outcome.error,
    }
}

fn execute(request: &ShellRequest, state: &Rc<RunState>, max_stack_bytes: usize) -> Outcome {
    let internal = |message: String| Outcome {
        result: ShellResult::None,
        database_after: None,
        error: Some(failure(FailureKind::Internal, message)),
    };
    if request.database.trim().is_empty() {
        return Outcome {
            result: ShellResult::None,
            database_after: None,
            error: Some(failure(FailureKind::BadRequest, "a database name is required")),
        };
    }
    let runtime = match Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => return internal(format!("could not start the JavaScript engine: {error}")),
    };
    runtime.set_memory_limit(MAX_MEMORY_BYTES);
    runtime.set_max_stack_size(max_stack_bytes);
    let handler_state = Rc::clone(state);
    runtime.set_interrupt_handler(Some(Box::new(move || handler_state.poll_abort().is_some())));
    let context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => return internal(format!("could not create the JavaScript context: {error}")),
    };
    context.with(|ctx| match run_in_context(&ctx, request, state) {
        Ok(outcome) => outcome,
        Err(error) => internal(error.to_string()),
    })
}

/// A failure of the machinery itself (not of the script).
#[derive(Debug, thiserror::Error)]
enum SetupError {
    #[error("JavaScript engine error: {0}")]
    Engine(#[from] rquickjs::Error),
    #[error("the shell produced an unreadable result: {0}")]
    Result(#[from] serde_json::Error),
}

fn run_in_context<'js>(
    ctx: &Ctx<'js>,
    request: &ShellRequest,
    state: &Rc<RunState>,
) -> Result<Outcome, SetupError> {
    let host = build_host(ctx, state)?;
    let wrapper: Function = eval_named(ctx, PRELUDE, "prelude", false)?;
    let setup: Function = wrapper.call((host,))?;
    let request_value = Object::new(ctx.clone())?;
    request_value.set("database", request.database.as_str())?;
    if let Some(cursor) = &request.cursor {
        let reference = Object::new(ctx.clone())?;
        reference.set("id", cursor.id.as_str())?;
        reference.set("database", cursor.database.as_str())?;
        reference.set("collection", cursor.collection.as_str())?;
        reference.set("batch_size", cursor.batch_size)?;
        request_value.set("cursor", reference)?;
    }
    let api: Object = setup.call((request_value,))?;

    let script = crate::preprocess::rewrite_line_commands(&request.script).code;
    state.paused.set(false);
    let evaluated = match state.poll_abort() {
        // Stopped before the script began: it never runs.
        Some(_) => {
            state.paused.set(true);
            return Ok(Outcome {
                result: ShellResult::None,
                database_after: None,
                error: Some(describe_abort(state, request)),
            });
        }
        None => evaluate_script(ctx, &script, request, state),
    };
    let outcome = match evaluated {
        Ok(value) => finish_value(ctx, &api, value, request, state),
        Err(failed) => Ok(*failed),
    };
    if outcome.as_ref().is_ok_and(|outcome| outcome.error.is_some()) && state.abort.get().is_none() {
        // A failed script leaves no cursor behind either (still interruptible).
        let _ = api
            .get::<_, Function>("releaseCursors")
            .and_then(|release| release.call::<_, ()>((Value::new_undefined(ctx.clone()),)));
        let _ = ctx.catch();
    }
    state.paused.set(true);
    let database_after = api
        .get::<_, Function>("currentDatabase")
        .and_then(|function| function.call::<_, String>(()))
        .ok();
    let mut outcome = outcome?;
    outcome.database_after = database_after;
    Ok(outcome)
}

fn eval_named<'js, V: rquickjs::FromJs<'js>>(
    ctx: &Ctx<'js>,
    source: &str,
    name: &str,
    promise: bool,
) -> rquickjs::Result<V> {
    let mut options = EvalOptions::default();
    options.global = true;
    options.promise = promise;
    options.filename = Some(name.to_owned());
    ctx.eval_with_options(source, options)
}

/// Evaluates the user's script; a script failure comes back as an
/// [`Outcome`] with the typed error, not as `Err`.
fn evaluate_script<'js>(
    ctx: &Ctx<'js>,
    script: &str,
    request: &ShellRequest,
    state: &Rc<RunState>,
) -> Result<Value<'js>, Box<Outcome>> {
    let fail = |ctx: &Ctx<'js>| {
        Box::new(Outcome {
            result: ShellResult::None,
            database_after: None,
            error: Some(describe_exception(ctx, state, request)),
        })
    };
    if !uses_await(script) {
        return eval_named::<Value>(ctx, script, "script", false).map_err(|_| fail(ctx));
    }
    let promise: rquickjs::Promise = eval_named(ctx, script, "script", true).map_err(|_| fail(ctx))?;
    while ctx.execute_pending_job() {}
    match promise.result::<Value>() {
        Some(Ok(value)) => unwrap_async_completion(value).map_err(|_| fail(ctx)),
        Some(Err(_)) => Err(fail(ctx)),
        None => Err(Box::new(Outcome {
            result: ShellResult::None,
            database_after: None,
            error: Some(failure(
                FailureKind::Unsupported,
                "await on something that never settles: the shell has no timers or event loop",
            )),
        })),
    }
}

/// A script evaluated as async resolves to `{ value }`.
fn unwrap_async_completion(value: Value<'_>) -> rquickjs::Result<Value<'_>> {
    match value.as_object() {
        Some(object) if object.contains_key("value")? => object.get("value"),
        _ => Ok(value),
    }
}

fn uses_await(script: &str) -> bool {
    script
        .match_indices("await")
        .any(|(at, word)| {
            let before = script[..at].chars().next_back();
            let after = script[at + word.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$' || c == '.')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
        })
}

fn finish_value<'js>(
    ctx: &Ctx<'js>,
    api: &Object<'js>,
    value: Value<'js>,
    request: &ShellRequest,
    state: &Rc<RunState>,
) -> Result<Outcome, SetupError> {
    let page_size = request.page_size.max(1);
    let finish: Function = api.get("finish")?;
    let described: Result<String, _> = finish.call((value.clone(), page_size));
    let text = match described {
        Ok(text) => text,
        Err(_) => {
            return Ok(Outcome {
                result: ShellResult::None,
                database_after: None,
                error: Some(describe_exception(ctx, state, request)),
            })
        }
    };
    let result: ShellResult = serde_json::from_str(&text)?;
    let release: Function = api.get("releaseCursors")?;
    if state.abort.get().is_none() {
        // Cursors the script left open are closed; a failure here is not the script's.
        let _ = release.call::<_, ()>((value,));
        let _ = ctx.catch();
    }
    Ok(Outcome {
        result,
        database_after: None,
        error: None,
    })
}

// ---- natives ------------------------------------------------------------------------

fn build_host<'js>(ctx: &Ctx<'js>, state: &Rc<RunState>) -> rquickjs::Result<Object<'js>> {
    let host = Object::new(ctx.clone())?;

    let run_state = Rc::clone(state);
    host.set(
        "run",
        Func::from(move |ctx: Ctx<'js>, database: String, command: String| {
            host_run(&ctx, &run_state, &database, &command)
        }),
    )?;

    let print_state = Rc::clone(state);
    host.set(
        "print",
        Func::from(move |text: Coerced<String>| print_state.output.borrow_mut().push(&text.0)),
    )?;

    host.set(
        "shellText",
        Func::from(|text: String, pretty: bool| match crate::ejson::parse_value(&text) {
            Ok(value) => crate::ejson::shell_text(&value, pretty),
            Err(_) => text,
        }),
    )?;

    let sleep_state = Rc::clone(state);
    host.set(
        "sleep",
        Func::from(move |ms: f64| sleep(&sleep_state, ms)),
    )?;

    host.set("random", Func::from(random_hex))?;
    host.set("batchDocuments", Func::from(batch_documents))?;

    let limits = Object::new(ctx.clone())?;
    limits.set("maxDocuments", MAX_ARRAY_DOCUMENTS)?;
    limits.set("maxBytes", MAX_ARRAY_BYTES)?;
    host.set("limits", limits)?;
    Ok(host)
}

/// A command from the script: the string reply on success, `{ kind, message }`
/// when it must fail.
fn host_run<'js>(
    ctx: &Ctx<'js>,
    state: &RunState,
    database: &str,
    command: &str,
) -> rquickjs::Result<Value<'js>> {
    let refusal = |kind: &str, message: &str| -> rquickjs::Result<Value<'js>> {
        let object = Object::new(ctx.clone())?;
        object.set("kind", kind)?;
        object.set("message", message)?;
        Ok(object.into_value())
    };
    match state.poll_abort() {
        Some(Abort::Cancelled) => return refusal("cancelled", "operation cancelled"),
        Some(Abort::TimedOut) => return refusal("timeout", "the script ran out of time"),
        None => {}
    }
    if !state.allow_writes {
        let guarded = crate::ejson::parse_document(command)
            .map_err(|error| error.to_string())
            .and_then(|document| crate::guard::check_read_only(&document).map_err(|r| r.to_string()));
        if let Err(message) = guarded {
            return refusal("read_only", &message);
        }
    }
    match state.port.get().run_command_text(database, command) {
        Ok(reply) => Ok(rquickjs::String::from_str(ctx.clone(), &reply)?.into_value()),
        Err(CommandError::Cancelled) => {
            state.abort.set(Some(Abort::Cancelled));
            refusal("cancelled", "operation cancelled")
        }
        Err(CommandError::Refused { code, message }) => {
            let kind = if code == engine_driver_api::sql::codes::QUERY_READ_ONLY_VIOLATION {
                "read_only"
            } else {
                "failed"
            };
            refusal(kind, &message)
        }
        Err(CommandError::Failed(message)) => refusal("failed", &message),
    }
}

/// Sleeps `ms`, waking to watch for cancel / timeout; `false` when stopped.
fn sleep(state: &RunState, ms: f64) -> bool {
    let end = Instant::now() + Duration::from_secs_f64(ms.max(0.0) / 1000.0);
    loop {
        if state.poll_abort().is_some() {
            return false;
        }
        let now = Instant::now();
        if now >= end {
            return true;
        }
        std::thread::sleep((end - now).min(SLEEP_SLICE));
    }
}

/// The documents of `cursor.<key>` (`firstBatch` / `nextBatch`) in a command
/// reply, each as the server's own text, so key order survives.
fn batch_documents(reply: String, key: String) -> rquickjs::Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Reply {
        cursor: Batches,
    }
    #[derive(serde::Deserialize)]
    struct Batches {
        #[serde(rename = "firstBatch", default)]
        first: Vec<Box<serde_json::value::RawValue>>,
        #[serde(rename = "nextBatch", default)]
        next: Vec<Box<serde_json::value::RawValue>>,
    }
    let parsed: Reply = serde_json::from_str(&reply)
        .map_err(|error| rquickjs::Error::new_from_js_message("reply", "documents", error.to_string()))?;
    let batch = if key == "firstBatch" { parsed.cursor.first } else { parsed.cursor.next };
    Ok(batch.into_iter().map(|document| document.get().to_owned()).collect())
}

fn random_hex(bytes: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let wanted = bytes.min(64) * 2;
    let mut out = String::with_capacity(wanted + 16);
    let mut counter = 0u64;
    while out.len() < wanted {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        counter += 1;
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out.truncate(wanted);
    out
}

// ---- failures -------------------------------------------------------------------------

fn describe_exception<'js>(ctx: &Ctx<'js>, state: &RunState, request: &ShellRequest) -> ShellFailure {
    let thrown = ctx.catch();
    if state.abort.get().is_some() {
        return describe_abort(state, request);
    }
    if thrown.is_null() || thrown.is_undefined() {
        // QuickJS has no object left to throw when the heap is exhausted.
        return failure(
            FailureKind::Memory,
            format!("the script ran out of memory (limit {} MiB)", MAX_MEMORY_BYTES >> 20),
        );
    }
    let Some(object) = thrown.as_object() else {
        let text = Coerced::<String>::from_js(ctx, thrown)
            .map(|text| text.0)
            .unwrap_or_else(|_| "the script threw a value".into());
        return failure(FailureKind::Script, format!("Uncaught {text}"));
    };
    let text = |name: &str| {
        object
            .get::<_, Option<Coerced<String>>>(name)
            .ok()
            .flatten()
            .map(|value| value.0)
    };
    let message = text("message").unwrap_or_else(|| "the script failed".into());
    let kind = match text("kind").as_deref() {
        Some("server") => FailureKind::Server,
        Some("read_only") => FailureKind::ReadOnly,
        Some("unsupported") => FailureKind::Unsupported,
        Some("cancelled") => FailureKind::Cancelled,
        Some("timeout") => FailureKind::Timeout,
        Some("limit") => FailureKind::Limit,
        Some(_) => FailureKind::Script,
        None if text("name").as_deref() == Some("SyntaxError") => FailureKind::Syntax,
        None if message.contains("out of memory") => FailureKind::Memory,
        None => FailureKind::Script,
    };
    let position = text("stack").as_deref().and_then(script_position);
    ShellFailure {
        kind,
        message,
        line: position.map(|(line, _)| line),
        column: position.map(|(_, column)| column),
        code: object
            .get::<_, Option<f64>>("code")
            .ok()
            .flatten()
            .map(|code| code as i64),
        code_name: text("codeName"),
    }
}

fn describe_abort(state: &RunState, request: &ShellRequest) -> ShellFailure {
    match state.abort.get() {
        Some(Abort::TimedOut) => failure(
            FailureKind::Timeout,
            format!(
                "the script ran longer than {} ms and was stopped",
                request.timeout_ms.unwrap_or_default()
            ),
        ),
        _ => failure(FailureKind::Cancelled, "operation cancelled"),
    }
}

/// The first `script:LINE:COLUMN` in an error's stack: where in the user's
/// code (not the shell's own) the failure happened.
fn script_position(stack: &str) -> Option<(u32, u32)> {
    static FRAME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let frame = FRAME.get_or_init(|| regex::Regex::new(r"\bscript:(\d+):(\d+)").expect("static regex"));
    let found = frame.captures(stack)?;
    Some((found[1].parse().ok()?, found[2].parse().ok()?))
}
