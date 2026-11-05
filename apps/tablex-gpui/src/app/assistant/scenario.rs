//! UI scenarios of the assistant (`--ui-scenario assistant ...`): the dock open on the fixture's
//! PostgreSQL connection, driven by a scripted provider and the agent crate's fakes, so a capture
//! shows real runs of the real service without a network, a key or a database.

use super::scripted::{failure, say, Scripted, Step};
use super::store::{self, ConversationStore, Header};
use super::*;
use crate::ui_scenario::ScenarioKind;
use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{AgentAccess, ConnectionKind, ConnectionTypeInfo};
use engine_ai::ProviderError;
use serde_json::json;

const ANSWER: &str = "Revenue per customer, top five:\n\n\
1. **Acme Corp**: 48 200\n\
2. **Globex**: 31 950\n\
3. **Initech**: 12 400\n\n\
Run it against `public.orders`:\n\n\
```sql\nSELECT customer_id, SUM(total) AS revenue\nFROM public.orders\nGROUP BY customer_id\nORDER BY revenue DESC\nLIMIT 5;\n```\n\n\
Only paid orders are counted when `status = 'paid'` is added to a `WHERE` clause.";

const EXPLANATION: &str = "This query ranks customers by what they paid:\n\n\
1. It keeps only orders whose `status` is `'paid'`.\n\
2. It adds up `total` for each `customer_id`.\n\
3. It sorts the sums from largest to smallest and returns the first five.\n\n\
Nothing in it changes data. An index on `(status, customer_id)` would let the database skip unpaid orders.";

fn table() -> engine_agent::table::TableDescription {
    use engine_agent::table::{ColumnDescription, TableDescription};
    let column = |name: &str, data_type: &str, primary_key: bool| ColumnDescription {
        name: name.into(),
        data_type: data_type.into(),
        nullable: !primary_key,
        primary_key,
        default: None,
        comment: None,
    };
    TableDescription {
        name: "orders".into(),
        database: Some("warehouse".into()),
        schema: Some("public".into()),
        columns: vec![
            column("id", "bigint", true),
            column("customer_id", "bigint", false),
            column("total", "numeric(12,2)", false),
            column("status", "text", false),
        ],
        primary_key: vec!["id".into()],
        unique_columns: vec![],
        indexes: vec![],
        foreign_keys: vec![],
        ddl: None,
        ddl_truncated: false,
    }
}

/// An UPDATE of forty orders by id, one `WHEN` per line: longer than the card's code box.
fn long_update() -> String {
    let mut statement = String::from("UPDATE public.orders\nSET status = CASE id\n");
    for id in 1000..1040 {
        statement.push_str(&format!("    WHEN {id} THEN 'archived'\n"));
    }
    statement.push_str("    ELSE status\nEND\nWHERE created_at < DATE '2024-01-01'");
    statement
}

/// A stored conversation about the fixture's PostgreSQL connection: a question, one tool call and
/// the answer.
fn stored_conversation(question: &str, statement: &str, answer: &str) -> engine_ai::Conversation {
    let id = crate::ui_fixture::POSTGRES_ID;
    serde_json::from_value(json!({
        "scope": {"connection_id": id, "database": "warehouse", "schema": "public"},
        "context": null,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": question}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Look at the orders, then answer.", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "execute_query",
                 "input": {"connection_id": id, "statement": statement}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "{\"columns\":[{\"name\":\"n\",\"type\":\"bigint\"}],\"rows\":[[\"1\"]],\"row_count\":1}"}
            ]},
            {"role": "assistant", "content": [{"type": "text", "text": answer}]}
        ]
    }))
    .expect("a valid stored conversation")
}

/// A fresh store in a temporary folder with three conversations of the fixture's connection:
/// `(id, age in minutes)`: the newest first. `access` is the level the newest one ran at.
fn seeded_store(access: engine_agent::AgentAccess) -> (Arc<ConversationStore>, ScenarioFolder) {
    let root = std::env::temp_dir().join(format!(
        "tablex-scenario-conversations-{}",
        uuid::Uuid::new_v4()
    ));
    let folder = ScenarioFolder(root.clone());
    let store = Arc::new(ConversationStore::new(root));
    let now = store::now_ms();
    let stored = [
        (
            "top-customers",
            2,
            "Who are my top five customers by revenue?",
            "SELECT customer_id, SUM(total) FROM public.orders GROUP BY 1 ORDER BY 2 DESC LIMIT 5",
            "Acme Corp leads with 48 200, then Globex (31 950) and Initech (12 400).",
        ),
        (
            "slow-orders",
            3 * 60,
            "Why is a scan of the orders table slow?",
            "SELECT COUNT(*) FROM public.orders",
            "The table has no index on status, so every filter on it reads all rows.",
        ),
        (
            "cancelled",
            27 * 60,
            "How many orders were cancelled last month?",
            "SELECT COUNT(*) FROM public.orders WHERE status = 'cancelled'",
            "There were 312 cancelled orders last month.",
        ),
    ];
    for (id, minutes_ago, question, statement, answer) in stored {
        let header = Header {
            format: store::FORMAT,
            id: id.to_owned(),
            title: store::title_of(question),
            created: now - minutes_ago * 60_000,
            updated: now - minutes_ago * 60_000,
            provider: "anthropic".to_owned(),
            model: engine_ai::DEFAULT_MODEL.to_owned(),
            access,
            seq: 0,
        };
        let conversation = stored_conversation(question, statement, answer);
        let encoded = store::encode(&header, &conversation).expect("small enough");
        store
            .write(crate::ui_fixture::POSTGRES_ID, &encoded)
            .expect("a temporary folder is writable");
    }
    (store, folder)
}

/// A scenario's temporary folder of conversations: removed when the app state goes (or quits).
pub(in crate::app) struct ScenarioFolder(std::path::PathBuf);

impl ScenarioFolder {
    #[cfg(test)]
    pub(super) fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for ScenarioFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl AppRoot {
    /// A store with three conversations for the Settings scenario (so its delete button is on).
    #[cfg(any(test, feature = "ui-scenarios"))]
    pub(in crate::app) fn seed_conversation_store(&mut self) {
        let (store, folder) = seeded_store(engine_agent::AgentAccess::Read);
        self.assistant.store = Some(store);
        self.assistant.scenario_folder = Some(folder);
    }

    /// Opens the dock on the fixture's PostgreSQL connection in the state `kind` names.
    pub(in crate::app) fn seed_assistant_scenario(
        &mut self,
        kind: ScenarioKind,
        cx: &mut Context<Self>,
    ) {
        let id = crate::ui_fixture::POSTGRES_ID;
        let name = self.connection_name(id).unwrap_or("warehouse").to_owned();
        // A key, unless the scenario is about the missing one; the connection's own agent
        // access, unless it is about the disclosure.
        let key = kind != ScenarioKind::AssistantNoKey;
        if key {
            let _ = self.assistant.keys.save("sk-ant-scenario-key");
        }
        self.assistant.set_key_state(
            AiProvider::Anthropic,
            if key { KeyState::Set } else { KeyState::NotSet },
        );
        let writes = matches!(
            kind,
            ScenarioKind::AssistantApproval
                | ScenarioKind::AssistantApprovalDangerous
                | ScenarioKind::AssistantApprovalLong
        );
        let production = kind == ScenarioKind::AssistantApprovalDangerous;
        let stored = matches!(
            kind,
            ScenarioKind::AssistantHistory | ScenarioKind::AssistantReadonly
        );
        if !matches!(
            kind,
            ScenarioKind::AssistantAccessOff | ScenarioKind::AssistantAccessWrite
        ) {
            let mut profile = crate::settings::connection::ConnectionProfile::default();
            profile.overrides.set(
                engine_connect::profile::AGENT_ACCESS,
                engine_connect::profile::OverrideValue::Text(
                    if writes { "read_write" } else { "read" }.into(),
                ),
            );
            if production {
                profile.type_id = crate::settings::connection::PRODUCTION.into();
            }
            self.connection_policy.set_profile(id, profile);
        }

        // The agent side: fakes, with a statement that never ends for the streaming scenario.
        let session = FakeSession::new(ConnectionKind::PostgreSql).arc();
        session.tables.lock().unwrap().push(table());
        if kind == ScenarioKind::AssistantStreaming {
            session.set_behavior(engine_agent::testing::Behavior::BlockUntilCancelled);
        }
        let mut info = connection(
            id,
            ConnectionKind::PostgreSql,
            Some(if writes {
                AgentAccess::ReadWrite
            } else {
                AgentAccess::Read
            }),
        );
        info.name = name.clone();
        if production {
            info.connection_type = ConnectionTypeInfo {
                name: "Production".into(),
                production: true,
                ..ConnectionTypeInfo::default()
            };
        }
        let catalog = FakeCatalog::new(vec![info]);
        let sessions = FakeProvider::new().with_session(id, session);
        let layer = Arc::new(
            ToolLayer::new(catalog.clone(), sessions, FRONT_END).with_approver(AppApprover::new()),
        );
        self.assistant.set_layer_for_test(layer);
        let statement = json!({
            "connection_id": id,
            "statement": "SELECT customer_id, SUM(total) FROM public.orders GROUP BY customer_id ORDER BY 2 DESC LIMIT 5"
        });
        let reasoning = |text: &str| Step::Say {
            reasoning: vec![text.to_owned()],
            text: Vec::new(),
            tools: vec![("t1", "execute_query", statement.clone())],
        };
        let write = |text: &str, statement: serde_json::Value| Step::Say {
            reasoning: vec![text.to_owned()],
            text: Vec::new(),
            tools: vec![("t1", "execute_query", statement)],
        };
        let steps = match kind {
            ScenarioKind::AssistantApproval => vec![write(
                "Order 1042 is the one to cancel; mark it cancelled and nothing else.",
                json!({
                    "connection_id": id,
                    "database": "warehouse",
                    "statement": "UPDATE public.orders\nSET status = 'cancelled'\nWHERE id = 1042"
                }),
            )],
            ScenarioKind::AssistantApprovalLong => vec![write(
                "A status for every order that is older than the cut-off, as listed.",
                json!({
                    "connection_id": id,
                    "database": "warehouse",
                    "statement": long_update()
                }),
            )],
            ScenarioKind::AssistantApprovalDangerous => vec![write(
                "You asked to clear the table, so every row has to go.",
                json!({
                    "connection_id": id,
                    "database": "warehouse",
                    "statement": "DELETE FROM public.orders",
                    "dangerous": true
                }),
            )],
            ScenarioKind::AssistantStreaming => vec![Step::Say {
                reasoning: vec!["Aggregate the orders by customer, then rank them.".into()],
                text: vec!["Let me look at the biggest customers first.".into()],
                tools: vec![("t1", "execute_query", statement.clone())],
            }],
            ScenarioKind::AssistantError => {
                vec![Step::Fail(ProviderError::Overloaded(failure("Overloaded")))]
            }
            ScenarioKind::AssistantEditorAction => vec![say(EXPLANATION)],
            _ => vec![
                reasoning("Aggregate the orders by customer, then rank them."),
                say(ANSWER),
            ],
        };
        let provider =
            Scripted::new(steps).with_fallback("That is all I can say in this scenario.");
        self.assistant.provider_override = Some(provider);
        self.assistant.dock.open = true;

        let chat = self.assistant_chat(cx);
        let scope = engine_ai::Scope {
            connection_id: id.to_owned(),
            database: Some("warehouse".into()),
            schema: Some("public".into()),
        };
        chat.update(cx, |chat, cx| chat.new_chat(Some(scope), name.clone(), cx));
        self.assistant_sync(cx);
        if stored {
            // Three conversations on disk; the newest is the open one. The read-only scenario
            // stored them at a level above the connection's present "read".
            let ran_at = if kind == ScenarioKind::AssistantReadonly {
                AgentAccess::ReadWrite
            } else {
                AgentAccess::Read
            };
            let (store, folder) = seeded_store(ran_at);
            self.assistant.store = Some(Arc::clone(&store));
            self.assistant.scenario_folder = Some(folder);
            if let Ok(loaded) = store.load(id, "top-customers") {
                chat.update(cx, |chat, cx| {
                    chat.open_stored(super::session::Opened::new(loaded), name, cx)
                });
            }
            self.assistant_sync(cx);
            #[cfg(any(test, feature = "ui-scenarios"))]
            {
                self.assistant.scenario_open_history = kind == ScenarioKind::AssistantHistory;
            }
            return;
        }
        if kind == ScenarioKind::AssistantEditorAction {
            // A SQL editor with a statement in it; "Explain" on the part the user selected:
            // the composed message goes through the chat like any user message.
            let sql = "SELECT customer_id, SUM(total) AS revenue\nFROM public.orders\nWHERE status = 'paid'\nGROUP BY customer_id\nORDER BY revenue DESC\nLIMIT 5;";
            let tab = self.workspace.open_query_tab_with_metadata(
                "Query",
                TabMetadata {
                    connection_id: id.to_owned(),
                    database: "warehouse".to_owned(),
                    schema: Some("public".to_owned()),
                    ..Default::default()
                },
            );
            if let Some(tab) = self.workspace.tab_mut(tab) {
                tab.sql = sql.to_owned();
            }
            self.assistant_send(
                super::ask::compose(super::ask::EditorAsk::Explain, sql, None),
                cx,
            );
            return;
        }
        if matches!(
            kind,
            ScenarioKind::Assistant
                | ScenarioKind::AssistantStreaming
                | ScenarioKind::AssistantError
        ) {
            self.assistant_send("Who are my top five customers by revenue?".to_owned(), cx);
        } else if writes {
            self.assistant_send(
                if production {
                    "Empty the orders table."
                } else if kind == ScenarioKind::AssistantApprovalLong {
                    "Archive the old orders."
                } else {
                    "Cancel order 1042."
                }
                .to_owned(),
                cx,
            );
        }
    }
}
