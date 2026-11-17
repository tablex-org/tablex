//! `add_connection`, `duplicate_connection` and `remove_connection`: only on a layer built with a
//! connection admin, every call asked before anything changes, what reaches the admin, the access
//! rule of a copy, the audit line, and the session of a removed connection closed.

use engine_agent::testing::{connection, FakeCatalog, FakeProvider, FakeSession};
use engine_agent::{
    read_entries, AgentAccess, AgentError, ApprovalDecision, AuditConfig, AuditLog, CancelToken,
    ConnectionAdmin, ConnectionChange, ConnectionChanged, ConnectionKind, ToolLayer, ToolOutcome,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

const PG: &str = "pg1";
const SECRET: &str = "hunter2-Zq81-secret";

#[derive(Default)]
struct FakeAdmin {
    /// What the user answers.
    decision: Mutex<Option<ApprovalDecision>>,
    /// What `check` says is wrong with the change.
    invalid: Mutex<Option<String>>,
    /// `check`, `approve` and `apply` in order, with what they were about.
    calls: Mutex<Vec<String>>,
    /// Edits the catalog while the question is open.
    while_asking: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl FakeAdmin {
    fn new(decision: ApprovalDecision) -> Arc<Self> {
        let admin = Self::default();
        *admin.decision.lock().unwrap() = Some(decision);
        Arc::new(admin)
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn about(change: &ConnectionChange<'_>) -> String {
        match change {
            ConnectionChange::Add(request) => format!("add {}", request.name),
            ConnectionChange::Duplicate { source, name } => format!("duplicate {} as {name}", source.id),
            ConnectionChange::Remove { connection } => format!("remove {}", connection.id),
        }
    }
}

impl ConnectionAdmin for FakeAdmin {
    fn check(&self, change: &ConnectionChange<'_>) -> Result<String, AgentError> {
        self.calls.lock().unwrap().push(format!("check {}", Self::about(change)));
        match self.invalid.lock().unwrap().clone() {
            Some(reason) => Err(AgentError::invalid(reason)),
            None => Ok(Self::about(change)),
        }
    }

    fn approve(&self, change: &ConnectionChange<'_>, _cancel: &CancelToken) -> ApprovalDecision {
        self.calls.lock().unwrap().push(format!("approve {}", Self::about(change)));
        if let Some(edit) = &*self.while_asking.lock().unwrap() {
            edit();
        }
        self.decision.lock().unwrap().clone().unwrap()
    }

    fn apply(&self, change: &ConnectionChange<'_>) -> Result<ConnectionChanged, AgentError> {
        self.calls.lock().unwrap().push(format!("apply {}", Self::about(change)));
        Ok(ConnectionChanged {
            connection_id: match change {
                ConnectionChange::Remove { connection } => connection.id.clone(),
                _ => PG.into(),
            },
            password_saved: matches!(change, ConnectionChange::Add(request) if request.password.is_some()),
            app_notified: true,
            warning: None,
        })
    }
}

struct Rig {
    layer: ToolLayer,
    catalog: Arc<FakeCatalog>,
    admin: Arc<FakeAdmin>,
    audit: Arc<AuditLog>,
    audit_path: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

fn rig(access: Option<AgentAccess>, decision: ApprovalDecision, with_admin: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let audit_path = dir.path().join("audit.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path, AuditConfig::default()).unwrap());
    let catalog = FakeCatalog::new(vec![connection(PG, ConnectionKind::PostgreSql, access)]);
    let provider = FakeProvider::new();
    provider.with_session(PG, FakeSession::new(ConnectionKind::PostgreSql).arc());
    let admin = FakeAdmin::new(decision);
    let layer = ToolLayer::new(catalog.clone(), provider, "test").with_audit(Arc::clone(&audit));
    let layer = if with_admin {
        layer.with_connection_admin(admin.clone())
    } else {
        layer
    };
    Rig { layer, catalog, admin, audit, audit_path, _dir: dir }
}

impl Rig {
    fn entries(&self) -> Vec<engine_agent::AuditEntry> {
        self.audit.flush();
        read_entries(&self.audit_path, 100).unwrap().entries
    }
}

fn code(outcome: &ToolOutcome) -> &str {
    outcome.code().unwrap_or("ok")
}

fn ok(outcome: ToolOutcome) -> Value {
    match outcome {
        ToolOutcome::Success(value) => value,
        ToolOutcome::Failure(error) => panic!("{error:?}"),
    }
}

fn message(outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Failure(error) => error.message.clone(),
        ToolOutcome::Success(value) => value.to_string(),
    }
}

fn add_args() -> Value {
    json!({"name": "shop", "kind": "postgresql", "host": "db.local", "port": 5432, "password": SECRET})
}

#[test]
fn the_tools_exist_only_on_a_layer_with_an_admin() {
    let without = rig(Some(AgentAccess::Read), ApprovalDecision::Approve, false);
    let names: Vec<_> = without.layer.tools().iter().map(|tool| tool.name).collect();
    assert_eq!(names.len(), 19);
    for tool in ["add_connection", "duplicate_connection", "remove_connection"] {
        assert!(!names.contains(&tool), "{tool}");
        assert_eq!(code(&without.layer.call(tool, json!({"connection_id": PG, "name": "x", "kind": "sqlite"}))), "agent.unknown_tool");
    }
    let with = rig(Some(AgentAccess::Read), ApprovalDecision::Approve, true);
    let names: Vec<_> = with.layer.tools().iter().map(|tool| tool.name).collect();
    assert_eq!(&names[19..], ["add_connection", "duplicate_connection", "remove_connection"]);
    assert!(with.admin.calls().is_empty());
}

#[test]
fn an_approved_add_is_checked_asked_then_made_and_the_audit_line_has_no_password() {
    let rig = rig(None, ApprovalDecision::Approve, true);
    let result = ok(rig.layer.call("add_connection", add_args()));
    assert_eq!(
        result,
        json!({"connection_id": PG, "agent_access": "off", "password_saved": true, "app_notified": true})
    );
    assert_eq!(rig.admin.calls(), ["check add shop", "approve add shop", "apply add shop"]);
    let entry = &rig.entries()[0];
    assert_eq!((entry.tool.as_str(), entry.outcome.as_str()), ("add_connection", "ok"));
    assert_eq!(entry.approval.as_deref(), Some("approved"));
    assert_eq!(entry.connection_id.as_deref(), Some(PG));
    assert!(!serde_json::to_string(entry).unwrap().contains(SECRET));
}

#[test]
fn a_new_connection_reports_the_access_its_type_gives() {
    let rig = rig(Some(AgentAccess::ReadWrite), ApprovalDecision::Approve, true);
    // The fake admin "adds" the id that already exists in the catalog: its access is what is reported.
    let result = ok(rig.layer.call("add_connection", add_args()));
    assert_eq!(result["agent_access"], "read_write");
}

#[test]
fn a_decline_a_refusal_and_no_answer_change_nothing_and_are_audited() {
    for (decision, outcome, note) in [
        (ApprovalDecision::Deny { reason: None }, "agent.write_refused", "declined"),
        (ApprovalDecision::Deny { reason: Some("the question was dismissed".into()) }, "agent.write_refused", "declined: the question was dismissed"),
        (ApprovalDecision::Refuse { reason: "this MCP client cannot ask the user".into() }, "agent.write_refused", "refused: this MCP client cannot ask the user"),
        (ApprovalDecision::Unanswered { reason: "no usable answer came back".into() }, "agent.write_refused", "unanswered: no usable answer came back"),
    ] {
        let rig = rig(None, decision, true);
        for (tool, args) in [
            ("add_connection", add_args()),
            ("duplicate_connection", json!({"connection_id": PG, "name": "copy"})),
            ("remove_connection", json!({"connection_id": PG})),
        ] {
            let result = rig.layer.call(tool, args);
            assert_eq!(code(&result), outcome, "{tool}");
            assert!(!message(&result).contains(SECRET));
        }
        assert!(rig.admin.calls().iter().all(|call| !call.starts_with("apply")), "{:?}", rig.admin.calls());
        let entries = rig.entries();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|entry| entry.approval.as_deref() == Some(note)), "{entries:?}");
    }
}

#[test]
fn a_change_that_fails_the_check_is_never_asked() {
    let rig = rig(None, ApprovalDecision::Approve, true);
    *rig.admin.invalid.lock().unwrap() = Some("the name is in use".into());
    let result = rig.layer.call("add_connection", add_args());
    assert_eq!(code(&result), "agent.invalid_arguments");
    assert!(message(&result).contains("in use"));
    assert_eq!(rig.admin.calls(), ["check add shop"]);
}

#[test]
fn the_arguments_are_validated_before_the_admin_sees_them() {
    let rig = rig(None, ApprovalDecision::Approve, true);
    for (tool, args) in [
        ("add_connection", json!({"name": "x"})),
        ("add_connection", json!({"name": "x", "kind": "oracle"})),
        ("add_connection", json!({"name": "x", "kind": "sqlite", "path": "/a.db", "ssh": {"host": "b"}})),
        ("add_connection", json!({"name": "x", "kind": "postgresql", "host": "h", "options": {"tls": "maybe"}})),
        ("add_connection", json!({"name": "x", "kind": "postgresql", "host": "h", "port": "5432"})),
        ("duplicate_connection", json!({"connection_id": PG})),
        ("remove_connection", json!({})),
    ] {
        assert_eq!(code(&rig.layer.call(tool, args)), "agent.invalid_arguments", "{tool}");
    }
    assert!(rig.admin.calls().is_empty());
    // Not even the password of a refused request reaches the model.
    let result = rig.layer.call("add_connection", json!({"name": "x", "kind": "oracle", "password": SECRET}));
    assert!(!message(&result).contains(SECRET));
}

#[test]
fn an_unknown_connection_is_refused_before_the_question() {
    let rig = rig(None, ApprovalDecision::Approve, true);
    for tool in ["duplicate_connection", "remove_connection"] {
        let result = rig.layer.call(tool, json!({"connection_id": "nope", "name": "copy"}));
        assert_eq!(code(&result), "agent.invalid_arguments", "{tool}");
    }
    assert!(rig.admin.calls().is_empty());
}

#[test]
fn a_copy_is_refused_when_it_would_get_more_access_than_the_original() {
    let rig = rig(Some(AgentAccess::Off), ApprovalDecision::Approve, true);
    // The type gives `read`; the original was set to `off` by the user.
    rig.catalog.update(PG, |info| info.connection_type.default_agent_access = Some(AgentAccess::Read));
    let result = rig.layer.call("duplicate_connection", json!({"connection_id": PG, "name": "copy"}));
    assert_eq!(code(&result), "agent.access_denied");
    assert!(rig.admin.calls().is_empty());
    // Lowered below the type's default: refused too. At the default, or above it, it is not.
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::SchemaOnly));
    assert_eq!(code(&rig.layer.call("duplicate_connection", json!({"connection_id": PG, "name": "copy"}))), "agent.access_denied");
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::Read));
    ok(rig.layer.call("duplicate_connection", json!({"connection_id": PG, "name": "copy"})));
    rig.catalog.update(PG, |info| info.agent_access = Some(AgentAccess::ReadWrite));
    ok(rig.layer.call("duplicate_connection", json!({"connection_id": PG, "name": "copy"})));
    // A copy of a connection that is off, whose type gives nothing, is fine: it is off too.
    rig.catalog.update(PG, |info| {
        info.agent_access = Some(AgentAccess::Off);
        info.connection_type.default_agent_access = None;
    });
    ok(rig.layer.call("duplicate_connection", json!({"connection_id": PG, "name": "copy"})));
}

#[test]
fn a_removal_closes_the_agents_session() {
    let rig = rig(Some(AgentAccess::Read), ApprovalDecision::Approve, true);
    ok(rig.layer.call("open_session", json!({"connection_id": PG})));
    assert!(rig.layer.is_open(PG));
    let result = ok(rig.layer.call("remove_connection", json!({"connection_id": PG})));
    assert_eq!(result, json!({"connection_id": PG, "app_notified": true}));
    assert!(!rig.layer.is_open(PG));
    // A declined removal leaves it open.
    let kept = self::rig(Some(AgentAccess::Read), ApprovalDecision::Deny { reason: None }, true);
    ok(kept.layer.call("open_session", json!({"connection_id": PG})));
    assert_eq!(code(&kept.layer.call("remove_connection", json!({"connection_id": PG}))), "agent.write_refused");
    assert!(kept.layer.is_open(PG));
}

#[test]
fn a_connection_edited_or_removed_while_the_question_is_open_is_left_alone() {
    let rig = rig(Some(AgentAccess::Read), ApprovalDecision::Approve, true);
    let catalog = rig.catalog.clone();
    *rig.admin.while_asking.lock().unwrap() = Some(Box::new(move || {
        catalog.update(PG, |info| info.revision = engine_agent::Revision::new(7));
    }));
    // The fake catalog's connection starts with an unknown revision: edit it before the call.
    rig.catalog.update(PG, |info| info.revision = engine_agent::Revision::new(3));
    let result = rig.layer.call("remove_connection", json!({"connection_id": PG}));
    assert_eq!(code(&result), "agent.not_connected");
    assert!(rig.admin.calls().iter().all(|call| !call.starts_with("apply")));
    assert_eq!(rig.entries()[0].approval.as_deref(), Some("approved, then refused: the connection was edited"));
}

#[test]
fn a_cancelled_call_asks_nothing() {
    let rig = rig(None, ApprovalDecision::Approve, true);
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = rig.layer.call_cancellable("add_connection", add_args(), &cancel);
    assert_eq!(code(&result), "agent.cancelled");
    assert!(rig.admin.calls().iter().all(|call| !call.starts_with("approve") && !call.starts_with("apply")));
}
