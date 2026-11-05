//! `mongo_command`: one raw `runCommand` for clients that speak the MongoDB
//! command protocol themselves (the optional MongoDB Shell plugin).
//!
//! Request (`dbengine_mongo_command`, capability `mongo.command`):
//! `{ operation_id, database, command, read_only?, max_time_ms? }` where
//! `command` is a canonical Extended JSON object whose first key is the command
//! name. Reply: `{ reply }`, the server's reply document as canonical Extended
//! JSON in the server's key order. A command the server rejects (`ok: 0`) is
//! still a reply (`{ ok: 0, errmsg, code, codeName }`), exactly like on the
//! wire; only transport trouble, a cancel or a refused request is an error
//! envelope. Cursor commands (`find`, `aggregate`, `listCollections`, ...)
//! return the raw cursor reply and `getMore` / `killCursors` pass through the
//! same op, so the caller pages.
//!
//! With `read_only` only the read commands of [`READ_COMMANDS`] run (an
//! `aggregate` must not write; `explain` wraps an allowed command); anything
//! else is refused with `query.read_only_violation` before it is sent.

use bson::{Bson, Document};
use engine_driver_api::OperationId;

/// Commands a read-only caller may run, matched case-insensitively (the
/// server itself accepts a few in lower case).
const READ_COMMANDS: &[&str] = &[
    "find",
    "count",
    "distinct",
    "getMore",
    "killCursors",
    "listCollections",
    "listDatabases",
    "listIndexes",
    "collStats",
    "dbStats",
    "dbHash",
    "dataSize",
    "serverStatus",
    "buildInfo",
    "hostInfo",
    "connectionStatus",
    "connPoolStats",
    "currentOp",
    "ping",
    "hello",
    "isMaster",
    "listCommands",
    "getCmdLineOpts",
    "getParameter",
    "getLog",
    "whatsmyuri",
    "usersInfo",
    "rolesInfo",
    "top",
    "lockInfo",
    "replSetGetStatus",
    "replSetGetConfig",
];

/// Prefix of the operation tag raw commands carry (see `ffi::begin_operation`).
pub(crate) const COMMAND_TAG_PREFIX: &str = "tablex-cmd:";

/// First wire version that takes `comment` on every command (MongoDB 4.4).
const WIRE_VERSION_COMMENT_ON_ALL: i32 = 9;

/// Commands that accept `comment` on every server; the rest take it from
/// [`WIRE_VERSION_COMMENT_ON_ALL`] on.
const TAGGED_ALWAYS: &[&str] = &["find", "aggregate"];
const TAGGED_FROM_4_4: &[&str] = &["count", "distinct", "update", "delete", "findAndModify"];

/// Commands that accept `maxTimeMS` on every server; `update` / `delete` take
/// it from [`WIRE_VERSION_COMMENT_ON_ALL`] on.
const TIMED_ALWAYS: &[&str] = &["find", "aggregate", "count", "distinct", "findAndModify"];
const TIMED_FROM_4_4: &[&str] = &["update", "delete"];

/// Keys a `profile` command may carry besides `profile` itself: anything else
/// (`slowms`, `sampleRate`, `filter`) changes the server even at level -1.
const PROFILE_GENERIC_KEYS: &[&str] = &["comment", "$db", "lsid"];

#[derive(serde::Deserialize)]
pub(crate) struct CommandRequest {
    pub operation_id: OperationId,
    pub database: String,
    /// Canonical Extended JSON; see [`parse_command`].
    pub command: serde_json::Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub max_time_ms: Option<u64>,
}

/// Why a `mongo_command` did not produce a reply.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CommandError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    ReadOnlyViolation(String),
    #[error("operation cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
    #[error("the server rejected the command: {message}")]
    Server {
        code: i32,
        code_name: String,
        message: String,
    },
    #[error("this connection does not run raw commands")]
    Unsupported,
}

impl CommandError {
    /// The server's refusal as the reply a server sends for `ok: 0`.
    pub(crate) fn server_reply(code: i32, code_name: &str, message: &str) -> Document {
        bson::doc! { "ok": 0.0, "errmsg": message, "code": code, "codeName": code_name }
    }
}

/// The one parser for a command: the same Extended JSON reading the shell
/// plugin's read-only guard uses, so a plain JSON integer is an Int32 (or Int64
/// when it does not fit) in both places and typed numbers keep their tags.
pub(crate) fn parse_command(value: serde_json::Value) -> Result<Document, CommandError> {
    match Bson::try_from(value) {
        Ok(Bson::Document(document)) if !document.is_empty() => Ok(document),
        Ok(Bson::Document(_)) => Err(CommandError::BadRequest("the command document is empty".into())),
        Ok(_) => Err(CommandError::BadRequest("the command must be a document".into())),
        Err(error) => Err(CommandError::BadRequest(format!("the command is not valid Extended JSON: {error}"))),
    }
}

/// The command's name: its first key.
pub(crate) fn command_name(command: &Document) -> Option<&str> {
    command.keys().next().map(String::as_str)
}

fn is_read_command(name: &str) -> bool {
    READ_COMMANDS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(name))
}

fn violation(name: &str, why: &str) -> CommandError {
    CommandError::ReadOnlyViolation(format!(
        "the command `{name}` {why}; this connection is read-only"
    ))
}

/// Refuses everything but the read commands (see the module docs).
pub(crate) fn check_read_only(command: &Document) -> Result<(), CommandError> {
    let name = command_name(command)
        .ok_or_else(|| CommandError::BadRequest("the command document is empty".into()))?;
    if name.eq_ignore_ascii_case("aggregate") {
        return match command.get("pipeline") {
            Some(Bson::Array(stages)) => check_pipeline(stages, name),
            _ => Err(violation(name, "has no pipeline array to inspect")),
        };
    }
    if name.eq_ignore_ascii_case("explain") {
        return match command.get("explain") {
            Some(Bson::Document(inner)) => check_read_only(inner),
            _ => Err(violation(name, "does not wrap a command document")),
        };
    }
    if name.eq_ignore_ascii_case("profile") {
        // `profile: -1` only reads the level, and only without `slowms` /
        // `sampleRate` / `filter`, which apply server-wide even then.
        let level_only = match command.get("profile") {
            Some(Bson::Int32(-1)) | Some(Bson::Int64(-1)) => true,
            Some(Bson::Double(level)) => *level == -1.0,
            _ => false,
        };
        let generic_only = command
            .keys()
            .all(|key| key == name || PROFILE_GENERIC_KEYS.contains(&key.as_str()));
        return if level_only && generic_only {
            Ok(())
        } else {
            Err(violation(name, "changes the profiling settings"))
        };
    }
    if is_read_command(name) {
        Ok(())
    } else {
        Err(violation(name, "can write or change the server"))
    }
}

/// A pipeline that writes (`$out`, `$merge`) is refused wherever it sits,
/// including inside `$lookup` / `$unionWith` / `$facet` sub-pipelines.
fn check_pipeline(stages: &[Bson], command: &str) -> Result<(), CommandError> {
    for stage in stages {
        let Bson::Document(stage) = stage else {
            continue;
        };
        for (operator, body) in stage {
            if operator == "$out" || operator == "$merge" {
                return Err(violation(command, &format!("has a `{operator}` stage")));
            }
            match (operator.as_str(), body) {
                ("$lookup" | "$unionWith", Bson::Document(spec)) => {
                    if let Some(Bson::Array(inner)) = spec.get("pipeline") {
                        check_pipeline(inner, command)?;
                    }
                }
                ("$facet", Bson::Document(branches)) => {
                    for branch in branches.values() {
                        if let Bson::Array(inner) = branch {
                            check_pipeline(inner, command)?;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Whether [`apply_controls`] needs the server's wire version for `name`.
pub(crate) fn needs_wire_version(name: &str) -> bool {
    TAGGED_FROM_4_4.contains(&name) || TIMED_FROM_4_4.contains(&name)
}

/// Adds the cancel tag and the server time limit where the command takes
/// them and the caller did not set its own. Commands older servers reject
/// the fields on are only tagged when `wire_version` (the server's
/// `maxWireVersion`) says it is 4.4 or newer; unknown means no.
pub(crate) fn apply_controls(
    command: &mut Document,
    comment: Option<&str>,
    max_time_ms: Option<u64>,
    wire_version: Option<i32>,
) {
    let Some(name) = command_name(command) else {
        return;
    };
    let modern = wire_version.is_some_and(|version| version >= WIRE_VERSION_COMMENT_ON_ALL);
    let tagged = TAGGED_ALWAYS.contains(&name) || (modern && TAGGED_FROM_4_4.contains(&name));
    let timed = TIMED_ALWAYS.contains(&name) || (modern && TIMED_FROM_4_4.contains(&name));
    if let Some(comment) = comment.filter(|_| tagged && !command.contains_key("comment")) {
        command.insert("comment", comment);
    }
    if let Some(limit) = max_time_ms.filter(|_| timed && !command.contains_key("maxTimeMS")) {
        command.insert("maxTimeMS", i64::try_from(limit).unwrap_or(i64::MAX));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    fn refused(command: Document) -> bool {
        matches!(
            check_read_only(&command),
            Err(CommandError::ReadOnlyViolation(_))
        )
    }

    #[test]
    fn reads_pass_and_writes_are_refused() {
        for allowed in [
            doc! { "find": "c", "filter": {} },
            doc! { "count": "c" },
            doc! { "getMore": 5_i64, "collection": "c" },
            doc! { "killCursors": "c", "cursors": [5_i64] },
            doc! { "listCollections": 1 },
            doc! { "ismaster": 1 },
            doc! { "serverStatus": 1 },
            doc! { "profile": -1 },
            doc! { "profile": -1, "comment": "x", "$db": "admin" },
            doc! { "aggregate": "c", "pipeline": [{ "$match": {} }], "cursor": {} },
            doc! { "explain": { "find": "c" }, "verbosity": "queryPlanner" },
        ] {
            assert!(check_read_only(&allowed).is_ok(), "{allowed}");
        }
        for write in [
            doc! { "insert": "c", "documents": [{}] },
            doc! { "update": "c", "updates": [] },
            doc! { "delete": "c", "deletes": [] },
            doc! { "findAndModify": "c", "remove": true },
            doc! { "drop": "c" },
            doc! { "dropDatabase": 1 },
            doc! { "createIndexes": "c", "indexes": [] },
            doc! { "renameCollection": "a.b", "to": "a.c" },
            doc! { "killOp": 1, "op": 5 },
            doc! { "profile": 2 },
            doc! { "profile": -1, "slowms": 1 },
            doc! { "profile": -1, "sampleRate": 0.5 },
            doc! { "profile": -1, "filter": { "millis": { "$gt": 1 } } },
            doc! { "features": 1, "oidReset": true },
            doc! { "features": 1 },
            doc! { "eval": "1" },
            doc! { "mapReduce": "c", "out": "x" },
            doc! { "explain": { "update": "c" } },
            doc! { "explain": 1 },
            doc! { "aggregate": "c", "cursor": {} },
        ] {
            assert!(refused(write.clone()), "{write}");
        }
        assert!(matches!(
            check_read_only(&Document::new()),
            Err(CommandError::BadRequest(_))
        ));
    }

    #[test]
    fn a_writing_pipeline_is_refused_at_any_depth() {
        for stage in [
            doc! { "$out": "x" },
            doc! { "$merge": { "into": "x" } },
            doc! { "$lookup": { "from": "o", "pipeline": [{ "$out": "x" }], "as": "a" } },
            doc! { "$unionWith": { "coll": "o", "pipeline": [{ "$merge": "x" }] } },
            doc! { "$facet": { "a": [{ "$match": {} }], "b": [{ "$out": "x" }] } },
        ] {
            let command = doc! { "aggregate": "c", "pipeline": [{ "$match": {} }, stage.clone()] };
            assert!(refused(command), "{stage}");
        }
        let fine = doc! { "aggregate": "c", "pipeline": [
            { "$lookup": { "from": "o", "pipeline": [{ "$match": {} }], "as": "a" } },
            { "$facet": { "a": [{ "$limit": 1 }] } },
        ] };
        assert!(check_read_only(&fine).is_ok());
    }

    #[test]
    fn controls_are_added_only_where_they_apply_and_never_over_the_callers() {
        let mut find = doc! { "find": "c" };
        apply_controls(&mut find, Some("tablex:1:2"), Some(500), None);
        assert_eq!(find.get_str("comment"), Ok("tablex:1:2"));
        assert_eq!(find.get_i64("maxTimeMS"), Ok(500));
        let mut own = doc! { "find": "c", "comment": "mine", "maxTimeMS": 9 };
        apply_controls(&mut own, Some("tablex:1:2"), Some(500), Some(21));
        assert_eq!(own.get_str("comment"), Ok("mine"));
        assert_eq!(own.get_i32("maxTimeMS"), Ok(9));
        for name in ["getMore", "listCollections", "drop", "insert"] {
            let mut other = doc! { name: "c" };
            apply_controls(&mut other, Some("tablex:1:2"), Some(500), Some(21));
            assert_eq!(other.len(), 1, "{name} is sent untouched");
        }
    }

    #[test]
    fn the_tag_on_count_update_and_friends_waits_for_a_4_4_server() {
        for name in ["count", "distinct", "update", "delete", "findAndModify"] {
            assert!(needs_wire_version(name), "{name}");
            for wire in [None, Some(8)] {
                let mut command = doc! { name: "c" };
                apply_controls(&mut command, Some("tag"), None, wire);
                assert!(!command.contains_key("comment"), "{name} on wire {wire:?} must not carry a comment");
            }
            let mut modern = doc! { name: "c" };
            apply_controls(&mut modern, Some("tag"), None, Some(9));
            assert_eq!(modern.get_str("comment"), Ok("tag"), "{name}");
        }
        // find / aggregate take the tag on every server; maxTimeMS on update / delete does not.
        for name in ["find", "aggregate"] {
            assert!(!needs_wire_version(name));
            let mut command = doc! { name: "c" };
            apply_controls(&mut command, Some("tag"), Some(7), Some(6));
            assert_eq!((command.get_str("comment"), command.get_i64("maxTimeMS")), (Ok("tag"), Ok(7)));
        }
        let mut update = doc! { "update": "c" };
        apply_controls(&mut update, None, Some(7), Some(8));
        assert!(!update.contains_key("maxTimeMS"));
        let mut count = doc! { "count": "c" };
        apply_controls(&mut count, Some("tag"), Some(7), Some(8));
        assert_eq!((count.contains_key("comment"), count.get_i64("maxTimeMS")), (false, Ok(7)));
    }

    #[test]
    fn one_parser_types_plain_numbers_like_the_guard_does() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"insert":"c","documents":[{"a":-1,"b":{"$numberInt":"-1"},"c":{"$numberLong":"-1"},"d":{"$numberDouble":"-1.0"},"e":7,"f":3000000000}]}"#,
        )
        .unwrap();
        let command = parse_command(json).unwrap();
        let doc = command.get_array("documents").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(doc.get("a"), Some(&Bson::Int32(-1)), "a plain negative integer is an Int32, not an Int64");
        assert_eq!(doc.get("b"), Some(&Bson::Int32(-1)));
        assert_eq!(doc.get("c"), Some(&Bson::Int64(-1)));
        assert_eq!(doc.get("d"), Some(&Bson::Double(-1.0)));
        assert_eq!(doc.get("e"), Some(&Bson::Int32(7)));
        assert!(matches!(doc.get("f"), Some(Bson::Int64(3_000_000_000))));
        for bad in ["[1]", "5", "{}"] {
            let value: serde_json::Value = serde_json::from_str(bad).unwrap();
            assert!(matches!(parse_command(value), Err(CommandError::BadRequest(_))), "{bad}");
        }
    }
}
