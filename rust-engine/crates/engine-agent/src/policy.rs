//! The statement policy: one pure function from (access, classified
//! statement, the model's `dangerous` flag) to "run it", "ask first" or a
//! typed refusal. The tool layer applies it to every statement tool.
//!
//! | access      | read          | write                    | dangerous write                       | blocked / script |
//! |-------------|---------------|--------------------------|---------------------------------------|------------------|
//! | off         | access_denied | access_denied            | access_denied                         | access_denied    |
//! | schema_only | access_denied | access_denied            | access_denied                         | access_denied    |
//! | read        | run           | write_refused            | write_refused                         | see below        |
//! | read_write  | run           | ask the approver         | dangerous_refused unless flagged, then ask (if the approver allows it) | see below |
//!
//! Additive DDL (`CREATE ...`, `ALTER TABLE ... ADD ...`, `COMMENT ON`) is a plain write in the
//! table, not a dangerous one: see [`crate::classify`]. A multi-statement script is `multi_statement`; transaction / session state
//! (`BEGIN`, `SET`, `USE`, `MULTI`, ...) is `unsupported`; a blocked function or
//! clause (`pg_read_file`, `INTO OUTFILE`, ...) is `blocked_function`; a command
//! the driver blocks is `dangerous_refused`: all at every access that may run
//! statements, in that order.

use crate::access::AgentAccess;
use crate::classify::{Classified, StatementClass};
use crate::error::AgentError;

/// What must happen before the statement runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// Nothing: it only reads.
    Nothing,
    /// The approver must say yes. `dangerous` is the reason when the
    /// statement is dangerous (the model flagged it).
    Approval { dangerous: Option<String> },
}

/// Applies the policy. `what` names the kind of text in messages.
pub fn check_statement(
    access: AgentAccess,
    classified: &Classified,
    flagged_dangerous: bool,
    what: &str,
) -> Result<Need, AgentError> {
    if access < AgentAccess::Read {
        return Err(AgentError::AccessDenied(format!(
            "agent access `{access}` does not allow running {what}s; it needs `read` or more"
        )));
    }
    match classified.statements {
        0 => return Err(AgentError::invalid(format!("the {what} is empty"))),
        1 => {}
        _ => return Err(AgentError::MultiStatement),
    }
    if classified.session_state {
        let text = classified.session_statement.as_deref().unwrap_or(&classified.verb);
        return Err(session_state_refusal(access, what, &[(None, text)]));
    }
    if let Some(clause) = &classified.unsupported_clause {
        return Err(AgentError::Unsupported(format!(
            "the {clause} clause is not available to agents; send the {what} without it"
        )));
    }
    if let Some(function) = &classified.blocked_function {
        return Err(AgentError::BlockedFunction(function.clone()));
    }
    match classified.class {
        StatementClass::Blocked => Err(AgentError::DangerousRefused(format!(
            "`{}` is blocked for agents (it can change server configuration, run scripts or scan the whole keyspace)",
            classified.verb
        ))),
        StatementClass::Read => Ok(Need::Nothing),
        StatementClass::Write if access < AgentAccess::ReadWrite => {
            Err(AgentError::WriteRefused(format!(
                "this {what} is a write (`{}`) and the agent access is `{access}`; writes need `read_write`, which only the user can set on the connection",
                classified.verb
            )))
        }
        StatementClass::Write => match &classified.danger {
            Some(reason) if !flagged_dangerous => Err(AgentError::DangerousRefused(format!(
                "this {what} is dangerous: {reason}. Do not retry unless the user asked for it; then call again with dangerous=true and the user will be asked to approve"
            ))),
            danger => Ok(Need::Approval {
                dangerous: danger.clone(),
            }),
        },
    }
}

/// Session-state statements of a script a refusal names one by one.
const MAX_NAMED: usize = 5;

/// Settings whose `SET` a model reaches for to bound a wait; `lock_timeout_secs` does it for them.
const TIMEOUT_SETTINGS: [&str; 5] = [
    "lock_timeout",
    "lock_wait_timeout",
    "statement_timeout",
    "max_execution_time",
    "max_statement_time",
];

/// The refusal of session-state statements (`SET`, `BEGIN`, ...): names each one to leave out
/// (`(Some(n), text)` is statement `n` of a script) and says what is still allowed, so a model does
/// not conclude that the whole script, or DDL, is blocked.
pub fn session_state_refusal(
    access: AgentAccess,
    what: &str,
    statements: &[(Option<usize>, &str)],
) -> AgentError {
    // The message is clipped at 2000 characters by the layer: five names leave room for the rest.
    let mut names = statements
        .iter()
        .take(MAX_NAMED)
        .map(|(number, text)| match number {
            Some(number) => format!("statement {number} `{text}`"),
            None => format!("`{text}`"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    if statements.len() > MAX_NAMED {
        names.push_str(&format!(" and {} more", statements.len() - MAX_NAMED));
    }
    let (verb, pronoun) = if statements.len() == 1 {
        ("changes", "it")
    } else {
        ("change", "them")
    };
    let sql = what == "SQL statement";
    let allowed = match access {
        AgentAccess::ReadWrite if sql => "; other statements, including additive DDL such as ALTER TABLE ... ADD COLUMN, are allowed at access `read_write`",
        AgentAccess::Read if sql => "; other statements are judged on their own at access `read` (reads run)",
        _ => "",
    };
    let lowered = statements
        .iter()
        .map(|(_, text)| text.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let hint = if sql
        && lowered
            .iter()
            .any(|text| TIMEOUT_SETTINGS.iter().any(|setting| text.contains(setting)))
    {
        " To bound lock waits on PostgreSQL, MySQL or MariaDB, pass `lock_timeout_secs` (1-600) to execute_query or execute_batch instead; tablex applies it for that call only."
    } else {
        ""
    };
    AgentError::Unsupported(format!(
        "{names} {verb} transaction or session state, which agents cannot use: every tool call is one self-contained {what}. Leave {pronoun} out{allowed}.{hint}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentAccess::*;

    fn stmt(class: StatementClass, danger: Option<&str>, statements: usize) -> Classified {
        Classified {
            statements,
            class,
            danger: danger.map(str::to_owned),
            driver_confirms: false,
            verb: "X".into(),
            session_state: false,
            session_statement: None,
            unsupported_clause: None,
            blocked_function: None,
        }
    }

    fn outcome(access: AgentAccess, classified: &Classified, flagged: bool) -> String {
        match check_statement(access, classified, flagged, "SQL statement") {
            Ok(Need::Nothing) => "run".into(),
            Ok(Need::Approval { dangerous: None }) => "ask".into(),
            Ok(Need::Approval { dangerous: Some(_) }) => "ask-dangerous".into(),
            Err(error) => error.code().to_owned(),
        }
    }

    #[test]
    fn additive_ddl_asks_like_an_insert_and_destructive_ddl_needs_the_flag() {
        use crate::classify::classify_sql;
        use crate::ports::ConnectionKind;
        use engine_driver_kit::sql::SqlLexOptions;
        let check = |sql: &str, access: AgentAccess, flagged: bool| {
            let classified =
                classify_sql(sql, ConnectionKind::PostgreSql, &SqlLexOptions::postgres());
            match check_statement(access, &classified, flagged, "SQL statement") {
                Ok(Need::Nothing) => "run".to_owned(),
                Ok(Need::Approval { dangerous: None }) => "ask".to_owned(),
                Ok(Need::Approval { dangerous: Some(_) }) => "ask-dangerous".to_owned(),
                Err(error) => error.code().to_owned(),
            }
        };
        for sql in [
            "ALTER TABLE t ADD COLUMN biz_domain text",
            "CREATE TABLE t (a int)",
            "CREATE INDEX CONCURRENTLY i ON t (a)",
            "COMMENT ON TABLE t IS 'x'",
        ] {
            assert_eq!(check(sql, ReadWrite, false), "ask", "{sql}");
            assert_eq!(check(sql, Read, false), "agent.write_refused", "{sql}");
            assert_eq!(check(sql, SchemaOnly, false), "agent.access_denied", "{sql}");
        }
        for sql in [
            "ALTER TABLE t DROP COLUMN c",
            "ALTER TABLE t ALTER COLUMN c TYPE text",
            "DROP INDEX i",
        ] {
            assert_eq!(check(sql, ReadWrite, false), "agent.dangerous_refused", "{sql}");
            assert_eq!(check(sql, ReadWrite, true), "ask-dangerous", "{sql}");
            assert_eq!(check(sql, Read, true), "agent.write_refused", "{sql}");
        }
    }

    #[test]
    fn the_statement_matrix() {
        let read = stmt(StatementClass::Read, None, 1);
        let write = stmt(StatementClass::Write, None, 1);
        let danger = stmt(StatementClass::Write, Some("DROP removes objects"), 1);
        let blocked = stmt(StatementClass::Blocked, None, 1);
        let script = stmt(StatementClass::Read, None, 2);
        let empty = stmt(StatementClass::Read, None, 0);
        let session = Classified {
            session_state: true,
            session_statement: Some("SET x = 1".into()),
            ..stmt(StatementClass::Read, None, 1)
        };
        let function = Classified {
            blocked_function: Some("pg_read_file".into()),
            ..stmt(StatementClass::Read, None, 1)
        };
        let write_function = Classified {
            blocked_function: Some("load_file".into()),
            ..stmt(StatementClass::Write, None, 1)
        };
        let rows: [(&str, &Classified, bool, [&str; 4]); 14] = [
            // access:                    off              schema_only      read                      read_write
            (
                "read",
                &read,
                false,
                ["agent.access_denied", "agent.access_denied", "run", "run"],
            ),
            (
                "write",
                &write,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.write_refused",
                    "ask",
                ],
            ),
            (
                "dangerous",
                &danger,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.write_refused",
                    "agent.dangerous_refused",
                ],
            ),
            (
                "dangerous+flag",
                &danger,
                true,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.write_refused",
                    "ask-dangerous",
                ],
            ),
            (
                "write+flag",
                &write,
                true,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.write_refused",
                    "ask",
                ],
            ),
            (
                "blocked",
                &blocked,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.dangerous_refused",
                    "agent.dangerous_refused",
                ],
            ),
            (
                "blocked+flag",
                &blocked,
                true,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.dangerous_refused",
                    "agent.dangerous_refused",
                ],
            ),
            (
                "script",
                &script,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.multi_statement",
                    "agent.multi_statement",
                ],
            ),
            (
                "empty",
                &empty,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.invalid_arguments",
                    "agent.invalid_arguments",
                ],
            ),
            (
                "session",
                &session,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.unsupported",
                    "agent.unsupported",
                ],
            ),
            (
                "blocked function",
                &function,
                false,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.blocked_function",
                    "agent.blocked_function",
                ],
            ),
            (
                "blocked function+flag",
                &function,
                true,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.blocked_function",
                    "agent.blocked_function",
                ],
            ),
            (
                "write with blocked function",
                &write_function,
                true,
                [
                    "agent.access_denied",
                    "agent.access_denied",
                    "agent.blocked_function",
                    "agent.blocked_function",
                ],
            ),
            (
                "read+flag",
                &read,
                true,
                ["agent.access_denied", "agent.access_denied", "run", "run"],
            ),
        ];
        for (name, classified, flagged, expected) in rows {
            for (access, want) in AgentAccess::ALL.into_iter().zip(expected) {
                assert_eq!(
                    outcome(access, classified, flagged),
                    want,
                    "{name} at {access}"
                );
            }
        }
    }

    #[test]
    fn refusals_tell_the_model_what_to_do() {
        let danger = stmt(StatementClass::Write, Some("DROP removes objects"), 1);
        let error = check_statement(ReadWrite, &danger, false, "SQL statement").unwrap_err();
        assert!(error.to_string().contains("dangerous=true"));
        let write = stmt(StatementClass::Write, None, 1);
        let error = check_statement(Read, &write, false, "SQL statement").unwrap_err();
        assert!(error.to_string().contains("read_write"));
    }

    fn refusal(sql: &str, access: AgentAccess) -> String {
        use crate::classify::classify_sql;
        use crate::ports::ConnectionKind;
        use engine_driver_kit::sql::SqlLexOptions;
        let classified = classify_sql(sql, ConnectionKind::PostgreSql, &SqlLexOptions::postgres());
        check_statement(access, &classified, false, "SQL statement")
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_session_state_refusal_names_the_statement_and_says_ddl_is_allowed() {
        let message = refusal("SET lock_timeout = '5s';", ReadWrite);
        assert!(message.starts_with("`SET lock_timeout = '5s'` changes transaction or session state"));
        assert!(message.contains("Leave it out"));
        assert!(message.contains("additive DDL such as ALTER TABLE ... ADD COLUMN, are allowed at access `read_write`"));
        assert!(message.contains("`lock_timeout_secs` (1-600)"));
        // A statement timeout and the MySQL spellings point to it too; other settings do not.
        for sql in ["SET statement_timeout = 0", "SET SESSION innodb_lock_wait_timeout = 1", "SET max_execution_time = 5"] {
            assert!(refusal(sql, ReadWrite).contains("lock_timeout_secs"), "{sql}");
        }
        let plain = refusal("SET search_path TO app", ReadWrite);
        assert!(plain.contains("`SET search_path TO app`"));
        assert!(!plain.contains("lock_timeout_secs"));
        assert!(!refusal("BEGIN", ReadWrite).contains("lock_timeout_secs"));
        // At `read` it must not claim that DDL runs.
        let read = refusal("SET lock_timeout = '5s'", Read);
        assert!(!read.contains("ALTER TABLE"));
        assert!(read.contains("at access `read`"));
        // A long statement is clipped, not echoed whole.
        assert!(refusal(&format!("SET x = '{}'", "a".repeat(500)), ReadWrite).len() < 600);
    }

    #[test]
    fn a_script_refusal_names_every_statement_and_a_redis_one_only_the_verb() {
        let error = session_state_refusal(
            ReadWrite,
            "SQL statement",
            &[(Some(1), "SET lock_timeout = '5s'"), (Some(3), "RESET ALL")],
        )
        .to_string();
        assert!(error.starts_with("statement 1 `SET lock_timeout = '5s'`, statement 3 `RESET ALL` change transaction"));
        assert!(error.contains("Leave them out"));
        // Many: five are named, the rest counted, and the hint and the access sentence survive.
        let many: Vec<(Option<usize>, String)> = (1..=40)
            .map(|n| (Some(n), format!("SET lock_timeout = '{}'", "5".repeat(100))))
            .collect();
        let many: Vec<(Option<usize>, &str)> = many.iter().map(|(n, t)| (*n, t.as_str())).collect();
        let error = session_state_refusal(ReadWrite, "SQL statement", &many).to_string();
        assert!(error.contains("statement 5 ") && !error.contains("statement 6 "), "{error}");
        assert!(error.contains("and 35 more change transaction"), "{error}");
        assert!(error.contains("lock_timeout_secs") && error.contains("read_write"), "{error}");
        assert!(error.chars().count() < 1500, "{}", error.chars().count());
        let redis = crate::classify::classify_redis("AUTH hunter2");
        let error = check_statement(ReadWrite, &redis, false, "Redis command")
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("`AUTH` changes"), "{error}");
        assert!(!error.contains("hunter2") && !error.contains("ALTER TABLE"));
    }
}
