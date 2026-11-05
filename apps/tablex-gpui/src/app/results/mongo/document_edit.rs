//! Editing the selected document of the Mongo Document view as one JSON text.
//!
//! The editor holds the document as it currently stands (staged edits
//! applied). Save never writes to the server: the top-level difference becomes
//! staged `$set` / `$unset` changes, one per field, that go out with the
//! regular Commit and use its undo, redo and rollback. While the editor is open
//! the document list, the Table switch and the view controls are locked, so the
//! text can only leave through Save or Cancel.

use super::super::super::*;
use super::{check_mongo_identity, mongo_field_refusal, MongoEditRefusal, MongoInputError};
use crate::mongo_result::{MongoDocumentKey, PendingMongoMutation, STAGED_OPERATION_ID};

/// What the status row says when something would leave the open editor behind.
pub(in crate::app) const EDITING_MESSAGE: &str = "Save or cancel the document edit first.";

/// The open document editor of one tab: created by Edit, dropped by Save or
/// Cancel (or when its document is no longer the selected one).
pub(in crate::app) struct MongoDocumentEdit {
    /// The document the text was seeded from.
    pub(in crate::app) key: MongoDocumentKey,
    pub(in crate::app) editor: gpui::Entity<EditorState>,
    /// Why the last Save was refused; shown under the editor.
    pub(in crate::app) error: Option<String>,
}

/// Whether the selected document can be edited, and if not why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum DocumentEditability {
    Editable,
    ReadOnly(Option<String>),
}

/// One top-level change a document edit makes.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) enum MongoFieldChange {
    Set {
        field: String,
        value: serde_json::Value,
    },
    Unset {
        field: String,
    },
}

/// Whether two canonical Extended JSON values are the same BSON value. Documents
/// compare field by field IN ORDER, as MongoDB does (`{a: 1, b: 2}` is not
/// `{b: 2, a: 1}`); the order of the top-level fields is never compared (see
/// [`mongo_document_changes`]).
fn same_bson_value(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((lk, lv), (rk, rv))| lk == rk && same_bson_value(lv, rv))
        }
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right).all(|(l, r)| same_bson_value(l, r))
        }
        _ => left == right,
    }
}

/// The fields `text` adds, changes and removes relative to `current` (a
/// document in canonical Extended JSON). `text` is read like every Mongo
/// value the app takes: shell syntax or Extended JSON, a plain number typed
/// like mongosh types it, and fields compared as canonical Extended JSON so
/// `1` (Int32), `1.0` (Double) and `"1"` differ. `_id` must stay as it is and
/// every written or removed field name must be one `$set` reads literally.
/// A changed field keeps its place (a `$set` cannot move a top-level field and
/// a new one goes last), so reordering top-level fields is no change; nested
/// documents are compared in order.
pub(in crate::app) fn mongo_document_changes(
    current: &serde_json::Value,
    text: &str,
    has_identity: bool,
) -> Result<Vec<MongoFieldChange>, MongoInputError> {
    let edited = engine_mongo_script::shell_value::parse_document(
        text,
        engine_mongo_script::shell_value::NumberTyping::Mongosh,
    )
    .map_err(|error| MongoInputError::new(format!("Not a valid document: {error}")))?;
    let serde_json::Value::Object(edited) = bson::Bson::Document(edited).into_canonical_extjson()
    else {
        return Err(MongoInputError::new("Not a valid document: expected { … }"));
    };
    let empty = serde_json::Map::new();
    let current = current.as_object().unwrap_or(&empty);
    if !match (current.get("_id"), edited.get("_id")) {
        (Some(old), Some(new)) => same_bson_value(old, new),
        (None, None) => true,
        _ => false,
    } {
        return Err(MongoInputError::new(format!(
            "{} MongoDB does not let a document change or lose its _id: put the original _id \
             back, or insert a copy as a new document.",
            MongoEditRefusal::IdField
        )));
    }
    let refuse = |field: &str| match mongo_field_refusal(has_identity, field) {
        Some(refusal) => Err(MongoInputError::new(refusal.to_string())),
        None => Ok(()),
    };
    let mut changes = Vec::new();
    for (field, value) in &edited {
        if field != "_id" && !current.get(field).is_some_and(|old| same_bson_value(old, value)) {
            refuse(field)?;
            changes.push(MongoFieldChange::Set {
                field: field.clone(),
                value: value.clone(),
            });
        }
    }
    for field in current.keys() {
        if field != "_id" && !edited.contains_key(field) {
            refuse(field)?;
            changes.push(MongoFieldChange::Unset {
                field: field.clone(),
            });
        }
    }
    Ok(changes)
}

/// What a grid cell shows for a field staged to `value`.
fn staged_display(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::String(text) => text.clone(),
        _ => crate::mongo_result::ejson_scalar(value)
            .map(|(display, _)| display)
            .or_else(|| crate::cell_value::shell_text(value, false))
            .unwrap_or_else(|| value.to_string()),
    }
}

/// The staged mutations of `changes` for the document `{ "_id": … }` of
/// `row`: one single-field `$set` or `$unset` each, the same shape the grid's
/// cell edit stages.
pub(in crate::app) fn mongo_document_edit_mutations(
    changes: Vec<MongoFieldChange>,
    identity: &serde_json::Value,
    database: &str,
    collection: &str,
    row: usize,
) -> Result<Vec<PendingMongoMutation>, MongoInputError> {
    check_mongo_identity(identity)?;
    let request = |payload| engine_driver_api::MutationRequest {
        operation_id: STAGED_OPERATION_ID,
        kind: "update".into(),
        database: Some(database.to_string()),
        object: Some(collection.to_string()),
        identity: identity.clone(),
        payload,
    };
    Ok(changes
        .into_iter()
        .map(|change| match change {
            MongoFieldChange::Set { field, value } => PendingMongoMutation::Update {
                request: request(serde_json::json!({"$set": {field.clone(): value.clone()}})),
                row,
                display_text: staged_display(&value),
                field,
                value,
            },
            MongoFieldChange::Unset { field } => PendingMongoMutation::Unset {
                request: request(serde_json::json!({"$unset": {field.clone(): ""}})),
                row,
                field,
            },
        })
        .collect())
}

impl AppRoot {
    /// Whether the document selected in the tab's Document view can be edited.
    /// Reads only state the frame already holds; text is built only for a
    /// read-only reason.
    pub(in crate::app) fn mongo_document_editability(&self, tab_id: u64) -> DocumentEditability {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return DocumentEditability::ReadOnly(None);
        };
        let inspector = &runtime.mongo.inspector;
        let (Some(selected), Some(snapshot)) = (inspector.selected, inspector.snapshot.as_ref())
        else {
            return DocumentEditability::ReadOnly(None);
        };
        if let Some(message) = self.read_only_edit_message(tab_id) {
            return DocumentEditability::ReadOnly(Some(message));
        }
        if matches!(
            runtime.mongo.commit_status,
            crate::mongo_result::MongoCommitStatus::Running
        ) {
            return DocumentEditability::ReadOnly(Some("A commit is running.".into()));
        }
        if snapshot.mutation_identity.is_none() {
            return DocumentEditability::ReadOnly(Some(MongoEditRefusal::NoIdentity.to_string()));
        }
        if runtime.mongo.mutations.is_deleted(selected.row_index as usize) {
            return DocumentEditability::ReadOnly(Some(
                "This document is staged for deletion.".into(),
            ));
        }
        DocumentEditability::Editable
    }

    /// Opens the editor on the selected document, staged edits included, in
    /// the current Shell / Canonical mode.
    pub(in crate::app) fn start_mongo_document_edit(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view(tab_id).mongo.document_edit.is_some() {
            return;
        }
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if self.mongo_document_editability(tab_id) != DocumentEditability::Editable {
            return;
        }
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        let (Some(key), Some(snapshot)) = (
            runtime.mongo.inspector.selected,
            runtime.mongo.inspector.snapshot.as_ref(),
        ) else {
            return;
        };
        let mode = self.view(tab_id).mongo.document_json_mode;
        let text = if runtime.mongo.mutations.is_empty() {
            super::super::json_view::document_json_text(&snapshot.document, &snapshot.json, mode)
        } else {
            let document = crate::mongo_result::apply_staged_updates(
                &snapshot.document,
                key.row_index as usize,
                &runtime.mongo.mutations,
            );
            let canonical = crate::mongo_result::pretty_json(&document);
            super::super::json_view::document_json_text(&document, &canonical, mode)
        };
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(true)
                .folding(true)
                .soft_wrap(false)
                .default_value(text)
        });
        editor.update(cx, |state, cx| state.focus(window, cx));
        self.view_mut(tab_id).mongo.document_edit = Some(MongoDocumentEdit {
            key,
            editor,
            error: None,
        });
        self.notify_tab(tab_id, cx);
    }

    /// Closes the editor and drops its text.
    pub(in crate::app) fn cancel_mongo_document_edit(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view_mut(tab_id).mongo.document_edit.take().is_some() {
            self.finish_mongo_document_edit(tab_id, window, cx);
        }
    }

    /// After the editor closed: the "Save or cancel" reminder goes and focus
    /// returns to the documents view.
    fn finish_mongo_document_edit(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let detail_ui = &mut self.view_mut(tab_id).detail_ui;
        if detail_ui.error.as_deref() == Some(EDITING_MESSAGE) {
            detail_ui.error = None;
        }
        self.restore_grid_focus(tab_id, Some(window), cx);
        self.notify_tab(tab_id, cx);
    }

    /// Turns the edited text into staged changes. A refusal (bad text, a
    /// changed `_id`, a field `$set` cannot address) keeps the editor open and
    /// says why under it; an edit that changes nothing just closes.
    pub(in crate::app) fn save_mongo_document_edit(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.view(tab_id).mongo.document_edit.as_ref() else {
            return;
        };
        // What was staged meanwhile (a delete, a read-only switch) can make the
        // document unwritable: say so, keep the text.
        if let DocumentEditability::ReadOnly(reason) = self.mongo_document_editability(tab_id) {
            let reason = reason.unwrap_or_else(|| "This document can no longer be edited.".into());
            if let Some(edit) = self.view_mut(tab_id).mongo.document_edit.as_mut() {
                edit.error = Some(reason);
            }
            self.notify_tab(tab_id, cx);
            return;
        }
        let (key, text) = (edit.key, edit.editor.read(cx).value().to_string());
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        let inspector = &runtime.mongo.inspector;
        let outcome = match inspector.snapshot.as_ref().filter(|snapshot| {
            inspector.selected == Some(key) && snapshot.key == key
        }) {
            None => Err(MongoInputError::new(
                "The document is no longer the one that was opened. Cancel and open it again.",
            )),
            Some(snapshot) => {
                let row = key.row_index as usize;
                let current = crate::mongo_result::apply_staged_updates(
                    &snapshot.document,
                    row,
                    &runtime.mongo.mutations,
                );
                mongo_document_changes(&current, &text, snapshot.mutation_identity.is_some())
                    .and_then(|changes| match &snapshot.mutation_identity {
                        Some(identity) if !changes.is_empty() => mongo_document_edit_mutations(
                            changes,
                            identity,
                            &tab.database,
                            &tab.collection,
                            row,
                        ),
                        _ => Ok(Vec::new()),
                    })
            }
        };
        match outcome {
            Err(error) => {
                if let Some(edit) = self.view_mut(tab_id).mongo.document_edit.as_mut() {
                    edit.error = Some(error.to_string());
                }
            }
            Ok(mutations) => {
                self.view_mut(tab_id).mongo.document_edit = None;
                self.stage_mongo_mutations(tab_id, mutations, cx);
                self.refresh_cell_detail(tab_id);
                self.finish_mongo_document_edit(tab_id, window, cx);
                return;
            }
        }
        self.notify_tab(tab_id, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> serde_json::Value {
        json!({
            "_id": {"$oid": "000000000000000000000002"},
            "name": "Ada",
            "age": {"$numberInt": "36"},
            "ratio": {"$numberDouble": "1.0"},
            "big": {"$numberLong": "9007199254740993"},
            "at": {"$date": {"$numberLong": "1725148800123"}},
            "price": {"$numberDecimal": "12.50"},
            "none": null,
            "tags": ["a", {"$numberLong": "5"}],
            "nested": {"ok": true, "n": {"$numberDouble": "NaN"}},
            "bin": {"$binary": {"base64": "AAH/", "subType": "00"}},
            "re": {"$regularExpression": {"pattern": "^a", "options": "i"}},
            "ts": {"$timestamp": {"t": 1, "i": 2}}
        })
    }

    fn changes(current: &serde_json::Value, text: &str) -> Vec<MongoFieldChange> {
        mongo_document_changes(current, text, true).expect(text)
    }

    #[test]
    fn the_text_the_editor_is_seeded_with_reads_back_as_no_change_in_both_modes() {
        let current = sample();
        let shell = crate::cell_value::shell_text(&current, true).unwrap();
        let canonical = serde_json::to_string_pretty(&current).unwrap();
        assert_eq!(changes(&current, &shell), vec![], "{shell}");
        assert_eq!(changes(&current, &canonical), vec![], "{canonical}");
    }

    #[test]
    fn added_changed_and_removed_fields_are_separate_changes_and_unchanged_ones_are_not() {
        let current = sample();
        let shell = crate::cell_value::shell_text(&current, true).unwrap();
        let text = shell
            .replace("\"Ada\"", "\"Grace\"")
            .replace("  \"none\": null,\n", "")
            .replacen("{\n", "{\n  \"extra\": 7,\n", 1);
        let mut found = changes(&current, &text);
        found.sort_by_key(|change| match change {
            MongoFieldChange::Set { field, .. } | MongoFieldChange::Unset { field } => field.clone(),
        });
        assert_eq!(
            found,
            vec![
                MongoFieldChange::Set {
                    field: "extra".into(),
                    value: json!({"$numberInt": "7"})
                },
                MongoFieldChange::Set {
                    field: "name".into(),
                    value: json!("Grace")
                },
                MongoFieldChange::Unset { field: "none".into() },
            ]
        );
    }

    #[test]
    fn equality_is_by_bson_type_so_int32_double_and_string_one_differ() {
        let current = json!({"_id": {"$numberInt": "1"}, "n": {"$numberInt": "1"}});
        let set = |value: serde_json::Value| {
            vec![MongoFieldChange::Set {
                field: "n".into(),
                value,
            }]
        };
        let doc = |n: &str| format!("{{ \"_id\": 1, \"n\": {n} }}");
        assert_eq!(changes(&current, &doc("1")), vec![]);
        assert_eq!(changes(&current, &doc("1.0")), set(json!({"$numberDouble": "1.0"})));
        assert_eq!(changes(&current, &doc("\"1\"")), set(json!("1")));
        assert_eq!(changes(&current, &doc("Long('1')")), set(json!({"$numberLong": "1"})));
        assert_eq!(
            changes(&current, &doc("{\"$numberLong\": \"1\"}")),
            set(json!({"$numberLong": "1"}))
        );
    }

    #[test]
    fn nested_documents_compare_in_order_but_top_level_order_is_not_a_change() {
        let current = json!({
            "_id": {"$numberInt": "1"},
            "a": {"x": {"$numberInt": "1"}, "y": {"$numberInt": "2"}},
            "list": [{"p": {"$numberInt": "1"}, "q": {"$numberInt": "2"}}],
            "b": {"$numberInt": "3"}
        });
        let text = |body: &str| format!("{{ \"_id\": 1, {body} }}");
        let a = "\"a\": { \"x\": 1, \"y\": 2 }";
        let list = "\"list\": [{ \"p\": 1, \"q\": 2 }]";
        let b = "\"b\": 3";
        assert_eq!(changes(&current, &text(&format!("{a}, {list}, {b}"))), vec![]);
        // Fields of the top level in another order: nothing to write (`$set` cannot move them).
        assert_eq!(changes(&current, &text(&format!("{b}, {list}, {a}"))), vec![]);
        // A nested document whose fields moved is a different value, and so is one inside an array.
        let moved = changes(&current, &text(&format!("\"a\": {{ \"y\": 2, \"x\": 1 }}, {list}, {b}")));
        assert!(matches!(moved.as_slice(), [MongoFieldChange::Set { field, .. }] if field == "a"), "{moved:?}");
        let moved = changes(&current, &text(&format!("{a}, \"list\": [{{ \"q\": 2, \"p\": 1 }}], {b}")));
        assert!(matches!(moved.as_slice(), [MongoFieldChange::Set { field, .. }] if field == "list"), "{moved:?}");
        // An `_id` that is a document is compared in order too.
        let id_doc = json!({"_id": {"k": {"$numberInt": "1"}, "j": {"$numberInt": "2"}}});
        assert!(mongo_document_changes(&id_doc, "{ \"_id\": { \"k\": 1, \"j\": 2 } }", true).unwrap().is_empty());
        assert!(mongo_document_changes(&id_doc, "{ \"_id\": { \"j\": 2, \"k\": 1 } }", true).is_err());
    }

    #[test]
    fn bad_text_and_an_id_that_moved_are_refused_with_a_message() {
        let current = json!({"_id": {"$numberInt": "1"}, "a": "x"});
        let refused = |text: &str, has_identity: bool| {
            mongo_document_changes(&current, text, has_identity)
                .expect_err(text)
                .to_string()
        };
        assert!(refused("{ \"_id\": 1, ", true).contains("Not a valid document"));
        assert!(refused("[1, 2]", true).contains("Not a valid document"));
        assert!(refused("{ \"_id\": 1, \"a\": 1, \"a\": 2 }", true).contains("duplicate field \"a\""));
        for text in [
            "{ \"_id\": 2, \"a\": \"x\" }",
            "{ \"a\": \"x\" }",
            "{ \"_id\": \"1\", \"a\": \"x\" }",
        ] {
            let message = refused(text, true);
            assert!(message.contains("_id cannot be changed"), "{text}: {message}");
        }
        // A name `$set` would read as a path or an operator, and a row without
        // a write identity, are refused like the grid refuses them.
        assert!(refused("{ \"_id\": 1, \"a\": \"x\", \"b.c\": 1 }", true).contains("\"b.c\""));
        assert!(refused("{ \"_id\": 1, \"a\": \"x\", \"$b\": 1 }", true).contains("\"$b\""));
        assert!(refused("{ \"_id\": 1, \"a\": \"y\" }", false).contains("no _id"));
        let dotted = json!({"_id": {"$numberInt": "1"}, "a.b": 1});
        assert!(mongo_document_changes(&dotted, "{ \"_id\": 1 }", true)
            .unwrap_err()
            .to_string()
            .contains("\"a.b\""));
    }

    #[test]
    fn staged_changes_are_single_field_updates_filtered_on_the_id_only() {
        let identity = json!({"_id": {"$oid": "000000000000000000000002"}});
        let mutations = mongo_document_edit_mutations(
            vec![
                MongoFieldChange::Set {
                    field: "n".into(),
                    value: json!({"$numberInt": "7"}),
                },
                MongoFieldChange::Unset { field: "gone".into() },
            ],
            &identity,
            "shop",
            "invoices",
            4,
        )
        .unwrap();
        let [PendingMongoMutation::Update { request: set, row: 4, display_text, .. }, PendingMongoMutation::Unset { request: unset, row: 4, field }] =
            mutations.as_slice()
        else {
            panic!("{mutations:?}");
        };
        assert_eq!(display_text, "7");
        assert_eq!(field, "gone");
        for request in [set, unset] {
            assert_eq!(request.kind, "update");
            assert_eq!(request.identity, identity);
            assert_eq!(request.database.as_deref(), Some("shop"));
            assert_eq!(request.object.as_deref(), Some("invoices"));
            assert_eq!(request.operation_id, STAGED_OPERATION_ID);
        }
        assert_eq!(set.payload, json!({"$set": {"n": {"$numberInt": "7"}}}));
        assert_eq!(unset.payload, json!({"$unset": {"gone": ""}}));
        // The driver reads both payloads as operator documents.
        for payload in [&set.payload, &unset.payload] {
            assert!(matches!(bson::Bson::try_from(payload.clone()), Ok(bson::Bson::Document(_))));
        }
        assert!(mongo_document_edit_mutations(vec![], &json!({"name": "x"}), "d", "c", 0).is_err());
        assert!(mongo_document_edit_mutations(vec![], &json!({"_id": 1, "x": 2}), "d", "c", 0).is_err());
    }
}
