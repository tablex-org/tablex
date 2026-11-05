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
//! A multi-statement script is `multi_statement`; transaction / session state
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
        return Err(AgentError::Unsupported(format!(
            "`{}` changes transaction or session state, which agents cannot use: every tool call is one self-contained {what}",
            classified.verb
        )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use AgentAccess::*;

    fn stmt(class: StatementClass, danger: Option<&str>, statements: usize) -> Classified {
        Classified {
            statements,
            class,
            danger: danger.map(str::to_owned),
            verb: "X".into(),
            session_state: false,
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
    fn the_statement_matrix() {
        let read = stmt(StatementClass::Read, None, 1);
        let write = stmt(StatementClass::Write, None, 1);
        let danger = stmt(StatementClass::Write, Some("DROP removes objects"), 1);
        let blocked = stmt(StatementClass::Blocked, None, 1);
        let script = stmt(StatementClass::Read, None, 2);
        let empty = stmt(StatementClass::Read, None, 0);
        let session = Classified {
            session_state: true,
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
}
