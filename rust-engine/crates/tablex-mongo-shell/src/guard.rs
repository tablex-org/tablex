//! The read-only guard: with `allow_writes = false` only read commands leave
//! the shell. This is the plugin's own copy of the list the driver enforces
//! again (`read_only` on `mongo_command`): the plugin ships on its own, so it
//! cannot lean on the driver being new enough.

use bson::{Bson, Document};

/// Keys a `profile` command may carry besides `profile`: anything else
/// (`slowms`, `sampleRate`, `filter`) changes the server even at level -1.
const PROFILE_GENERIC_KEYS: &[&str] = &["comment", "$db", "lsid"];

const READ_COMMANDS: &[&str] = &[
    "find", "count", "distinct", "getMore", "killCursors", "listCollections", "listDatabases",
    "listIndexes", "collStats", "dbStats", "dbHash", "dataSize", "serverStatus", "buildInfo",
    "hostInfo", "connectionStatus", "connPoolStats", "currentOp", "ping", "hello", "isMaster",
    "listCommands", "getCmdLineOpts", "getParameter", "getLog", "whatsmyuri", "usersInfo",
    "rolesInfo", "top", "lockInfo", "replSetGetStatus", "replSetGetConfig",
];

/// A command the read-only guard refused.
#[derive(Debug, thiserror::Error)]
#[error("the command `{command}` {why}; this connection is read-only")]
pub struct Refused {
    pub command: String,
    pub why: String,
}

fn refuse(command: &str, why: &str) -> Refused {
    Refused {
        command: command.into(),
        why: why.into(),
    }
}

/// Lets read commands through (an `aggregate` must not write; `explain` wraps
/// an allowed command; `profile: -1` only reads the level).
pub fn check_read_only(command: &Document) -> Result<(), Refused> {
    let Some(name) = command.keys().next() else {
        return Err(refuse("", "is empty"));
    };
    let is = |expected: &str| name.eq_ignore_ascii_case(expected);
    if is("aggregate") {
        return match command.get("pipeline") {
            Some(Bson::Array(stages)) => check_pipeline(stages, name),
            _ => Err(refuse(name, "has no pipeline array to inspect")),
        };
    }
    if is("explain") {
        return match command.get("explain") {
            Some(Bson::Document(inner)) => check_read_only(inner),
            _ => Err(refuse(name, "does not wrap a command document")),
        };
    }
    if is("profile") {
        let reads_only = match command.get("profile") {
            Some(Bson::Int32(level)) => *level == -1,
            Some(Bson::Int64(level)) => *level == -1,
            Some(Bson::Double(level)) => *level == -1.0,
            _ => false,
        };
        let generic_only = command
            .keys()
            .all(|key| key == name || PROFILE_GENERIC_KEYS.contains(&key.as_str()));
        return if reads_only && generic_only {
            Ok(())
        } else {
            Err(refuse(name, "changes the profiling settings"))
        };
    }
    if READ_COMMANDS.iter().any(|allowed| is(allowed)) {
        Ok(())
    } else {
        Err(refuse(name, "can write or change the server"))
    }
}

fn check_pipeline(stages: &[Bson], command: &str) -> Result<(), Refused> {
    for stage in stages.iter().filter_map(Bson::as_document) {
        for (operator, body) in stage {
            if operator == "$out" || operator == "$merge" {
                return Err(refuse(command, &format!("has a `{operator}` stage")));
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

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn reads_pass_writes_are_refused() {
        for ok in [
            doc! { "find": "c" },
            doc! { "getMore": 5_i64, "collection": "c" },
            doc! { "listDatabases": 1 },
            doc! { "isMaster": 1 },
            doc! { "profile": -1 },
            doc! { "profile": -1, "comment": "c" },
            doc! { "aggregate": "c", "pipeline": [{ "$match": {} }], "cursor": {} },
            doc! { "explain": { "find": "c" } },
        ] {
            assert!(check_read_only(&ok).is_ok(), "{ok}");
        }
        for bad in [
            doc! { "insert": "c" },
            doc! { "update": "c" },
            doc! { "delete": "c" },
            doc! { "findAndModify": "c" },
            doc! { "createIndexes": "c" },
            doc! { "dropIndexes": "c" },
            doc! { "drop": "c" },
            doc! { "dropDatabase": 1 },
            doc! { "renameCollection": "a.b" },
            doc! { "create": "c" },
            doc! { "killOp": 1 },
            doc! { "profile": 1 },
            doc! { "profile": -1, "slowms": 1 },
            doc! { "profile": -1, "sampleRate": 0.1 },
            doc! { "profile": -1, "filter": { "ns": "x" } },
            doc! { "features": 1 },
            doc! { "features": 1, "oidReset": true },
            doc! { "explain": { "delete": "c" } },
            doc! { "aggregate": "c", "pipeline": [{ "$out": "x" }] },
            doc! { "aggregate": "c", "pipeline": [{ "$lookup": { "pipeline": [{ "$merge": "x" }] } }] },
            doc! { "aggregate": "c", "pipeline": [{ "$facet": { "a": [{ "$out": "x" }] } }] },
            doc! { "aggregate": "c" },
        ] {
            assert!(check_read_only(&bad).is_err(), "{bad}");
        }
        assert!(check_read_only(&Document::new()).is_err());
    }
}
