//! A fake connection for the shell tests: records every command and answers
//! from a closure.
#![allow(dead_code)]

use bson::Document;
use engine_driver_api::mongo_shell::{ShellReply, ShellRequest};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tablex_mongo_shell::{CommandError, CommandPort};

pub type Responder = Box<dyn Fn(&str, &Document) -> Result<Document, CommandError> + Send + Sync>;

pub struct FakePort {
    pub commands: Mutex<Vec<(String, Document)>>,
    responder: Responder,
    pub cancelled: AtomicBool,
}

impl FakePort {
    pub fn new(responder: impl Fn(&str, &Document) -> Result<Document, CommandError> + Send + Sync + 'static) -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            responder: Box::new(responder),
            cancelled: AtomicBool::new(false),
        }
    }

    pub fn ok() -> Self {
        Self::new(|_, _| Ok(bson::doc! { "ok": 1.0 }))
    }

    pub fn seen(&self) -> Vec<(String, Document)> {
        self.commands.lock().unwrap().clone()
    }

    pub fn names(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .map(|(_, command)| command.keys().next().cloned().unwrap_or_default())
            .collect()
    }
}

impl CommandPort for FakePort {
    fn run_command(&self, database: &str, command: Document) -> Result<Document, CommandError> {
        self.commands.lock().unwrap().push((database.to_string(), command.clone()));
        (self.responder)(database, &command)
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

pub fn request(script: &str) -> ShellRequest {
    ShellRequest {
        script: script.into(),
        database: "app".into(),
        allow_writes: true,
        page_size: 50,
        timeout_ms: None,
        cursor: None,
    }
}

pub fn run(script: &str, port: &FakePort) -> ShellReply {
    run_with(&request(script), port)
}

/// Runs on a thread with a big stack (the debug build's QuickJS frames are large).
pub fn run_with(request: &ShellRequest, port: &FakePort) -> ShellReply {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn_scoped(scope, || tablex_mongo_shell::run_script_with_stack(request, port, 8 * 1024 * 1024))
            .unwrap()
            .join()
            .unwrap()
    })
}

use bson::doc;
use engine_driver_api::mongo_shell::{FailureKind, ShellResult};

/// `{ cursor: { id, ns, firstBatch }, ok: 1 }`.
pub fn first_batch(ns: &str, id: i64, batch: Vec<Document>) -> Document {
    doc! { "cursor": { "id": id, "ns": ns, "firstBatch": batch }, "ok": 1.0 }
}

/// `{ cursor: { id, ns, nextBatch }, ok: 1 }`.
pub fn next_batch(ns: &str, id: i64, batch: Vec<Document>) -> Document {
    doc! { "cursor": { "id": id, "ns": ns, "nextBatch": batch }, "ok": 1.0 }
}

pub fn numbered(count: i32) -> Vec<Document> {
    (0..count).map(|n| doc! { "_id": n, "n": n }).collect()
}

pub fn command_name(command: &Document) -> &str {
    command.keys().next().map(String::as_str).unwrap_or("")
}

/// The documents a result carries, parsed back to BSON.
pub fn result_documents(reply: &ShellReply) -> Vec<Document> {
    let raw = match &reply.result {
        ShellResult::Documents { documents } | ShellResult::Cursor { documents, .. } => documents,
        other => panic!("not a document result: {other:?}"),
    };
    raw.iter()
        .map(|text| tablex_mongo_shell::text::parse_document(text.get()).expect("document"))
        .collect()
}

pub fn value_text(reply: &ShellReply) -> String {
    match &reply.result {
        ShellResult::Value { text } => text.clone(),
        other => panic!("not a value result: {other:?} (error: {:?})", reply.error),
    }
}

pub fn failure_kind(reply: &ShellReply) -> FailureKind {
    reply.error.as_ref().unwrap_or_else(|| panic!("no error: {reply:?}")).kind
}

pub fn assert_ok(reply: &ShellReply) {
    assert!(reply.error.is_none(), "unexpected error: {:?}", reply.error);
}
