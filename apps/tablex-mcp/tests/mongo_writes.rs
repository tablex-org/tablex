//! MongoDB writes and `execute_batch` through the MCP server: the real server and approver over an
//! in-memory pipe, fake sessions that record what reaches them. A write runs only in the one
//! allowed cell (switch on, not production, a client that can be asked, a yes); the question shows
//! the database, the collection and the exact command, or the whole script of a batch.

mod support;

use std::sync::Arc;

use engine_agent::testing::{connection, Behavior, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{AgentAccess, ConnectionKind, Session, ToolLayer};
use serde_json::{json, Value};
use support::*;
use tablex_mcp::approver::{
    ElicitingApprover, REASON_NO_ELICITATION, REASON_PRODUCTION, REASON_SWITCH_OFF,
};

const MONGO: &str = "mongo-0";
const PG: &str = "pg-0";

struct Rig {
    harness: Harness<Asker>,
    mongo: Arc<FakeSession>,
    pg: Arc<FakeSession>,
}

async fn rig(production: bool, switch: bool, asker: Asker) -> Rig {
    let mongo = FakeSession::new(ConnectionKind::MongoDb).arc();
    // The server's replies, as canonical Extended JSON.
    mongo.set_mongo(|_, command, _| {
        let int = |n: usize| json!({"$numberInt": n.to_string()});
        let ok = json!({"$numberDouble": "1.0"});
        Ok(match command.as_object().unwrap().keys().next().unwrap().as_str() {
            "insert" => json!({"n": int(command["documents"].as_array().unwrap().len()), "ok": ok}),
            "update" => json!({"n": int(1), "nModified": int(1), "ok": ok}),
            "delete" => json!({"n": int(1), "ok": ok}),
            other => panic!("unexpected command {other}"),
        })
    });
    let pg = FakeSession::new(ConnectionKind::PostgreSql).arc();
    pg.set_behavior(Behavior::Affected(1));
    let catalog = FakeCatalog::new(
        [(MONGO, ConnectionKind::MongoDb), (PG, ConnectionKind::PostgreSql)]
            .into_iter()
            .map(|(id, kind)| {
                let mut info = connection(id, kind, Some(AgentAccess::ReadWrite));
                info.name = format!("Shop {id}");
                info.connection_type.production = production;
                info
            })
            .collect(),
    );
    let provider = MapProvider(
        [
            (MONGO.to_owned(), Arc::clone(&mongo) as Arc<dyn Session>),
            (PG.to_owned(), Arc::clone(&pg) as Arc<dyn Session>),
        ]
        .into(),
    );
    let fixture = Fixture::with_layer_in(|audit, settings| {
        ToolLayer::new(catalog, Arc::new(provider), "mcp")
            .with_transactions()
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit)
    });
    fixture.set_mcp_writes(switch);
    let harness = Harness::start_with(fixture, asker).await;
    for id in [MONGO, PG] {
        let opened = harness.call("open_session", json!({"connection_id": id})).await;
        assert_eq!(code(&opened), "ok", "{}", text(&opened));
    }
    Rig { harness, mongo, pg }
}

impl Rig {
    async fn write(&self, tool: &str, extra: Value) -> rmcp::model::CallToolResult {
        let mut args = json!({"connection_id": MONGO, "database": "shop", "collection": "orders"});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.harness.call(tool, args).await
    }

    async fn batch(&self, script: &str, extra: Value) -> rmcp::model::CallToolResult {
        let mut args = json!({"connection_id": PG, "script": script});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        self.harness.call("execute_batch", args).await
    }

    fn sent(&self) -> Vec<Value> {
        self.mongo.mongo_calls().into_iter().map(|(_, command)| command).collect()
    }

    fn ran(&self) -> Vec<String> {
        self.pg.runs.lock().unwrap().iter().map(|(text, _)| text.clone()).collect()
    }

    async fn finish(self) {
        let Harness { client, server, .. } = self.harness;
        client.cancel().await.unwrap();
        server.await.unwrap().unwrap();
    }
}

fn message(result: &rmcp::model::CallToolResult) -> String {
    let body: Value = serde_json::from_str(&text(result)).expect("the error is JSON");
    body["error"]["message"].as_str().expect("a message").to_owned()
}

const INSERT: fn() -> Value = || json!({"documents": [{"sku": "a-1", "stock": 4}]});

#[tokio::test]
async fn a_mongodb_write_runs_exactly_once_in_the_one_allowed_cell_and_nowhere_else() {
    // (production, switch, can be asked, answer) -> sent
    for (production, switch, capability, answer, allowed) in [
        (false, true, true, true, true),
        (false, true, true, false, false),
        (false, true, false, true, false),
        (false, false, true, true, false),
        (true, true, true, true, false),
        (true, false, false, false, false),
    ] {
        let rig = rig(production, switch, Asker::answering(capability, Ok(accepted(answer)))).await;
        for (tool, extra) in [
            ("mongo_insert", INSERT()),
            ("mongo_update", json!({"filter": {"a": 1}, "update": {"$set": {"a": 2}}})),
            ("mongo_delete", json!({"filter": {"a": 1}, "many": true})),
        ] {
            let result = rig.write(tool, extra).await;
            let cell = format!("{tool}: production {production}, switch {switch}, capability {capability}, answer {answer}");
            if allowed {
                assert_eq!(code(&result), "ok", "{cell}: {}", text(&result));
            } else {
                assert_eq!(code(&result), "agent.write_refused", "{cell}: {}", text(&result));
            }
        }
        assert_eq!(rig.sent().len(), if allowed { 3 } else { 0 }, "{production} {switch} {capability} {answer}");
        if !allowed {
            let reason = message(&rig.write("mongo_insert", INSERT()).await);
            let expected = if !switch {
                Some(REASON_SWITCH_OFF)
            } else if production {
                Some(REASON_PRODUCTION)
            } else if !capability {
                Some(REASON_NO_ELICITATION)
            } else {
                None
            };
            if let Some(expected) = expected {
                assert!(reason.contains("the user was not asked") && reason.contains(expected), "{reason}");
            }
        }
        rig.finish().await;
    }
}

#[tokio::test]
async fn the_question_names_the_database_the_collection_and_the_exact_command() {
    let rig = rig(false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = rig
        .write(
            "mongo_update",
            json!({"filter": {"_id": {"$oid": "65a1b2c3d4e5f60718293a4b"}}, "update": {"$set": {"status": "paid"}}, "upsert": true}),
        )
        .await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let body = structured(&result);
    assert_eq!((body["n"].as_u64(), body["n_modified"].as_u64()), (Some(1), Some(1)));
    let asked = rig.harness.client.service().seen.asked();
    assert_eq!(asked.len(), 1);
    let message = &asked[0].0;
    assert!(message.starts_with("An MCP client asks to run a command that changes data."), "{message}");
    assert!(message.contains("Connection: `Shop mongo-0` (`Development`)\n"), "{message}");
    assert!(message.contains("Database: `shop`\n"), "{message}");
    assert!(message.contains("Collection: `orders`\n"), "{message}");
    // The code block holds the command that was sent, indented JSON.
    let block = message.split("\n```\n").nth(1).expect("a code block");
    let shown: Value = serde_json::from_str(block.trim_end_matches("\n```")).expect("the block is JSON");
    assert_eq!(shown, rig.sent()[0]);
    assert_eq!(shown["updates"][0]["q"]["_id"], json!({"$oid": "65a1b2c3d4e5f60718293a4b"}));
    assert!(block.contains("\n  \"updates\": ["), "{block}");
    rig.finish().await;
}

#[tokio::test]
async fn an_empty_filter_never_asks_through_mcp_and_a_mistake_is_refused_before_it_could() {
    let rig = rig(false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let mistake = rig.write("mongo_delete", json!({"filter": {}, "many": true})).await;
    assert_eq!(code(&mistake), "agent.invalid_arguments", "{}", text(&mistake));
    let dangerous = rig
        .write("mongo_delete", json!({"filter": {}, "many": true, "allow_all": true}))
        .await;
    assert_eq!(code(&dangerous), "agent.dangerous_refused", "{}", text(&dangerous));
    assert!(message(&dangerous).contains("never runs dangerous"), "{}", message(&dangerous));
    assert!(rig.sent().is_empty());
    assert!(rig.harness.client.service().seen.asked().is_empty());
    rig.finish().await;
}

#[tokio::test]
async fn a_switch_turned_off_while_the_mongodb_question_waits_sends_nothing() {
    let (asker, mut desk) = Asker::manual(true);
    let rig = rig(false, true, asker).await;
    let call = rig
        .harness
        .start_call(
            "mongo_insert",
            json!({"connection_id": MONGO, "database": "shop", "collection": "orders", "documents": [{"a": 1}]}),
        )
        .await;
    let question = within("a question", desk.recv()).await.expect("the question arrives");
    rig.harness
        .fixture
        .write_settings(r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#);
    question.accept(true);
    let result = unwrap_call(within("the answer", call.await_response()).await.unwrap());
    assert_eq!(code(&result), "agent.write_refused");
    assert_eq!(
        message(&result),
        format!("the change was approved, but it can no longer run: {REASON_SWITCH_OFF}")
    );
    assert!(rig.sent().is_empty());
    rig.finish().await;
}

// ------------------------------------------------------------------ execute_batch

#[tokio::test]
async fn a_batch_asks_once_for_all_its_changes_and_runs_after_the_yes() {
    let rig = rig(false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = rig
        .batch(
            "SELECT 1; INSERT INTO items (name) VALUES ('a'); UPDATE orders SET status = 'paid' WHERE id = 1; SELECT 2",
            json!({}),
        )
        .await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    let body = structured(&result);
    assert_eq!(body["statement_count"], 4);
    assert_eq!(body["changes"], 2);
    assert_eq!(rig.ran().len(), 4);
    let asked = rig.harness.client.service().seen.asked();
    assert_eq!(asked.len(), 1, "one question for the whole script");
    let message = &asked[0].0;
    assert!(message.starts_with("An MCP client asks to run a script that changes data."), "{message}");
    assert!(message.contains("the 2 statements below that change data"), "{message}");
    assert!(
        message.ends_with(
            "\n```\nINSERT INTO items (name) VALUES ('a');\n\nUPDATE orders SET status = 'paid' WHERE id = 1;\n```"
        ),
        "{message}"
    );
    assert!(!message.contains("SELECT"), "{message}");
    rig.finish().await;
}

#[tokio::test]
async fn a_batch_that_is_declined_refused_or_dangerous_runs_nothing() {
    // The client says no.
    let no = rig(false, true, Asker::answering(true, Ok(accepted(false)))).await;
    let result = no.batch("SELECT 1; INSERT INTO items (name) VALUES ('a')", json!({})).await;
    assert_eq!(code(&result), "agent.write_refused");
    assert!(no.ran().is_empty());
    no.finish().await;

    // The switch is off, or the connection is a production one: nobody is asked.
    for (production, switch, reason) in [(false, false, REASON_SWITCH_OFF), (true, true, REASON_PRODUCTION)] {
        let refused = rig(production, switch, Asker::answering(true, Ok(accepted(true)))).await;
        let result = refused.batch("SELECT 1; INSERT INTO items (name) VALUES ('a')", json!({})).await;
        assert_eq!(code(&result), "agent.write_refused");
        assert!(message(&result).contains(reason), "{}", message(&result));
        assert!(refused.ran().is_empty());
        assert!(refused.harness.client.service().seen.asked().is_empty());
        refused.finish().await;
    }

    // A dangerous statement refuses the whole script before any question or statement.
    let danger = rig(false, true, Asker::answering(true, Ok(accepted(true)))).await;
    let result = danger.batch("INSERT INTO items (name) VALUES ('a'); DROP TABLE items", json!({})).await;
    assert_eq!(code(&result), "agent.dangerous_refused");
    assert!(message(&result).contains("statement 2"), "{}", message(&result));
    assert!(danger.ran().is_empty());
    assert!(danger.harness.client.service().seen.asked().is_empty());
    danger.finish().await;
}

#[tokio::test]
async fn a_batch_in_a_transaction_is_approved_once_and_committed_without_a_second_question() {
    let own = FakeSession::new(ConnectionKind::PostgreSql).arc();
    own.set_behavior(Behavior::Affected(1));
    let shared = FakeSession::new(ConnectionKind::PostgreSql).arc();
    shared.set_behavior(Behavior::Affected(1));
    let provider = FakeProvider::new();
    provider.with_session(PG, Arc::clone(&shared));
    provider.with_extra_session(PG, Arc::clone(&own));
    let catalog = FakeCatalog::new(vec![connection(PG, ConnectionKind::PostgreSql, Some(AgentAccess::ReadWrite))]);
    let fixture = Fixture::with_layer_in(|audit, settings| {
        ToolLayer::new(catalog, provider, "mcp")
            .with_transactions()
            .with_approver(Arc::new(ElicitingApprover::new(Some(settings))))
            .with_audit(audit)
    });
    fixture.set_mcp_writes(true);
    let harness = Harness::start_with(fixture, Asker::answering(true, Ok(accepted(true)))).await;
    harness.call("open_session", json!({"connection_id": PG})).await;
    let result = harness
        .call(
            "execute_batch",
            json!({"connection_id": PG, "script": "INSERT INTO a VALUES (1); INSERT INTO a VALUES (2)", "use_transaction": true}),
        )
        .await;
    assert_eq!(code(&result), "ok", "{}", text(&result));
    assert_eq!(structured(&result)["transaction"], "committed");
    assert_eq!(
        own.control_calls(),
        [engine_agent::TxControl::Begin, engine_agent::TxControl::Commit]
    );
    assert_eq!(harness.client.service().seen.asked().len(), 1, "no question for the commit");
    let Harness { client, server, .. } = harness;
    client.cancel().await.unwrap();
    server.await.unwrap().unwrap();
}
