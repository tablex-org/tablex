//! Writes through the MCP server: a statement that changes data is put to the user through MCP
//! elicitation, behind three conditions (settings that allow it, a non-production connection, a
//! client that can be asked), and runs only when the client answers `accept` with `run: true`.
//!
//! The server is the real one over an in-memory pipe, the client is an rmcp client that does or does
//! not declare the elicitation capability. The matrix runs the real SQLite driver (`Fixture::new`,
//! so `setup::build_layer` is what is tested); the rest use fake sessions that record every
//! statement that reaches them. Nothing here touches the user's stores or Keychain.

mod support;

use std::sync::Arc;
use std::time::Duration;

use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeSession};
use engine_agent::{AgentAccess, ConnectionKind, Session, ToolLayer};
use rmcp::model::{ElicitResult, ElicitationAction, ErrorData as McpError};
use serde_json::{json, Value};
use support::*;
use tablex_mcp::approver::{
    no_way_to_ask, refusal, ElicitingApprover, MAX_CHARS, MAX_DECLINES, MAX_LINES, MAX_LINE_CHARS, MAX_OPEN_QUESTIONS,
    REASON_BUSY, REASON_DECLINED_OFTEN, REASON_DISMISSED, REASON_HIDDEN_BLANKS, REASON_NO_ANSWER,
    REASON_NO_ELICITATION, REASON_PRODUCTION, REASON_SETTINGS_UNAVAILABLE, REASON_SWITCH_OFF, REASON_TOO_LONG,
};
use tablex_mcp::server::McpServer;

const INSERT: &str = "INSERT INTO items(label) VALUES ('written')";
const COUNT: &str = "SELECT count(*) AS n FROM items WHERE label = 'written'";

/// What the user (the client) answers to the question.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Said {
    Yes,
    No,
    Decline,
    Cancel,
    /// `accept`, but `run` is a string.
    Malformed,
}

impl Said {
    const ALL: [Said; 5] = [
        Said::Yes,
        Said::No,
        Said::Decline,
        Said::Cancel,
        Said::Malformed,
    ];

    fn reply(self) -> ElicitResult {
        match self {
            Said::Yes => accepted(true),
            Said::No => accepted(false),
            Said::Decline => ElicitResult::new(ElicitationAction::Decline),
            Said::Cancel => ElicitResult::new(ElicitationAction::Cancel),
            Said::Malformed => {
                ElicitResult::new(ElicitationAction::Accept).with_content(json!({"answer": "yes"}))
            }
        }
    }
}

/// The exact text the model gets for a write that was not asked about.
fn not_asked(reason: &str) -> String {
    format!("this SQL statement was not run, and the user was not asked: {}", refusal(reason))
}

/// The exact text the model gets for a write whose question got no usable answer.
fn not_run(reason: &str) -> String {
    format!("this SQL statement was not run: {reason}")
}

fn error_message(result: &rmcp::model::CallToolResult) -> String {
    let body: Value = serde_json::from_str(&text(result)).expect("the error is JSON");
    body["error"]["message"]
        .as_str()
        .expect("a message")
        .to_owned()
}

fn rows_written(result: &rmcp::model::CallToolResult) -> i64 {
    structured(result)["rows"][0][0]
        .as_str()
        .and_then(|n| n.parse().ok())
        .or_else(|| structured(result)["rows"][0][0].as_i64())
        .expect("a count")
}

// ---- the matrix ------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Cell {
    /// `true`: the settings of the current app (schema 7, no switch). `false`: those of an older app
    /// whose switch "Let MCP clients run changes" is off (schema 6), which still blocks changes.
    switch: bool,
    access: &'static str,
    production: bool,
    capability: bool,
    said: Said,
}

struct Outcome {
    code: String,
    message: String,
    questions: Vec<(String, Value)>,
    written: i64,
    approval: Option<String>,
}

async fn run_cell(cell: Cell) -> Outcome {
    let fixture = Fixture::new();
    fixture.set_mcp_writes(cell.switch);
    let conn = fixture.typed_connection(
        Some(cell.access),
        if cell.production { "production" } else { "" },
    );
    run_on(fixture, conn, cell.capability, cell.said, format!("{cell:?}")).await
}

/// One INSERT through `execute_query` on `conn`, with a client that answers `said`.
async fn run_on(fixture: Fixture, conn: Conn, capability: bool, said: Said, cell: String) -> Outcome {
    let asker = Asker::answering(capability, Ok(said.reply()));
    let seen = Arc::clone(&asker.seen);
    let harness = Harness::start_with(fixture, asker).await;
    let opened = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&opened), "ok", "{cell}: {}", text(&opened));
    let result = harness
        .call(
            "execute_query",
            json!({"connection_id": conn.id, "statement": INSERT}),
        )
        .await;
    let counted = harness
        .call(
            "execute_query",
            json!({"connection_id": conn.id, "statement": COUNT}),
        )
        .await;
    let approval = harness
        .fixture
        .audit_entries()
        .into_iter()
        .filter(|entry| entry.tool == "execute_query")
        .find_map(|entry| {
            entry
                .statement
                .as_deref()
                .filter(|statement| statement.starts_with("INSERT"))
                .and(Some(entry.approval))
        })
        .flatten();
    let outcome = Outcome {
        code: code(&result),
        message: if result.is_error == Some(true) {
            error_message(&result)
        } else {
            String::new()
        },
        questions: seen.asked(),
        written: rows_written(&counted),
        approval,
    };
    harness.client.cancel().await.unwrap();
    harness.server.await.unwrap().unwrap();
    outcome
}

/// What the matrix expects for `cell`: the first condition that fails, in the documented order.
fn expected(cell: Cell) -> (&'static str, String, usize, i64) {
    if cell.access != "read_write" {
        // A statement the access does not allow is refused by the policy before any approver.
        return ("agent.write_refused", String::new(), 0, 0);
    }
    let refused = |reason: &str| ("agent.write_refused", not_asked(reason), 0, 0);
    if !cell.switch {
        return refused(REASON_SWITCH_OFF);
    }
    if cell.production {
        return refused(REASON_PRODUCTION);
    }
    if !cell.capability {
        return refused(&no_way_to_ask(None));
    }
    match cell.said {
        Said::Yes => ("ok", String::new(), 1, 1),
        Said::Decline => (
            "agent.write_refused",
            "the user did not approve this SQL statement".into(),
            1,
            0,
        ),
                // The choice "Do not run it": the user chose not to, and the agent is told so.
        Said::No => (
            "agent.write_refused",
            format!("the user did not approve this SQL statement: {}", tablex_mcp::approver::chose_not_to(tablex_mcp::approver::ANSWER_TITLE)),
            1,
            0,
        ),
        Said::Cancel => (
            "agent.write_refused",
            format!("the user did not approve this SQL statement: {REASON_DISMISSED}"),
            1,
            0,
        ),
        Said::Malformed => ("agent.write_refused", not_run(REASON_NO_ANSWER), 1, 0),
    }
}

#[tokio::test]
async fn a_write_runs_exactly_once_in_the_one_allowed_cell_and_nowhere_else() {
    let mut allowed = 0;
    let mut cells = 0;
    for switch in [false, true] {
        for access in ["read", "read_write"] {
            for production in [false, true] {
                for capability in [false, true] {
                    for said in Said::ALL {
                        let cell = Cell {
                            switch,
                            access,
                            production,
                            capability,
                            said,
                        };
                        let outcome = run_cell(cell).await;
                        let (want_code, want_message, want_questions, want_written) =
                            expected(cell);
                        assert_eq!(outcome.code, want_code, "{cell:?}: {}", outcome.message);
                        if !want_message.is_empty() {
                            assert_eq!(outcome.message, want_message, "{cell:?}");
                        }
                        assert_eq!(outcome.questions.len(), want_questions, "{cell:?}");
                        assert_eq!(outcome.written, want_written, "{cell:?}: rows written");
                        cells += 1;
                        allowed += usize::from(outcome.written == 1);
                        if let Some((_, schema)) = outcome.questions.first() {
                            assert_eq!(
                                *schema,
                                json!({
                                    "type": "object",
                                    "properties": {
                                        "answer": {
                                            "type": "string",
                                            "title": "Run this statement?",
                                            "enum": ["do_not_run", "run"],
                                            "enumNames": ["Do not run it", "Run this statement"]
                                        }
                                    },
                                    "required": ["answer"]
                                }),
                                "{cell:?}"
                            );
                        }
                    }
                }
            }
        }
    }
    assert_eq!(cells, 80);
    assert_eq!(allowed, 1, "exactly one cell writes");
}

// ---- the default access, with no switch (schema 7) --------------------------------------------------

#[tokio::test]
async fn with_a_default_of_read_and_write_a_write_on_a_connection_with_no_setting_is_asked() {
    // (a) The settings of the current app: schema 7, no switch, the default access Read and write.
    // The connection sets no level itself and has no type, or a type that is not production.
    for type_id in ["", "development"] {
        let fixture = Fixture::new();
        fixture.set_default_access("read_write");
        let conn = fixture.typed_connection(None, type_id);
        let outcome = run_on(fixture, conn, true, Said::Yes, type_id.to_owned()).await;
        assert_eq!(outcome.code, "ok", "{type_id:?}: {}", outcome.message);
        assert_eq!(outcome.questions.len(), 1, "{type_id:?}: put to the user, once");
        assert_eq!(outcome.written, 1);
        assert_eq!(outcome.approval.as_deref(), Some("approved"));
        // A no changes nothing; so does a client that cannot be asked.
        let fixture = Fixture::new();
        fixture.set_default_access("read_write");
        let conn = fixture.typed_connection(None, type_id);
        let declined = run_on(fixture, conn, true, Said::Decline, type_id.to_owned()).await;
        assert_eq!((declined.code.as_str(), declined.questions.len(), declined.written), ("agent.write_refused", 1, 0));
        let fixture = Fixture::new();
        fixture.set_default_access("read_write");
        let conn = fixture.typed_connection(None, type_id);
        let no_client = run_on(fixture, conn, false, Said::Yes, type_id.to_owned()).await;
        assert_eq!(no_client.message, not_asked(&no_way_to_ask(None)));
        assert_eq!(no_client.written, 0);
    }
    // The default is read: a write is refused by the access, before any question.
    let fixture = Fixture::new();
    fixture.set_default_access("read");
    let conn = fixture.typed_connection(None, "");
    let outcome = run_on(fixture, conn, true, Said::Yes, "read".into()).await;
    assert_eq!((outcome.code.as_str(), outcome.questions.len(), outcome.written), ("agent.write_refused", 0, 0));
    // The connection's own level still wins over the default.
    let fixture = Fixture::new();
    fixture.set_default_access("read_write");
    let conn = fixture.typed_connection(Some("read"), "");
    let own = run_on(fixture, conn, true, Said::Yes, "own".into()).await;
    assert_eq!((own.code.as_str(), own.questions.len(), own.written), ("agent.write_refused", 0, 0));
}

#[tokio::test]
async fn a_default_of_read_and_write_changes_nothing_for_a_production_connection() {
    // (b) Production gets no default: with no level of its own the agent cannot even open a session.
    let fixture = Fixture::new();
    fixture.set_default_access("read_write");
    let conn = fixture.typed_connection(None, "production");
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(Said::Yes.reply()))).await;
    let listed = harness.call("list_connections", json!({})).await;
    assert!(text(&listed).contains("\"off\""), "{}", text(&listed));
    let opened = harness.call("open_session", json!({"connection_id": conn.id})).await;
    assert_eq!(opened.is_error, Some(true), "{}", text(&opened));
    harness.client.cancel().await.unwrap();
    harness.server.await.unwrap().unwrap();
    // With a level of its own it is as before: a write is refused for the type, never asked.
    let fixture = Fixture::new();
    fixture.set_default_access("read_write");
    let conn = fixture.typed_connection(Some("read_write"), "production");
    let outcome = run_on(fixture, conn, true, Said::Yes, "production".into()).await;
    assert_eq!(outcome.message, not_asked(REASON_PRODUCTION));
    assert_eq!((outcome.questions.len(), outcome.written), (0, 0));
}

#[tokio::test]
async fn settings_of_an_older_app_with_the_switch_off_still_refuse_a_write_on_a_read_write_connection() {
    // (c) Schema 6 with `mcp_writes: false`: refused, naming the switch of that app, even with a
    // default of Read and write and a connection of its own Read and write.
    let fixture = Fixture::new();
    fixture.write_settings(r#"{"schema_version": 6, "ai": {"mcp_writes": false}}"#);
    let conn = fixture.typed_connection(Some("read_write"), "");
    let outcome = run_on(fixture, conn, true, Said::Yes, "schema 6".into()).await;
    assert_eq!(outcome.message, not_asked(REASON_SWITCH_OFF));
    assert_eq!((outcome.questions.len(), outcome.written), (0, 0));
    // The same file at schema 7: an explicit `false` (the old switch turned off after a downgrade and
    // back) is still the user's "no"; a file with the key `true`, or none, asks as usual.
    let fixture = Fixture::new();
    fixture.write_settings(r#"{"schema_version": 7, "ai": {"mcp_writes": false}}"#);
    let conn = fixture.typed_connection(Some("read_write"), "");
    let outcome = run_on(fixture, conn, true, Said::Yes, "schema 7".into()).await;
    assert_eq!(outcome.message, not_asked(REASON_SWITCH_OFF));
    assert_eq!((outcome.questions.len(), outcome.written), (0, 0));
    for file in [r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#, r#"{"schema_version": 7, "ai": {}}"#] {
        let fixture = Fixture::new();
        fixture.write_settings(file);
        let conn = fixture.typed_connection(Some("read_write"), "");
        let outcome = run_on(fixture, conn, true, Said::Yes, "schema 7".into()).await;
        assert_eq!((outcome.code.as_str(), outcome.questions.len(), outcome.written), ("ok", 1, 1), "{file}");
    }
}

#[tokio::test]
async fn the_audit_entries_say_how_each_write_was_decided() {
    let base = Cell {
        switch: true,
        access: "read_write",
        production: false,
        capability: true,
        said: Said::Yes,
    };
    let approved = run_cell(base).await;
    assert_eq!(approved.approval.as_deref(), Some("approved"));
    let declined = run_cell(Cell { said: Said::Decline, ..base }).await;
    assert_eq!(declined.approval.as_deref(), Some("declined"));
    let chose_not = run_cell(Cell { said: Said::No, ..base }).await;
    assert_eq!(
        chose_not.approval,
        Some(format!("declined: {}", tablex_mcp::approver::chose_not_to(tablex_mcp::approver::ANSWER_TITLE)))
    );
    let dismissed = run_cell(Cell {
        said: Said::Cancel,
        ..base
    })
    .await;
    assert_eq!(
        dismissed.approval.as_deref(),
        Some(format!("declined: {REASON_DISMISSED}").as_str())
    );
    let unreadable = run_cell(Cell {
        said: Said::Malformed,
        ..base
    })
    .await;
    assert_eq!(
        unreadable.approval.as_deref(),
        Some(format!("unanswered: {REASON_NO_ANSWER}").as_str())
    );
    let no_ask = no_way_to_ask(None);
    for (cell, reason) in [
        (
            Cell {
                switch: false,
                ..base
            },
            REASON_SWITCH_OFF,
        ),
        (
            Cell {
                production: true,
                ..base
            },
            REASON_PRODUCTION,
        ),
        (
            Cell {
                capability: false,
                ..base
            },
            no_ask.as_str(),
        ),
    ] {
        let refused = run_cell(cell).await;
        assert_eq!(
            refused.approval.as_deref(),
            Some(format!("refused: {}", engine_agent::sanitize::clean(&refusal(reason), 200)).as_str()),
            "{cell:?}"
        );
    }
    // A read asks nothing and records no approval.
    let read = run_cell(Cell {
        access: "read",
        ..base
    })
    .await;
    assert_eq!(read.approval, None);
}

// ---- fake sessions: what reaches the driver --------------------------------------------------------

struct Rig {
    harness: Harness<Asker>,
    sessions: Vec<Arc<FakeSession>>,
    ids: Vec<String>,
}

/// `count` connections with access `read_write` over fake sessions that record statements, and the
/// real approver reading the fixture's `settings.json`.
fn fakes(
    count: usize,
    production: bool,
    switch: bool,
) -> (Fixture, Vec<Arc<FakeSession>>, Vec<String>) {
    let ids: Vec<String> = (0..count).map(|n| format!("fake-{n}")).collect();
    let sessions: Vec<Arc<FakeSession>> = ids
        .iter()
        .map(|_| {
            let session = FakeSession::new(ConnectionKind::Sqlite).arc();
            session.set_behavior(Behavior::Affected(1));
            session
        })
        .collect();
    let catalog = FakeCatalog::new(
        ids.iter()
            .map(|id| {
                let mut info = connection(id, ConnectionKind::Sqlite, Some(AgentAccess::ReadWrite));
                info.name = format!("Warehouse {id}");
                info.connection_type.production = production;
                info
            })
            .collect(),
    );
    let provider = MapProvider(
        ids.iter()
            .cloned()
            .zip(sessions.iter().map(|s| Arc::clone(s) as Arc<dyn Session>))
            .collect(),
    );
    let fixture = Fixture::with_layer_in(|audit, settings| {
        ToolLayer::new(catalog, Arc::new(provider), "mcp")
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit)
    });
    fixture.set_mcp_writes(switch);
    (fixture, sessions, ids)
}

/// [`fakes`] with a client that answers as `asker` does.
async fn rig(count: usize, production: bool, switch: bool, asker: Asker) -> Rig {
    let (fixture, sessions, ids) = fakes(count, production, switch);
    let harness = Harness::start_with(fixture, asker).await;
    for id in &ids {
        let opened = harness
            .call("open_session", json!({"connection_id": id}))
            .await;
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    Rig {
        harness,
        sessions,
        ids,
    }
}

impl Rig {
    async fn execute(&self, n: usize, statement: &str) -> rmcp::model::CallToolResult {
        self.harness
            .call(
                "execute_query",
                json!({"connection_id": self.ids[n], "statement": statement}),
            )
            .await
    }

    fn runs(&self, n: usize) -> Vec<String> {
        self.sessions[n]
            .runs
            .lock()
            .unwrap()
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }

    async fn finish(self) {
        let Harness { client, server, .. } = self.harness;
        client.cancel().await.unwrap();
        server.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn the_statement_in_the_question_is_the_one_that_reaches_the_driver() {
    let statement = "UPDATE items\r\n\tSET label = 'caf\u{e9} \u{4e2d}\u{6587} \u{1f600} \"q\" \\ ''x'''\nWHERE id = 1";
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = rig.execute(0, statement).await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let asked = rig.harness.client.service().seen.asked();
    assert_eq!(asked.len(), 1);
    let message = &asked[0].0;
    // Nothing is spelled: the message carries the statement as it is, in a code block that is
    // the last thing in it.
    assert!(
        message.ends_with(&format!("\n```\n{statement}\n```")),
        "{message}"
    );
    assert!(!message.contains("<U+"), "{message}");
    assert_eq!(rig.runs(0), [statement.to_owned()], "byte for byte");
    // The question names the connection, its type and the database.
    assert!(
        message.contains("Connection: `Warehouse fake-0` (`Development`)\n"),
        "{message}"
    );
    assert!(message.contains("Database: `main`\n"), "{message}");
    assert!(
        message.contains(&format!(
            "The statement has 3 lines and {} characters.\n",
            statement.chars().count()
        )),
        "{message}"
    );
    rig.finish().await;
}

#[tokio::test]
async fn characters_without_a_shape_and_right_to_left_letters_are_spelled_but_the_original_runs() {
    let statement =
        "UPDATE items SET label = 'a\u{202E}b\u{200B}c\u{7}' -- \u{5D0}\u{5D1}\nWHERE id = 1";
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = rig.execute(0, statement).await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let message = rig.harness.client.service().seen.asked()[0].0.clone();
    for ch in ['\u{202E}', '\u{200B}', '\u{7}'] {
        assert!(!message.contains(ch), "U+{:04X} was drawn", ch as u32);
    }
    assert!(
        message.contains("'a<U+202E>b<U+200B>c<U+0007>'"),
        "{message}"
    );
    assert!(
        message.contains("3 characters without a shape of its own"),
        "{message}"
    );
    assert!(message.contains("written as <U+XXXX> below"), "{message}");
    assert!(
        message.contains("The statement that runs has the original characters."),
        "{message}"
    );
    assert!(message.contains("right-to-left letters"), "{message}");
    // The right-to-left letters themselves stay, announced above the statement.
    assert!(message.contains("\u{5D0}\u{5D1}"), "{message}");
    assert_eq!(
        rig.runs(0),
        [statement.to_owned()],
        "the original statement ran"
    );
    rig.finish().await;
}

#[tokio::test]
async fn a_bare_carriage_return_is_refused_before_any_question() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = rig
        .execute(0, "UPDATE items SET label = 'x' -- note\rWHERE id = 1")
        .await;
    assert_eq!(
        code(&result),
        "agent.invalid_arguments",
        "{}",
        text(&result)
    );
    assert!(rig.harness.client.service().seen.asked().is_empty());
    assert!(rig.runs(0).is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn a_dangerous_statement_is_asked_about_off_production_only_when_flagged() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    // Without the flag the model is told how to ask; nobody is asked yet.
    for dangerous in [json!(null), json!(false)] {
        let mut arguments = json!({"connection_id": rig.ids[0], "statement": "DELETE FROM items"});
        if !dangerous.is_null() {
            arguments["dangerous"] = dangerous.clone();
        }
        let result = rig.harness.call("execute_query", arguments).await;
        assert_eq!(code(&result), "agent.dangerous_refused", "{dangerous}: {}", text(&result));
        assert!(
            error_message(&result).contains("call again with dangerous=true"),
            "{dangerous}: {}",
            error_message(&result)
        );
    }
    assert!(rig.harness.client.service().seen.asked().is_empty());
    assert!(rig.runs(0).is_empty());
    // With it the user is asked, with the danger said first and its reason named, and an
    // approval runs the statement.
    let result = rig
        .harness
        .call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": "DROP TABLE items", "dangerous": true}),
        )
        .await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let asked = rig.harness.client.service().seen.asked();
    assert_eq!(asked.len(), 1);
    let message = &asked[0].0;
    assert!(message.starts_with("DANGEROUS: an MCP client asks to run a statement"), "{message}");
    assert!(
        message.contains("Why it is dangerous: DROP permanently removes database objects"),
        "{message}"
    );
    assert!(message.ends_with("\n```\nDROP TABLE items\n```"), "{message}");
    assert_eq!(rig.runs(0), ["DROP TABLE items".to_owned()]);
    rig.finish().await;
}

#[tokio::test]
async fn a_declined_dangerous_statement_does_not_run() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(false)))).await;
    let result = rig
        .harness
        .call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": "TRUNCATE items", "dangerous": true}),
        )
        .await;
    assert_eq!(code(&result), "agent.write_refused", "{}", text(&result));
    assert_eq!(rig.harness.client.service().seen.asked().len(), 1);
    assert!(rig.runs(0).is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn a_dangerous_statement_on_a_production_connection_is_refused_without_asking() {
    let rig = rig(1, true, true, Asker::answering(true, Ok(accepted(true)))).await;
    for dangerous in [json!(null), json!(false), json!(true)] {
        let mut arguments = json!({"connection_id": rig.ids[0], "statement": "DELETE FROM items"});
        if !dangerous.is_null() {
            arguments["dangerous"] = dangerous.clone();
        }
        let result = rig.harness.call("execute_query", arguments).await;
        assert_eq!(
            code(&result),
            "agent.dangerous_refused",
            "{dangerous}: {}",
            text(&result)
        );
        // Said at once, with the reason and what to do: no "call again with the flag" first.
        assert_eq!(
            error_message(&result),
            "this SQL statement is dangerous (DELETE without WHERE removes every row of the table) \
             and mcp does not run dangerous statements on a production connection (this one is of \
             type `Development`); do not retry it or reword it, ask the user to run it themselves \
             in the tablex app",
            "{dangerous}"
        );
    }
    let drop_table = rig.execute(0, "DROP TABLE items").await;
    assert_eq!(code(&drop_table), "agent.dangerous_refused");
    assert!(rig.harness.client.service().seen.asked().is_empty());
    assert!(rig.runs(0).is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn additive_ddl_is_an_ordinary_write_asked_once_and_destructive_ddl_is_dangerous() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let add = "ALTER TABLE items ADD COLUMN biz_domain TEXT";
    let result = rig.execute(0, add).await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let asked = rig.harness.client.service().seen.asked();
    assert_eq!(asked.len(), 1, "asked once");
    assert!(asked[0].0.starts_with("An MCP client asks to run a statement that changes data."), "{}", asked[0].0);
    assert!(!asked[0].0.contains("DANGEROUS"), "{}", asked[0].0);
    assert_eq!(rig.runs(0), [add.to_owned()]);
    // Dropping a column is dangerous: refused without the flag, nobody is asked.
    let drop = rig.execute(0, "ALTER TABLE items DROP COLUMN biz_domain").await;
    assert_eq!(code(&drop), "agent.dangerous_refused", "{}", text(&drop));
    assert_eq!(rig.harness.client.service().seen.asked().len(), 1);
    assert_eq!(rig.runs(0).len(), 1);
    // A script of additive DDL is one question; a destructive statement refuses the script.
    let script = rig
        .harness
        .call(
            "execute_batch",
            json!({"connection_id": rig.ids[0], "script": "CREATE TABLE a (x int); ALTER TABLE a ADD COLUMN y int"}),
        )
        .await;
    assert_eq!(code(&script), "ok", "{}", text(&script));
    assert_eq!(rig.harness.client.service().seen.asked().len(), 2);
    let script = rig
        .harness
        .call(
            "execute_batch",
            json!({"connection_id": rig.ids[0], "script": "ALTER TABLE a DROP COLUMN y"}),
        )
        .await;
    assert_eq!(code(&script), "agent.dangerous_refused", "{}", text(&script));
    assert_eq!(rig.harness.client.service().seen.asked().len(), 2);
    rig.finish().await;
}

#[tokio::test]
async fn ddl_on_a_connection_with_read_access_is_a_refused_write() {
    let fixture = Fixture::new();
    fixture.set_mcp_writes(true);
    let conn = fixture.typed_connection(Some("read"), "");
    let asker = Asker::answering(true, Ok(accepted(true)));
    let seen = Arc::clone(&asker.seen);
    let harness = Harness::start_with(fixture, asker).await;
    let opened = harness
        .call("open_session", json!({"connection_id": conn.id}))
        .await;
    assert_eq!(code(&opened), "ok", "{}", text(&opened));
    for (statement, dangerous) in [
        ("ALTER TABLE items ADD COLUMN biz_domain TEXT", false),
        ("ALTER TABLE items DROP COLUMN label", true),
    ] {
        let result = harness
            .call(
                "execute_query",
                json!({"connection_id": conn.id, "statement": statement, "dangerous": dangerous}),
            )
            .await;
        assert_eq!(code(&result), "agent.write_refused", "{statement}: {}", text(&result));
    }
    assert!(seen.asked().is_empty());
    harness.client.cancel().await.unwrap();
    harness.server.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_read_never_asks() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let read = rig.execute(0, "SELECT 1").await;
    assert_eq!(code(&read), "ok", "{}", text(&read));
    assert!(rig.harness.client.service().seen.asked().is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn an_unusable_answer_is_not_a_yes() {
    let accept = |content: Option<Value>| {
        let mut result = ElicitResult::new(ElicitationAction::Accept);
        result.content = content;
        Ok(result)
    };
    let no_choice = tablex_mcp::approver::no_choice(tablex_mcp::approver::ANSWER_TITLE);
    // Accepted with no choice: nothing ran, and the agent is told what to ask the user to do.
    for (label, answer) in [
        ("accept without content", accept(None)),
        ("answer is null", accept(Some(json!({"answer": null})))),
        ("another key", accept(Some(json!({"yes": true})))),
        ("the boolean of the old form", accept(Some(json!({"run": true})))),
        ("content is a bool", accept(Some(json!(true)))),
    ] {
        let rig = rig(2, false, true, Asker::answering(true, answer)).await;
        for _ in 0..MAX_DECLINES {
            let result = rig.execute(0, INSERT).await;
            assert_eq!(code(&result), "agent.write_refused", "{label}");
            assert_eq!(
                error_message(&result),
                format!("the user did not approve this SQL statement: {no_choice}"),
                "{label}"
            );
        }
        assert!(rig.runs(0).is_empty(), "{label}");
        // It counts like a decline: the same limit, and no question once it is reached.
        let closed = rig.execute(0, INSERT).await;
        assert_eq!(error_message(&closed), not_asked(REASON_DECLINED_OFTEN), "{label}");
        assert_eq!(rig.harness.client.service().seen.asked().len(), MAX_DECLINES as usize, "{label}");
        rig.finish().await;
    }
    for (label, answer) in [
        ("answer is 1", accept(Some(json!({"answer": 1})))),
        ("answer is a boolean", accept(Some(json!({"answer": true})))),
        ("answer is another word", accept(Some(json!({"answer": "yes"})))),
        (
            "an error from the client",
            Err(McpError::internal_error("boom", None)),
        ),
    ] {
        let rig = rig(2, false, true, Asker::answering(true, answer)).await;
        // Each is audited `unanswered`, and worded as what it is.
        for _ in 0..MAX_DECLINES {
            let result = rig.execute(0, INSERT).await;
            assert_eq!(code(&result), "agent.write_refused", "{label}");
            assert_eq!(error_message(&result), not_run(REASON_NO_ANSWER), "{label}");
        }
        assert!(rig.runs(0).is_empty(), "{label}");
        let unanswered = format!("unanswered: {REASON_NO_ANSWER}");
        assert_eq!(
            rig.harness
                .fixture
                .audit_entries()
                .iter()
                .filter(|entry| entry.approval.as_deref() == Some(unanswered.as_str()))
                .count(),
            MAX_DECLINES as usize,
            "{label}"
        );
        // A client that turns a dismissal into one of these cannot make asking free: the same
        // limit as for declines, and no question is sent once it is reached.
        let closed = rig.execute(0, INSERT).await;
        assert_eq!(
            error_message(&closed),
            not_asked(REASON_DECLINED_OFTEN),
            "{label}"
        );
        assert_eq!(
            rig.harness.client.service().seen.asked().len(),
            MAX_DECLINES as usize,
            "{label}: no fourth question"
        );
        // Another connection is unaffected.
        let other = rig.execute(1, INSERT).await;
        assert_eq!(error_message(&other), not_run(REASON_NO_ANSWER), "{label}");
        assert_eq!(
            rig.harness.client.service().seen.asked().len(),
            MAX_DECLINES as usize + 1,
            "{label}"
        );
        rig.finish().await;
    }
}

#[tokio::test]
async fn text_hidden_behind_blanks_is_not_asked() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let head = "UPDATE t SET x=1 WHERE id=5";
    let hidden = format!("{head}{}OR 1=1", " ".repeat(350));
    let tabbed = format!("{head}{}OR 1=1", "\t".repeat(4));
    for statement in [&hidden, &tabbed] {
        let result = rig.execute(0, statement).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert_eq!(error_message(&result), not_asked(REASON_HIDDEN_BLANKS));
    }
    assert!(rig.harness.client.service().seen.asked().is_empty());
    // At the limit, and with indentation only, it is asked.
    let at_limit = format!("{head}{}OR 1=1", "\t".repeat(3));
    let indented = format!("{}{head}", " ".repeat(100));
    for statement in [&at_limit, &indented] {
        assert_eq!(code(&rig.execute(0, statement).await), "ok");
    }
    assert_eq!(rig.harness.client.service().seen.asked().len(), 2);
    rig.finish().await;
}

// ---- the settings are read at each write -----------------------------------------------------------

#[tokio::test]
async fn the_settings_are_read_at_the_time_of_each_write_and_fail_closed() {
    let rig = rig(1, false, false, Asker::answering(true, Ok(accepted(true)))).await;
    let fixture = &rig.harness.fixture;
    let refused = |result: &rmcp::model::CallToolResult| {
        assert_eq!(code(result), "agent.write_refused", "{}", text(result));
        error_message(result)
    };
    // The rig starts with the file of an older app whose switch is off: nothing is asked.
    assert_eq!(
        refused(&rig.execute(0, INSERT).await),
        not_asked(REASON_SWITCH_OFF)
    );
    // A file of the current app (schema 7, no switch): the next write is asked and runs, without
    // restarting anything.
    fixture.write_settings(r#"{"schema_version": 7, "ai": {}}"#);
    assert_eq!(code(&rig.execute(0, INSERT).await), "ok");
    assert_eq!(rig.runs(0).len(), 1);
    // A file of an older app (schema 6) with its switch off: refused again, naming that switch.
    fixture.set_mcp_writes(false);
    assert_eq!(
        refused(&rig.execute(0, INSERT).await),
        not_asked(REASON_SWITCH_OFF)
    );
    // ...and with it on, asked again. From schema 7 the switch is ignored when it is `true` or
    // missing, but an explicit `false` still refuses.
    fixture.write_settings(r#"{"schema_version": 6, "ai": {"mcp_writes": true}}"#);
    assert_eq!(code(&rig.execute(0, INSERT).await), "ok");
    fixture.write_settings(r#"{"schema_version": 7, "ai": {"mcp_writes": false}}"#);
    assert_eq!(
        refused(&rig.execute(0, INSERT).await),
        not_asked(REASON_SWITCH_OFF)
    );
    fixture.write_settings(r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#);
    assert_eq!(code(&rig.execute(0, INSERT).await), "ok");
    assert_eq!(rig.runs(0).len(), 3);
    // Garbage and the wrong type read as unavailable; a file with no (or no usable) version and no
    // switch is an old app's, with the switch off.
    for (bad, reason) in [
        ("{ not json", REASON_SETTINGS_UNAVAILABLE),
        ("", REASON_SETTINGS_UNAVAILABLE),
        (r#"{"ai": {"mcp_writes": "true"}}"#, REASON_SETTINGS_UNAVAILABLE),
        (r#"{"schema_version": 7, "ai": {"mcp_writes": 1}}"#, REASON_SETTINGS_UNAVAILABLE),
        (r#"{"schema_version": 7, "ai": "on"}"#, REASON_SETTINGS_UNAVAILABLE),
        (r#"{"ai": {}}"#, REASON_SWITCH_OFF),
        ("{}", REASON_SWITCH_OFF),
        (r#"{"schema_version": "7", "ai": {}}"#, REASON_SWITCH_OFF),
    ] {
        fixture.write_settings(bad);
        assert_eq!(
            refused(&rig.execute(0, INSERT).await),
            not_asked(reason),
            "{bad:?}"
        );
    }
    std::fs::remove_file(fixture.settings_path()).unwrap();
    assert_eq!(
        refused(&rig.execute(0, INSERT).await),
        not_asked(REASON_SETTINGS_UNAVAILABLE)
    );
    fixture.set_mcp_writes(true);
    assert_eq!(code(&rig.execute(0, INSERT).await), "ok");
    assert_eq!(rig.runs(0).len(), 4, "four writes ran, the others were refused");
    assert_eq!(rig.harness.client.service().seen.asked().len(), 4);
    rig.finish().await;
}

// ---- cancellation and shutdown while a question waits ----------------------------------------------

#[tokio::test]
async fn cancelling_the_request_while_the_question_waits_stops_it_and_leaves_nothing_behind() {
    let (asker, mut desk) = Asker::manual(true);
    let seen = Arc::clone(&asker.seen);
    let rig = rig(1, false, true, asker).await;
    let write = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": INSERT}),
        )
        .await;
    let question = within("a question", desk.recv())
        .await
        .expect("the question arrives");
    // The protocol is not blocked while the user thinks: another request is answered.
    let listed = rig.harness.call("list_connections", json!({})).await;
    assert_eq!(code(&listed), "ok");
    assert!(!question.cancelled.is_cancelled());

    write.cancel(Some("test".into())).await.unwrap();
    // The client is told the question is withdrawn.
    poll("the withdrawn question", async || {
        question.cancelled.is_cancelled().then_some(())
    })
    .await;
    poll("the cancel notification", async || {
        (seen.cancels() >= 1).then_some(())
    })
    .await;
    assert_eq!(seen.cancels(), 1);
    // The call ended as cancelled, in the audit log, and nothing ran.
    let entry = poll("the cancelled call in the audit log", async || {
        rig.harness
            .fixture
            .audit_entries()
            .into_iter()
            .find(|entry| entry.tool == "execute_query" && entry.outcome == "agent.cancelled")
    })
    .await;
    assert_eq!(entry.approval.as_deref(), Some("cancelled"));
    assert!(rig.runs(0).is_empty());
    // A late answer to the withdrawn question changes nothing (raw frames test it further down).
    question.accept(true);
    assert!(rig.runs(0).is_empty());

    // The next write works (no permit, thread or hook was left behind) and is a question of its own.
    let again = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": INSERT}),
        )
        .await;
    let second = within("a question", desk.recv())
        .await
        .expect("a new question");
    second.accept(true);
    let answered = unwrap_call(
        within("the answer to the call", again.await_response())
            .await
            .unwrap(),
    );
    assert_eq!(code(&answered), "ok", "{}", text(&answered));
    assert_eq!(rig.runs(0).len(), 1);
    let Harness { client, server, .. } = rig.harness;
    client.cancel().await.unwrap();
    // `serve` waits for every call's permit, so a call that never returned would show here.
    server.await.unwrap().unwrap();
}

/// Far over anything the shutdown takes. The tests do not rely on it being short: a question that
/// held the server would end it with `TeardownIncomplete` at the shutdown deadline (5 s), which
/// `server.await.unwrap().unwrap()` rejects, whatever the clock says.
const PROMPT: Duration = Duration::from_secs(30);

#[tokio::test]
async fn the_client_closing_its_input_while_the_question_waits_ends_the_server_in_time() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(1, false, true, asker).await;
    let write = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": INSERT}),
        )
        .await;
    let _question = within("a question", desk.recv())
        .await
        .expect("the question arrives");
    let Harness {
        client,
        server,
        fixture,
    } = rig.harness;
    drop(write);
    tokio::time::timeout(PROMPT, async {
        client.cancel().await.unwrap();
        server.await.unwrap().unwrap();
    })
    .await
    .expect("the server ends while the question is open");
    // The call ended without running. How exactly depends on what reaches the server first as the
    // client goes: its end of input (the call is cancelled) or the answer its own shutdown gives
    // to the open question (dismissed, so a decline); both are "not run".
    let entries = fixture.audit_entries();
    let write = entries
        .iter()
        .find(|entry| entry.tool == "execute_query")
        .expect("the write left an audit line");
    assert!(
        matches!(
            write.outcome.as_str(),
            "agent.cancelled" | "agent.write_refused"
        ),
        "{entries:?}"
    );
    assert!(
        write.approval.as_deref().is_some_and(|approval| {
            approval == "cancelled"
                || approval.starts_with("declined")
                || approval.starts_with("refused")
        }),
        "{entries:?}"
    );
    assert!(rig.sessions[0].runs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_stop_signal_while_the_question_waits_ends_the_server_in_time() {
    let (asker, mut desk) = Asker::manual(true);
    let ids = fake_ids(1);
    let session = FakeSession::new(ConnectionKind::Sqlite).arc();
    session.set_behavior(Behavior::Affected(1));
    let catalog = FakeCatalog::new(vec![connection(
        &ids[0],
        ConnectionKind::Sqlite,
        Some(AgentAccess::ReadWrite),
    )]);
    let provider = MapProvider(
        [(ids[0].clone(), Arc::clone(&session) as Arc<dyn Session>)]
            .into_iter()
            .collect(),
    );
    let fixture = Fixture::with_layer_in(|audit, settings| {
        ToolLayer::new(catalog, Arc::new(provider), "mcp")
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit)
    });
    fixture.set_mcp_writes(true);
    let server = McpServer::new(Arc::clone(&fixture.layer)).unwrap();
    let (client_end, reader, writer) = pipe();
    let (signal, mut signals) = tokio::sync::mpsc::unbounded_channel::<()>();
    let serve = server.serve(reader, writer, async || {
        signals.recv().await;
    });
    let scenario = async {
        use rmcp::ServiceExt;
        let client = asker.serve(client_end).await.unwrap();
        let opened = client
            .peer()
            .call_tool(params("open_session", json!({"connection_id": ids[0]})))
            .await
            .unwrap();
        assert_eq!(code(&opened), "ok");
        let peer = client.peer().clone();
        let id = ids[0].clone();
        let call = tokio::spawn(async move {
            peer.call_tool(params(
                "execute_query",
                json!({"connection_id": id, "statement": INSERT}),
            ))
            .await
        });
        let _question = within("a question", desk.recv())
            .await
            .expect("the question arrives");
        signal.send(()).unwrap();
        // The pipe stays open: only the signal ends it.
        (client, call)
    };
    let (result, (client, call)) =
        tokio::time::timeout(PROMPT, async { tokio::join!(serve, scenario) })
            .await
            .expect("the stop ends the server while the question is open");
    assert!(
        matches!(result, Ok(())),
        "{result:?}: not TeardownIncomplete"
    );
    assert!(session.runs.lock().unwrap().is_empty());
    drop(client);
    let _ = call.await;
}

// ---- two writes at once ----------------------------------------------------------------------------

#[tokio::test]
async fn two_writes_at_once_each_have_their_own_question_and_the_answers_do_not_cross() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(2, false, true, asker).await;
    let first_statement = "INSERT INTO items(label) VALUES ('first')";
    let second_statement = "INSERT INTO items(label) VALUES ('second')";
    let first = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[0], "statement": first_statement}),
        )
        .await;
    let second = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[1], "statement": second_statement}),
        )
        .await;
    let mut questions = vec![
        within("a question", desk.recv()).await.unwrap(),
        within("a question", desk.recv()).await.unwrap(),
    ];
    // Whatever order they arrived in, find each by what it asks.
    questions.sort_by_key(|question| question.message.contains("'second'"));
    let [for_first, for_second] = <[Question; 2]>::try_from(questions).ok().unwrap();
    assert!(
        for_first.message.contains("Warehouse fake-0") && for_first.message.contains("'first'")
    );
    assert!(
        for_second.message.contains("Warehouse fake-1") && for_second.message.contains("'second'")
    );
    // Answered in the opposite order: the second says yes first, the first says no afterwards.
    for_second.accept(true);
    let second = unwrap_call(
        within("the answer to the call", second.await_response())
            .await
            .unwrap(),
    );
    assert_eq!(code(&second), "ok", "{}", text(&second));
    assert!(rig.runs(0).is_empty(), "the first is still waiting");
    for_first.accept(false);
    let first = unwrap_call(
        within("the answer to the call", first.await_response())
            .await
            .unwrap(),
    );
    assert_eq!(code(&first), "agent.write_refused");
    assert_eq!(rig.runs(0), Vec::<String>::new());
    assert_eq!(rig.runs(1), [second_statement.to_owned()]);
    rig.finish().await;
}

// ---- what the model is told ------------------------------------------------------------------------

#[tokio::test]
async fn the_instructions_and_tool_descriptions_say_a_write_may_be_asked_and_a_refusal_is_final() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let info = rig.harness.client.peer_info().expect("server info");
    let instructions = info.instructions.clone().expect("instructions");
    assert!(
        instructions.contains("may be put to the user for approval"),
        "{instructions}"
    );
    assert!(instructions.contains("final"), "{instructions}");
    assert!(
        instructions.contains("Settings \u{2192} AI") && !instructions.contains("Settings >"),
        "{instructions}"
    );
    assert!(instructions.contains("production type"), "{instructions}");
    assert!(
        !instructions.contains("Writes are refused by this server"),
        "{instructions}"
    );
    let tools = rig.harness.peer().list_all_tools().await.unwrap();
    for name in ["execute_query", "execute_redis_command"] {
        let tool = tools.iter().find(|tool| tool.name == name).expect(name);
        let description = tool.description.as_deref().unwrap_or_default();
        assert!(
            description.contains("may be put to the user for approval"),
            "{name}: {description}"
        );
        assert!(description.contains("final"), "{name}: {description}");
        assert!(
            description.contains("do not retry"),
            "{name}: {description}"
        );
    }
    rig.finish().await;
}

#[test]
fn the_fixed_reasons_read_as_the_documentation_says() {
    // The setting is named as the app shows it (locales/settings.yml), the way out is named, and
    // every refusal ends with the same sentence once.
    // Only a settings file written by an older app (schema below 7) still has the switch.
    assert_eq!(
        REASON_SWITCH_OFF,
        "changes through the MCP server are turned off in tablex. Ask the user to turn on \"Let MCP clients run \
         changes\" in the tablex app (Settings \u{2192} AI Assistant \u{2192} MCP server), or to update tablex, \
         which no longer has this switch"
    );
    assert!(REASON_SETTINGS_UNAVAILABLE.contains("Ask the user to open the tablex app once"));
    assert!(REASON_PRODUCTION.contains("Ask the user to make this change in the tablex app"));
    assert!(REASON_NO_ELICITATION.contains("does not support MCP elicitation"));
    assert!(no_way_to_ask(None).contains("open the tablex app and try again, or to use a client that supports MCP elicitation"));
    for reason in [REASON_SWITCH_OFF, REASON_SETTINGS_UNAVAILABLE, REASON_PRODUCTION, REASON_DECLINED_OFTEN, REASON_TOO_LONG, REASON_BUSY] {
        assert_eq!(refusal(reason).matches(tablex_mcp::approver::OUTSIDE_TABLEX).count(), 1, "{reason}");
    }
}

// ---- hand-written frames ---------------------------------------------------------------------------

/// A hand-driven client: frames as lines of JSON over the pipe, against the real server over the
/// fake connections of [`fakes`].
struct Raw {
    reader: tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    /// Kept alive for its temporary directory and settings file.
    #[allow(dead_code)]
    fixture: Fixture,
    sessions: Vec<Arc<FakeSession>>,
    ids: Vec<String>,
    server: tokio::task::JoinHandle<Result<(), tablex_mcp::transport::ServeError>>,
    next_id: i64,
}

impl Raw {
    /// Handshakes with `capabilities` and opens the sessions of `count` connections.
    async fn start(count: usize, capabilities: Value) -> Self {
        let (fixture, sessions, ids) = fakes(count, false, true);
        let (client_end, reader, writer) = pipe();
        let server = tokio::spawn(
            McpServer::new(Arc::clone(&fixture.layer))
                .unwrap()
                .serve(reader, writer, never),
        );
        let (read, write) = tokio::io::split(client_end);
        let mut raw = Self {
            reader: tokio::io::BufReader::new(read),
            writer: write,
            fixture,
            sessions,
            ids,
            server,
            next_id: 1,
        };
        let id = raw
            .request(
                "initialize",
                json!({
            "protocolVersion": "2025-06-18", "capabilities": capabilities,
            "clientInfo": {"name": "raw", "version": "0"}}),
            )
            .await;
        raw.response(id).await;
        raw.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        for n in 0..count {
            let id = raw
                .request(
                    "tools/call",
                    json!({"name": "open_session",
                    "arguments": {"connection_id": raw.ids[n]}}),
                )
                .await;
            raw.response(id).await;
        }
        raw
    }

    async fn send(&mut self, frame: Value) {
        use tokio::io::AsyncWriteExt;
        let mut line = frame.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    /// Sends a request and returns its id.
    async fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        id
    }

    async fn write(&mut self, n: usize, statement: &str) -> i64 {
        let arguments = json!({"connection_id": self.ids[n], "statement": statement});
        self.request(
            "tools/call",
            json!({"name": "execute_query", "arguments": arguments}),
        )
        .await
    }

    async fn next(&mut self) -> Value {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(30), self.reader.read_line(&mut line))
            .await
            .expect("a frame in time")
            .unwrap();
        assert!(read > 0, "the server closed the pipe");
        serde_json::from_str(&line).unwrap()
    }

    /// The next frame that is a response to `id` (anything else before it is skipped).
    async fn response(&mut self, id: i64) -> Value {
        loop {
            let frame = self.next().await;
            if frame["id"] == id && frame.get("method").is_none() {
                return frame;
            }
        }
    }

    /// The next frame that is the request `method`.
    async fn server_request(&mut self, method: &str) -> Value {
        loop {
            let frame = self.next().await;
            if frame["method"] == method {
                return frame;
            }
        }
    }

    /// A tool result by request id.
    async fn result(&mut self, id: i64) -> rmcp::model::CallToolResult {
        let answer = self.response(id).await;
        serde_json::from_value(answer["result"].clone()).unwrap_or_else(|_| panic!("{answer}"))
    }

    fn runs(&self, n: usize) -> usize {
        self.sessions[n].runs.lock().unwrap().len()
    }

    /// Waits until the server has handled everything sent so far (a ping answered after it).
    async fn sync(&mut self) {
        let id = self.request("ping", json!({})).await;
        self.response(id).await;
    }

    async fn finish(self) {
        drop(self.writer);
        drop(self.reader);
        tokio::time::timeout(PROMPT, self.server)
            .await
            .expect("the server ends")
            .unwrap()
            .unwrap();
    }
}

fn elicitation() -> Value {
    json!({"elicitation": {}})
}

fn accept_frame(id: &Value, content: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": {"action": "accept", "content": content}})
}

#[tokio::test]
async fn a_reply_that_is_not_a_valid_result_never_approves() {
    for bad in [
        json!({"action": "maybe"}),
        json!({"action": "Accept", "content": {"answer": "run"}}),
        json!({"action": "ACCEPT", "content": {"answer": "run"}}),
        json!({"content": {"answer": "run"}}),
        json!("yes"),
        json!({}),
    ] {
        let mut raw = Raw::start(1, elicitation()).await;
        let call = raw.write(0, INSERT).await;
        let question = raw.server_request("elicitation/create").await;
        assert_eq!(question["params"]["mode"], "form");
        raw.send(json!({"jsonrpc": "2.0", "id": question["id"], "result": bad}))
            .await;
        // The reply cannot be read as a result: rmcp answers the pending question with an error and
        // the call is not approved, with the statement unrun.
        let result = raw.result(call).await;
        assert_eq!(code(&result), "agent.write_refused", "{bad}");
        assert_eq!(error_message(&result), not_run(REASON_NO_ANSWER), "{bad}");
        assert_eq!(raw.runs(0), 0, "{bad}");
        raw.finish().await;
    }
}

#[tokio::test]
async fn a_url_only_elicitation_capability_is_not_a_way_to_ask() {
    for capabilities in [
        json!({"elicitation": {"url": {}}}),
        json!({"sampling": {}}),
        json!({"elicitation": null}),
    ] {
        let mut raw = Raw::start(1, capabilities.clone()).await;
        let call = raw.write(0, INSERT).await;
        let result = raw.result(call).await;
        assert_eq!(code(&result), "agent.write_refused", "{capabilities}");
        assert_eq!(
            error_message(&result),
            not_asked(&no_way_to_ask(None)),
            "{capabilities}"
        );
        assert_eq!(raw.runs(0), 0);
        raw.finish().await;
    }
    // A form-only declaration is a way to ask.
    let mut raw = Raw::start(1, json!({"elicitation": {"form": {}}})).await;
    let call = raw.write(0, INSERT).await;
    let question = raw.server_request("elicitation/create").await;
    raw.send(accept_frame(&question["id"], json!({"answer": "run"})))
        .await;
    assert_eq!(code(&raw.result(call).await), "ok");
    assert_eq!(raw.runs(0), 1);
    raw.finish().await;
}

#[tokio::test]
async fn a_second_initialize_cannot_change_what_the_client_declared() {
    // Declared nothing, then claims elicitation: still cannot be asked.
    let mut raw = Raw::start(1, json!({})).await;
    let again = raw
        .request(
            "initialize",
            json!({
            "protocolVersion": "2025-06-18", "capabilities": {"elicitation": {}},
            "clientInfo": {"name": "raw", "version": "0"}}),
        )
        .await;
    let answer = raw.response(again).await;
    assert!(answer.get("error").is_some(), "refused: {answer}");
    let call = raw.write(0, INSERT).await;
    let result = raw.result(call).await;
    assert_eq!(error_message(&result), not_asked(&no_way_to_ask(None)));
    assert_eq!(raw.runs(0), 0);
    raw.finish().await;
    // Declared elicitation, then drops it: the first declaration stands (and the server is
    // still the same one for the session).
    let mut raw = Raw::start(1, elicitation()).await;
    let again = raw
        .request(
            "initialize",
            json!({
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "raw", "version": "0"}}),
        )
        .await;
    assert!(raw.response(again).await.get("error").is_some());
    let call = raw.write(0, INSERT).await;
    let question = raw.server_request("elicitation/create").await;
    raw.send(accept_frame(&question["id"], json!({"answer": "run"})))
        .await;
    assert_eq!(code(&raw.result(call).await), "ok");
    raw.finish().await;
}

#[tokio::test]
async fn a_late_a_duplicate_and_a_foreign_reply_change_nothing() {
    let mut raw = Raw::start(2, elicitation()).await;
    // 1. A question withdrawn by cancelling the call; then a late "yes" for it.
    let call = raw.write(0, INSERT).await;
    let withdrawn = raw.server_request("elicitation/create").await;
    raw.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": {"requestId": call, "reason": "test"}}),
    )
    .await;
    // The server withdraws the question from the client too.
    let notice = raw.server_request("notifications/cancelled").await;
    assert_eq!(notice["params"]["requestId"], withdrawn["id"]);
    raw.send(accept_frame(&withdrawn["id"], json!({"answer": "run"})))
        .await;
    raw.sync().await;
    assert_eq!(raw.runs(0), 0, "a late yes did not run it");
    // 2. A reply with an id nobody asked: ignored, and the real question is still open.
    let call = raw.write(1, INSERT).await;
    let question = raw.server_request("elicitation/create").await;
    raw.send(accept_frame(&json!(987_654), json!({"answer": "run"})))
        .await;
    raw.send(accept_frame(&json!("fake-1"), json!({"answer": "run"})))
        .await;
    raw.sync().await;
    assert_eq!(raw.runs(1), 0, "a reply with another id approves nothing");
    // 3. The right answer runs it once; the same answer again changes nothing.
    raw.send(accept_frame(&question["id"], json!({"answer": "run"})))
        .await;
    assert_eq!(code(&raw.result(call).await), "ok");
    raw.send(accept_frame(&question["id"], json!({"answer": "run"})))
        .await;
    raw.send(accept_frame(&question["id"], json!({"answer": "run"})))
        .await;
    raw.sync().await;
    assert_eq!(raw.runs(1), 1, "one question, one run");
    assert_eq!(raw.runs(0), 0);
    raw.finish().await;
}

// ---- the switch, size, declines and open questions -------------------------------------------------

#[tokio::test]
async fn a_switch_turned_off_while_the_question_waits_stops_the_write() {
    for (turned_off, reason) in [
        (r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#, REASON_SWITCH_OFF),
        (r#"{"schema_version": 6, "ai": {"mcp_writes": false}}"#, REASON_SWITCH_OFF),
        ("{ not json", REASON_SETTINGS_UNAVAILABLE),
        (r#"{"schema_version": 4, "ai": {"mcp_writes": "true"}}"#, REASON_SETTINGS_UNAVAILABLE),
        (r#"{"ai": {"mcp_writes": true}}"#, REASON_SWITCH_OFF),
    ] {
        let (asker, mut desk) = Asker::manual(true);
        let rig = rig(1, false, true, asker).await;
        let write = rig
            .harness
            .start_call(
                "execute_query",
                json!({"connection_id": rig.ids[0], "statement": INSERT}),
            )
            .await;
        let question = within("a question", desk.recv())
            .await
            .expect("the question arrives");
        // The user turns the switch off in the app; the client answers yes afterwards.
        rig.harness.fixture.write_settings(turned_off);
        question.accept(true);
        let result = unwrap_call(
            within("the answer to the call", write.await_response())
                .await
                .unwrap(),
        );
        assert_eq!(code(&result), "agent.write_refused", "{turned_off}");
        assert_eq!(
            error_message(&result),
            format!("the change was approved, but it can no longer run: {}", refusal(reason)),
            "{turned_off}"
        );
        assert!(rig.runs(0).is_empty(), "{turned_off}: not run");
        let entries = rig.harness.fixture.audit_entries();
        let entry = entries
            .iter()
            .find(|entry| entry.tool == "execute_query")
            .unwrap();
        assert_eq!(
            entry.approval.as_deref(),
            Some(format!("approved, then refused: {}", engine_agent::sanitize::clean(&refusal(reason), 200)).as_str()),
            "{turned_off}"
        );
        // And the switch on again lets the next write be asked.
        rig.harness.fixture.set_mcp_writes(true);
        let again = rig
            .harness
            .start_call(
                "execute_query",
                json!({"connection_id": rig.ids[0], "statement": INSERT}),
            )
            .await;
        within("a question", desk.recv())
            .await
            .expect("asked again")
            .accept(true);
        assert_eq!(
            code(&unwrap_call(
                within("the answer to the call", again.await_response())
                    .await
                    .unwrap()
            )),
            "ok"
        );
        rig.finish().await;
    }
}

#[tokio::test]
async fn a_statement_too_long_to_review_is_not_asked() {
    let rig = rig(1, false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let (head, tail) = ("UPDATE items SET label = '", "' WHERE id = 1");
    // `total` characters in all, a line break every 100 of them (so lines are short).
    let block = |total: usize| {
        let filler: String = (0..total - head.len() - tail.len())
            .map(|n| if n % 100 == 99 { '\n' } else { 'x' })
            .collect();
        format!("{head}{filler}{tail}")
    };
    // One line of `width` characters.
    let line = |width: usize| {
        format!(
            "{head}{}{tail}",
            "x".repeat(width - head.len() - tail.len())
        )
    };
    let lines = |n: usize| format!("{head}{}{tail}", "\n".repeat(n - 1));
    assert_eq!(block(MAX_CHARS).chars().count(), MAX_CHARS);
    assert_eq!(line(MAX_LINE_CHARS).chars().count(), MAX_LINE_CHARS);
    assert_eq!(lines(MAX_LINES).split('\n').count(), MAX_LINES);
    // The limits themselves are reviewable.
    for statement in [block(MAX_CHARS), line(MAX_LINE_CHARS), lines(MAX_LINES)] {
        assert_eq!(code(&rig.execute(0, &statement).await), "ok");
    }
    assert_eq!(rig.harness.client.service().seen.asked().len(), 3);
    // One over is not: too many characters, a line too wide, too many lines, and characters that
    // are few as they are stored and many as they are shown (each is written `<U+200B>`).
    let hidden = format!("{head}{}{tail}", "\u{200B}".repeat(MAX_CHARS / 8 + 1));
    let wide_hidden = format!("{head}{}{tail}", "\u{200B}".repeat(MAX_LINE_CHARS / 8 + 1));
    for statement in [
        block(MAX_CHARS + 1),
        line(MAX_LINE_CHARS + 1),
        lines(MAX_LINES + 1),
        hidden,
        wide_hidden,
    ] {
        let result = rig.execute(0, &statement).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert_eq!(error_message(&result), not_asked(REASON_TOO_LONG));
    }
    assert_eq!(
        rig.harness.client.service().seen.asked().len(),
        3,
        "nothing more was asked"
    );
    assert_eq!(rig.runs(0).len(), 3);
    assert!(REASON_TOO_LONG.contains("run it in the tablex app"));
    rig.finish().await;
}

#[tokio::test]
async fn a_connection_declined_three_times_is_closed_to_writes_for_the_session() {
    let no = Ok(ElicitResult::new(ElicitationAction::Decline));
    let rig = rig(2, false, true, Asker::answering(true, no)).await;
    for _ in 0..MAX_DECLINES {
        let result = rig.execute(0, INSERT).await;
        assert_eq!(error_message(&result), not_approved_plain());
    }
    // The same statement again, and a different one, and with the switch flipped meanwhile.
    rig.harness.fixture.set_mcp_writes(false);
    rig.harness.fixture.set_mcp_writes(true);
    for statement in [INSERT, "INSERT INTO items(label) VALUES ('other')"] {
        let result = rig.execute(0, statement).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert_eq!(error_message(&result), not_asked(REASON_DECLINED_OFTEN));
    }
    assert_eq!(
        rig.harness.client.service().seen.asked().len(),
        MAX_DECLINES as usize
    );
    // Audited, with the fixed reason.
    let refused = format!("refused: {}", engine_agent::sanitize::clean(&refusal(REASON_DECLINED_OFTEN), 200));
    let audited = rig
        .harness
        .fixture
        .audit_entries()
        .iter()
        .filter(|entry| entry.approval.as_deref() == Some(refused.as_str()))
        .count();
    assert_eq!(audited, 2);
    // Another connection is not closed.
    let other = rig.execute(1, INSERT).await;
    assert_eq!(error_message(&other), not_approved_plain());
    assert!(rig.runs(0).is_empty());
    rig.finish().await;
}

fn not_approved_plain() -> String {
    "the user did not approve this SQL statement".to_owned()
}

#[tokio::test]
async fn at_most_four_questions_wait_and_the_next_write_is_refused_at_once() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(MAX_OPEN_QUESTIONS + 1, false, true, asker).await;
    let mut waiting = Vec::new();
    for n in 0..MAX_OPEN_QUESTIONS {
        let call = rig
            .harness
            .start_call(
                "execute_query",
                json!({"connection_id": rig.ids[n], "statement": INSERT}),
            )
            .await;
        waiting.push((
            call,
            within("a question", desk.recv()).await.expect("a question"),
        ));
    }
    // The fifth does not wait: it is refused, and says why.
    let fifth = rig.execute(MAX_OPEN_QUESTIONS, INSERT).await;
    assert_eq!(code(&fifth), "agent.write_refused");
    assert_eq!(error_message(&fifth), not_asked(REASON_BUSY));
    // Everything else still answers (the open questions did not eat every call permit).
    assert_eq!(
        code(&rig.harness.call("list_connections", json!({})).await),
        "ok"
    );
    assert_eq!(
        rig.harness.client.service().seen.asked().len(),
        MAX_OPEN_QUESTIONS
    );
    // A question answered gives its place back.
    let (call, question) = waiting.remove(0);
    question.accept(true);
    assert_eq!(
        code(&unwrap_call(
            within("the answer to the call", call.await_response())
                .await
                .unwrap()
        )),
        "ok"
    );
    let sixth = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[MAX_OPEN_QUESTIONS], "statement": INSERT}),
        )
        .await;
    waiting.push((
        sixth,
        within("a question", desk.recv())
            .await
            .expect("a place was free"),
    ));
    for (call, question) in waiting {
        question.accept(true);
        assert_eq!(
            code(&unwrap_call(
                within("the answer to the call", call.await_response())
                    .await
                    .unwrap()
            )),
            "ok"
        );
    }
    rig.finish().await;
}

#[tokio::test]
async fn a_run_false_and_a_dismissal_count_as_declines() {
    for (label, answer) in [
        ("run: false", Ok(accepted(false))),
        (
            "dismissed",
            Ok(ElicitResult::new(ElicitationAction::Cancel)),
        ),
    ] {
        let rig = rig(1, false, true, Asker::answering(true, answer)).await;
        for _ in 0..MAX_DECLINES {
            let result = rig.execute(0, INSERT).await;
            assert_eq!(code(&result), "agent.write_refused", "{label}");
        }
        let result = rig.execute(0, INSERT).await;
        assert_eq!(
            error_message(&result),
            not_asked(REASON_DECLINED_OFTEN),
            "{label}"
        );
        assert_eq!(
            rig.harness.client.service().seen.asked().len(),
            MAX_DECLINES as usize,
            "{label}"
        );
        rig.finish().await;
    }
}

/// Cancels `call` (the tool call) and waits until its audit line says it ended.
async fn cancel_and_settle(
    rig: &Rig,
    call: rmcp::service::RequestHandle<rmcp::RoleClient>,
    ended: usize,
) {
    call.cancel(Some("test".into())).await.unwrap();
    poll("the cancelled call in the audit log", async || {
        let cancelled = rig
            .harness
            .fixture
            .audit_entries()
            .iter()
            .filter(|entry| entry.tool == "execute_query" && entry.outcome == "agent.cancelled")
            .count();
        (cancelled >= ended).then_some(())
    })
    .await;
}

#[tokio::test]
async fn a_call_cancelled_after_its_question_was_sent_counts_as_a_decline() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(1, false, true, asker).await;
    let mut held = Vec::new();
    for n in 1..=MAX_DECLINES as usize {
        let call = rig
            .harness
            .start_call(
                "execute_query",
                json!({"connection_id": rig.ids[0], "statement": INSERT}),
            )
            .await;
        // Held, not dropped: a dropped question answers (with an error) and ends the call.
        held.push(within("a question", desk.recv()).await.expect("asked"));
        cancel_and_settle(&rig, call, n).await;
    }
    // A user who dismisses by interrupting the tool call is not askable without limit.
    let result = rig.execute(0, INSERT).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(error_message(&result), not_asked(REASON_DECLINED_OFTEN));
    assert_eq!(
        rig.harness.client.service().seen.asked().len(),
        MAX_DECLINES as usize
    );
    assert!(rig.runs(0).is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn a_cancelled_question_gives_its_place_back() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(MAX_OPEN_QUESTIONS + 1, false, true, asker).await;
    let mut calls = Vec::new();
    let mut held = Vec::new();
    for n in 0..MAX_OPEN_QUESTIONS {
        calls.push(
            rig.harness
                .start_call(
                    "execute_query",
                    json!({"connection_id": rig.ids[n], "statement": INSERT}),
                )
                .await,
        );
        held.push(within("a question", desk.recv()).await.expect("asked"));
    }
    let full = rig.execute(MAX_OPEN_QUESTIONS, INSERT).await;
    assert_eq!(error_message(&full), not_asked(REASON_BUSY));
    cancel_and_settle(&rig, calls.remove(0), 1).await;
    // The place is free again: the next write is asked, not refused.
    let call = rig
        .harness
        .start_call(
            "execute_query",
            json!({"connection_id": rig.ids[MAX_OPEN_QUESTIONS], "statement": INSERT}),
        )
        .await;
    let question = within("a question", desk.recv())
        .await
        .expect("a place was free");
    question.accept(true);
    assert_eq!(
        code(&unwrap_call(
            within("the answer", call.await_response()).await.unwrap()
        )),
        "ok"
    );
    rig.finish().await;
}

#[tokio::test]
async fn questions_open_at_once_on_one_connection_cannot_get_past_the_limit_together() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(1, false, true, asker).await;
    let mut open = Vec::new();
    for _ in 0..MAX_DECLINES {
        let call = rig
            .harness
            .start_call(
                "execute_query",
                json!({"connection_id": rig.ids[0], "statement": INSERT}),
            )
            .await;
        let question = within("a question", desk.recv()).await.expect("asked");
        open.push((call, question));
    }
    // Three are open on this connection: a fourth would be a possible sixth ask after declines.
    let fourth = rig.execute(0, INSERT).await;
    assert_eq!(error_message(&fourth), not_asked(REASON_BUSY));
    for (call, question) in open {
        question.answer(Ok(ElicitResult::new(ElicitationAction::Decline)));
        let result = unwrap_call(within("the answer", call.await_response()).await.unwrap());
        assert_eq!(code(&result), "agent.write_refused");
    }
    let closed = rig.execute(0, INSERT).await;
    assert_eq!(error_message(&closed), not_asked(REASON_DECLINED_OFTEN));
    assert_eq!(
        rig.harness.client.service().seen.asked().len(),
        MAX_DECLINES as usize
    );
    rig.finish().await;
}
