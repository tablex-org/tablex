use bson::Bson;
use engine_driver_api::MutationRequest;
use engine_mongo_script::shell_value::{self, NumberTyping};
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoResultView {
    #[default]
    Table,
    Document,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoDocumentView {
    #[default]
    Json,
    Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoColumnTab {
    #[default]
    Visible,
    Projection,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PendingMongoMutation {
    Update {
        request: MutationRequest,
        row: usize,
        field: String,
        value: serde_json::Value,
        display_text: String,
    },
    /// `$unset` of one top-level field (a document edit removed it).
    Unset {
        request: MutationRequest,
        row: usize,
        field: String,
    },
    Insert {
        request: MutationRequest,
        document: serde_json::Value,
    },
    Delete {
        request: MutationRequest,
        row: usize,
    },
}

impl PendingMongoMutation {
    pub fn request(&self) -> &MutationRequest {
        match self {
            Self::Update { request, .. }
            | Self::Unset { request, .. }
            | Self::Insert { request, .. }
            | Self::Delete { request, .. } => request,
        }
    }
}

/// Monotonic change counter for [`MongoMutationHistory`]. It never takes part
/// in equality: two histories with the same content compare equal.
#[derive(Debug, Clone, Copy, Default)]
pub struct HistoryRevision(u64);

impl PartialEq for HistoryRevision {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl HistoryRevision {
    fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }
}

/// Whether `mutation` is a `$set` of `field` of `row`.
fn is_update_for(mutation: &PendingMongoMutation, row: usize, field: &str) -> bool {
    matches!(mutation, PendingMongoMutation::Update { row: mutation_row, field: mutation_field, .. } if *mutation_row == row && mutation_field == field)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MongoMutationHistory {
    entries: Vec<PendingMongoMutation>,
    /// Where each undo step ends in `entries` (exclusive), ascending, the last one `entries.len()`: one `stage` is one step,
    /// and so is a group of changes staged together ([`Self::stage_group`]). `cursor` is always `0` or one of these.
    steps: Vec<usize>,
    cursor: usize,
    revision: HistoryRevision,
}

impl MongoMutationHistory {
    /// O(1) change signal: bumped by every operation that can change
    /// [`Self::active`] (stage, undo, redo, rollback, prefix removal, update
    /// removal). Views key derived data on it instead of comparing content.
    pub fn revision(&self) -> u64 {
        self.revision.0
    }

    pub fn stage(&mut self, mutation: PendingMongoMutation) {
        self.stage_group([mutation]);
    }

    /// Stages `mutations` as one undo step: one Undo takes all of them back and one Redo brings them all again.
    pub fn stage_group(&mut self, mutations: impl IntoIterator<Item = PendingMongoMutation>) {
        let mut mutations = mutations.into_iter().peekable();
        if mutations.peek().is_none() {
            return;
        }
        self.revision.bump();
        self.entries.truncate(self.cursor);
        self.steps.retain(|end| *end <= self.cursor);
        self.entries.extend(mutations);
        self.steps.push(self.entries.len());
        self.cursor = self.entries.len();
    }

    pub fn active(&self) -> &[PendingMongoMutation] {
        &self.entries[..self.cursor]
    }

    pub fn len(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.cursor == 0
    }

    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    pub fn can_redo(&self) -> bool {
        self.cursor < self.entries.len()
    }

    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        self.cursor = self.steps.iter().rev().find(|end| **end < self.cursor).copied().unwrap_or(0);
        self.revision.bump();
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(end) = self.steps.iter().find(|end| **end > self.cursor).copied() else {
            return false;
        };
        self.cursor = end;
        self.revision.bump();
        true
    }

    pub fn rollback(&mut self) {
        self.revision.bump();
        self.entries.clear();
        self.steps.clear();
        self.cursor = 0;
    }

    pub fn remove_committed_prefix(&mut self, count: usize) {
        let count = count.min(self.cursor);
        self.revision.bump();
        self.entries.drain(..count);
        self.cursor -= count;
        self.entries.truncate(self.cursor);
        // A step the commit ended inside keeps what is left of it.
        self.steps = self.steps.iter().filter(|end| **end > count).map(|end| end - count).filter(|end| *end <= self.cursor).collect();
    }

    #[cfg(test)]
    pub fn update_for(&self, row: usize, field: &str) -> Option<&PendingMongoMutation> {
        self.active().iter().rev().find(|mutation| {
            matches!(
                mutation,
                PendingMongoMutation::Update {
                    row: mutation_row,
                    field: mutation_field,
                    ..
                } if *mutation_row == row && mutation_field == field
            )
        })
    }

    /// The latest active `$set` or `$unset` of `field` of `row`: what the cell
    /// stands at once the staged changes are applied in order.
    pub fn latest_field_change(&self, row: usize, field: &str) -> Option<&PendingMongoMutation> {
        self.active().iter().rev().find(|mutation| {
            matches!(
                mutation,
                PendingMongoMutation::Update { row: r, field: f, .. }
                    | PendingMongoMutation::Unset { row: r, field: f, .. }
                    if *r == row && f == field
            )
        })
    }

    /// Whether the field is, after the staged changes, removed from the row.
    pub fn is_unset(&self, row: usize, field: &str) -> bool {
        matches!(
            self.latest_field_change(row, field),
            Some(PendingMongoMutation::Unset { .. })
        )
    }

    pub fn remove_updates_for(&mut self, row: usize, field: &str) -> bool {
        let removed_active = self.active().iter().filter(|mutation| is_update_for(mutation, row, field)).count();
        if removed_active == 0 {
            return false;
        }
        self.revision.bump();
        // Where the old positions land once the removed entries are gone; a step left empty goes with them.
        let mut kept = 0usize;
        let moved: Vec<usize> = self
            .entries
            .iter()
            .map(|mutation| {
                kept += usize::from(!is_update_for(mutation, row, field));
                kept
            })
            .collect();
        let at = |end: usize| if end == 0 { 0 } else { moved[end - 1] };
        let mut steps: Vec<usize> = self.steps.iter().map(|end| at(*end)).collect();
        steps.dedup();
        steps.retain(|end| *end > 0);
        self.cursor = at(self.cursor);
        self.steps = steps;
        self.entries.retain(|mutation| !is_update_for(mutation, row, field));
        true
    }

    pub fn is_deleted(&self, row: usize) -> bool {
        self.active().iter().rev().any(|mutation| {
            matches!(mutation, PendingMongoMutation::Delete { row: mutation_row, .. } if *mutation_row == row)
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum MongoCommitStatus {
    #[default]
    Idle,
    Running,
    Partial { committed: usize, error: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MongoDocumentKey {
    pub page_id: u64,
    pub row_index: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MongoDocumentSnapshot {
    pub key: MongoDocumentKey,
    pub document: serde_json::Value,
    /// The document's `_id` (canonical Extended JSON); `None` when the page
    /// has none (a projection that dropped it). No other identity is invented.
    pub identity: Option<serde_json::Value>,
    /// The filter a write to this document uses, exactly `{ "_id": … }` as the
    /// driver reports it; `None` when the driver offers none (no usable
    /// `_id`), so the document cannot be written.
    pub mutation_identity: Option<serde_json::Value>,
    /// Pretty-printed document, rendered by the read-only JSON code editor.
    pub json: Arc<str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoFilterMode {
    #[default]
    Equals,
    NotEquals,
    Contains,
    NotContains,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
    IsNull,
    IsNotNull,
}

impl MongoFilterMode {
    pub fn needs_value(self) -> bool {
        !matches!(self, Self::IsNull | Self::IsNotNull)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoFilterValueType {
    #[default]
    Auto,
    String,
    Number,
    Boolean,
    ObjectId,
    Date,
    Int32,
    Int64,
    Decimal128,
    Json,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoFilterRule {
    pub id: u64,
    pub field: String,
    pub mode: MongoFilterMode,
    pub value_type: MongoFilterValueType,
    pub value: String,
    pub conjunction: MongoFilterConjunction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MongoFilterConjunction {
    #[default]
    And,
    Or,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MongoFilterMenu {
    Field(u64),
    Mode(u64),
    ValueType(u64),
    Conjunction(u64),
}

impl MongoFilterRule {
    pub fn new(field: impl Into<String>) -> Self {
        Self {
            id: 0,
            field: field.into(),
            mode: MongoFilterMode::Equals,
            value_type: MongoFilterValueType::Auto,
            value: String::new(),
            conjunction: MongoFilterConjunction::And,
        }
    }
}

pub fn build_mongo_filter_condition(rule: &MongoFilterRule) -> Result<serde_json::Value, String> {
    if rule.field.trim().is_empty() {
        return Err("Filter field is required".into());
    }
    if rule.mode.needs_value() && rule.value.trim().is_empty() {
        return Err(format!("Filter value is required for {}", rule.field));
    }
    let value = rule
        .mode
        .needs_value()
        .then(|| parse_mongo_filter_value(rule));
    let value = value.transpose()?;
    let condition = match rule.mode {
        MongoFilterMode::Equals => serde_json::json!({rule.field.clone(): value}),
        MongoFilterMode::NotEquals => serde_json::json!({rule.field.clone(): {"$ne": value}}),
        MongoFilterMode::Contains => serde_json::json!({rule.field.clone(): {
            "$regex": regex_escape(rule.value.trim()), "$options": "i"
        }}),
        MongoFilterMode::NotContains => serde_json::json!({rule.field.clone(): {
            "$not": {"$regex": regex_escape(rule.value.trim()), "$options": "i"}
        }}),
        MongoFilterMode::GreaterThan => serde_json::json!({rule.field.clone(): {"$gt": value}}),
        MongoFilterMode::GreaterThanOrEqual => {
            serde_json::json!({rule.field.clone(): {"$gte": value}})
        }
        MongoFilterMode::LessThan => serde_json::json!({rule.field.clone(): {"$lt": value}}),
        MongoFilterMode::LessThanOrEqual => {
            serde_json::json!({rule.field.clone(): {"$lte": value}})
        }
        MongoFilterMode::IsNull => serde_json::json!({rule.field.clone(): null}),
        MongoFilterMode::IsNotNull => serde_json::json!({rule.field.clone(): {"$ne": null}}),
    };
    Ok(condition)
}

pub fn merge_mongo_filters(
    manual_filter: &str,
    rules: &[MongoFilterRule],
) -> Result<serde_json::Value, String> {
    // The find bar takes the same text the driver reads: JSON5, shell
    // constructors and Extended JSON. Typed values travel as canonical
    // Extended JSON, so none of them changes type on the way.
    let manual = if manual_filter.trim().is_empty() || manual_filter.trim() == "{}" {
        None
    } else {
        let document = shell_value::parse_document(manual_filter.trim(), NumberTyping::Filter)
            .map_err(|error| format!("Filter is invalid: {error}"))?;
        Some(canonical(Bson::Document(document)))
    };
    let mut filtered_rules = rules.iter().filter(|rule| !rule.field.trim().is_empty());
    let structured = filtered_rules
        .next()
        .map(build_mongo_filter_condition)
        .transpose()?;
    let structured = filtered_rules.try_fold(structured, |left, rule| {
        let right = build_mongo_filter_condition(rule)?;
        Ok::<_, String>(Some(match (left, rule.conjunction) {
            (Some(left), MongoFilterConjunction::Or) => serde_json::json!({"$or": [left, right]}),
            (Some(left), MongoFilterConjunction::And) => {
                serde_json::json!({"$and": [left, right]})
            }
            (None, _) => right,
        }))
    })?;
    match (manual, structured) {
        (Some(manual), Some(structured)) => Ok(serde_json::json!({"$and": [manual, structured]})),
        (Some(manual), None) => Ok(manual),
        (None, Some(structured)) => Ok(structured),
        (None, None) => Ok(serde_json::json!({})),
    }
}

fn canonical(value: Bson) -> serde_json::Value {
    value.into_canonical_extjson()
}

fn parse_mongo_filter_value(rule: &MongoFilterRule) -> Result<serde_json::Value, String> {
    let raw = rule.value.trim();
    let field = &rule.field;
    let parsed = |what: &str| {
        shell_value::parse_value(raw, NumberTyping::Filter)
            .map_err(|error| format!("{field} must be {what}: {error}"))
    };
    match rule.value_type {
        // Typed text (`5`, `true`, `ISODate("…")`, `{"$oid": "…"}`) is read as
        // what it says; anything else is a string.
        MongoFilterValueType::Auto => Ok(shell_value::parse_value(raw, NumberTyping::Filter)
            .map(canonical)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))),
        MongoFilterValueType::String => Ok(serde_json::Value::String(
            serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string()),
        )),
        MongoFilterValueType::Number => match parsed("a number")? {
            number @ (Bson::Int32(_) | Bson::Int64(_)) => Ok(canonical(number)),
            Bson::Double(value) if value.is_finite() => Ok(canonical(Bson::Double(value))),
            _ => Err(format!("{field} must be a finite number")),
        },
        MongoFilterValueType::Boolean => match raw.to_ascii_lowercase().as_str() {
            "true" => Ok(serde_json::Value::Bool(true)),
            "false" => Ok(serde_json::Value::Bool(false)),
            _ => Err(format!("{field} must be true or false")),
        },
        MongoFilterValueType::ObjectId => {
            let hex = raw
                .strip_prefix("ObjectId(")
                .and_then(|rest| rest.strip_suffix(')'))
                .map_or(raw, |inner| inner.trim().trim_matches(['"', '\'']));
            if hex.len() != 24 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{field} must be a 24-character ObjectId"));
            }
            Ok(serde_json::json!({"$oid": hex.to_ascii_lowercase()}))
        }
        MongoFilterValueType::Date => {
            let text = raw
                .strip_prefix("ISODate(")
                .and_then(|rest| rest.strip_suffix(')'))
                .map_or(raw, |inner| inner.trim().trim_matches(['"', '\'']));
            shell_value::parse_iso_date(text)
                .map(|date| canonical(Bson::DateTime(date)))
                .ok_or_else(|| format!("{field} must be a date such as 2024-09-01T00:00:00Z"))
        }
        MongoFilterValueType::Int32 => raw
            .parse::<i32>()
            .map(|value| canonical(Bson::Int32(value)))
            .map_err(|_| format!("{field} must be a 32-bit integer")),
        MongoFilterValueType::Int64 => raw
            .parse::<i64>()
            .map(|value| canonical(Bson::Int64(value)))
            .map_err(|_| format!("{field} must be a 64-bit integer")),
        MongoFilterValueType::Decimal128 => raw
            .parse::<bson::Decimal128>()
            .map(|value| canonical(Bson::Decimal128(value)))
            .map_err(|_| format!("{field} must be a decimal number")),
        MongoFilterValueType::Json => parsed("valid JSON or shell syntax").map(canonical),
    }
}

fn regex_escape(value: &str) -> String {
    value
        .chars()
        .fold(String::with_capacity(value.len()), |mut escaped, ch| {
            if matches!(
                ch,
                '.' | '+' | '*' | '?' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            ) {
                escaped.push('\\');
            }
            escaped.push(ch);
            escaped
        })
}

impl MongoDocumentSnapshot {
    pub fn new(key: MongoDocumentKey, document: serde_json::Value) -> Self {
        let identity = document.get("_id").cloned();
        Self {
            key,
            json: pretty_json(&document),
            document,
            identity,
            mutation_identity: None,
        }
    }
}

/// Operation id of a staged (not yet committed) Mongo mutation. It never
/// reaches a driver: the commit path replaces it with an id allocated by the
/// driver host (`DriverSession::begin_operation`, ids start at 1) right before
/// the request is sent, so staged requests cannot collide with an early cancel
/// remembered by the driver for a reused id (row index, tab id).
pub const STAGED_OPERATION_ID: engine_driver_api::OperationId = engine_driver_api::OperationId(0);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MongoInspectorState {
    pub selected: Option<MongoDocumentKey>,
    pub snapshot: Option<Arc<MongoDocumentSnapshot>>,
    pub loading: bool,
    pub error: Option<String>,
    pub expanded_paths: HashSet<String>,
    /// The Tree view's selected row, as `MongoTreeRow::path`.
    pub selected_path: Option<String>,
}

impl MongoInspectorState {
    pub fn select(&mut self, key: MongoDocumentKey) {
        self.selected = Some(key);
        self.snapshot = None;
        self.loading = true;
        self.error = None;
        self.expanded_paths.clear();
        self.selected_path = None;
    }

    pub fn accept_snapshot(&mut self, snapshot: MongoDocumentSnapshot) -> bool {
        if self.selected != Some(snapshot.key) {
            return false;
        }
        self.snapshot = Some(Arc::new(snapshot));
        self.loading = false;
        self.error = None;
        true
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MongoTreeRow {
    pub path: String,
    pub key: String,
    pub value: String,
    /// Native/inferred type label. `Borrowed` for everything derived from the
    /// value itself; callers may override top-level rows with driver types.
    pub type_name: Cow<'static, str>,
    pub depth: usize,
    pub container: bool,
    pub kind: MongoTreeRowKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MongoTreeRowKind {
    Scalar,
    Container,
}

pub fn pretty_json(document: &serde_json::Value) -> Arc<str> {
    Arc::from(serde_json::to_string_pretty(document).unwrap_or_default())
}

pub fn flatten_tree(
    document: &serde_json::Value,
    expanded_paths: &HashSet<String>,
) -> Vec<MongoTreeRow> {
    let mut rows = Vec::new();
    append_tree_rows(document, "", expanded_paths, 0, &mut rows);
    rows
}

pub fn apply_staged_updates(
    document: &serde_json::Value,
    row_index: usize,
    history: &MongoMutationHistory,
) -> serde_json::Value {
    let mut document = document.clone();
    let serde_json::Value::Object(fields) = &mut document else {
        return document;
    };
    for mutation in history.active() {
        match mutation {
            PendingMongoMutation::Update { request, row, .. } if *row == row_index => {
                if let Some(updates) = request
                    .payload
                    .get("$set")
                    .and_then(|value| value.as_object())
                {
                    for (field, value) in updates {
                        fields.insert(field.clone(), value.clone());
                    }
                }
            }
            PendingMongoMutation::Unset { row, field, .. } if *row == row_index => {
                fields.shift_remove(field);
            }
            _ => {}
        }
    }
    document
}

/// The value at a `MongoTreeRow::path` and the field names leading to it
/// (array items by index). `None` when the document has no such row.
pub fn tree_node<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Option<(Vec<String>, &'a serde_json::Value)> {
    let mut keys = Vec::new();
    let mut value = document;
    let mut rest = path;
    while !rest.is_empty() {
        rest = rest.strip_prefix('/')?;
        // A name ends the path or is followed by the next `/`.
        let at = |key: &str| {
            rest.strip_prefix(key)
                .filter(|tail| tail.is_empty() || tail.starts_with('/'))
        };
        let (key, child, tail) = match value {
            serde_json::Value::Object(fields) => fields
                .iter()
                .find_map(|(key, child)| at(key).map(|tail| (key.clone(), child, tail)))?,
            serde_json::Value::Array(items) => items
                .iter()
                .enumerate()
                .find_map(|(index, child)| {
                    let key = index.to_string();
                    at(&key).map(|tail| (key, child, tail))
                })?,
            _ => return None,
        };
        keys.push(key);
        value = child;
        rest = tail;
    }
    Some((keys, value))
}

/// What a scalar tree row copies: strings without quotes, an ObjectId as its
/// hex, a date as ISO text, a binary in full. `None` for containers, which
/// copy as JSON.
pub fn tree_scalar_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            Some(value.to_string())
        }
        _ => {
            let (display, type_name) = ejson_scalar(value)?;
            Some(match type_name {
                // The row shows a binary cut short; the copy is whole.
                "binary" => crate::cell_value::shell_text(value, false).unwrap_or(display),
                _ => display,
            })
        }
    }
}

fn append_tree_rows(
    value: &serde_json::Value,
    parent_path: &str,
    expanded_paths: &HashSet<String>,
    depth: usize,
    rows: &mut Vec<MongoTreeRow>,
) {
    let children = match value {
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect::<Vec<_>>(),
        serde_json::Value::Array(values) => values
            .iter()
            .enumerate()
            .map(|(index, value)| (index.to_string(), value))
            .collect::<Vec<_>>(),
        _ => return,
    };
    for (key, value) in children {
        let path = format!("{parent_path}/{key}");
        let scalar = ejson_scalar(value);
        let container = scalar.is_none() && (value.is_object() || value.is_array());
        let expanded = container && expanded_paths.contains(&path);
        let (display, type_name) = match scalar {
            Some((display, type_name)) => (display, type_name),
            None => (tree_value_summary(value), json_type_name(value)),
        };
        rows.push(MongoTreeRow {
            path: path.clone(),
            key,
            value: display,
            type_name: Cow::Borrowed(type_name),
            depth,
            container,
            kind: if container {
                MongoTreeRowKind::Container
            } else {
                MongoTreeRowKind::Scalar
            },
        });
        if expanded {
            append_tree_rows(value, &path, expanded_paths, depth + 1, rows);
        }
    }
}

fn tree_value_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(fields) => format!("{{{} fields}}", fields.len()),
        serde_json::Value::Array(values) => format!("[{} items]", values.len()),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Type label for a plain JSON value (Extended JSON wrappers are handled by
/// [`ejson_scalar`]). Uses the same vocabulary as the driver native types so
/// badge colors stay consistent between top-level and nested rows.
pub fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "document",
    }
}

/// Recognizes canonical/relaxed MongoDB Extended JSON scalar wrappers and
/// returns `(display text, type label)`. An object is a wrapper only when it
/// has exactly the wrapper key, so ordinary documents that merely contain a
/// `$`-prefixed field are still expandable containers.
pub fn ejson_scalar(value: &serde_json::Value) -> Option<(String, &'static str)> {
    let serde_json::Value::Object(fields) = value else {
        return None;
    };
    if fields.len() != 1 {
        return None;
    }
    let (key, inner) = fields.iter().next()?;
    match key.as_str() {
        "$oid" => Some((inner.as_str()?.to_string(), "object_id")),
        "$numberLong" => Some((inner.as_str()?.to_string(), "int64")),
        "$numberInt" => Some((inner.as_str()?.to_string(), "int32")),
        "$numberDouble" => {
            let text = inner.as_str()?;
            let mut display = String::new();
            match text.parse::<f64>() {
                Ok(value) => {
                    let _ = engine_mongo_script::shell_value::write_double(&mut display, value);
                }
                Err(_) => display.push_str(text),
            }
            Some((display, "double"))
        }
        "$numberDecimal" => Some((inner.as_str()?.to_string(), "decimal128")),
        "$date" => {
            let millis = match inner {
                serde_json::Value::String(iso) => {
                    return Some((iso.clone(), "date_time"));
                }
                serde_json::Value::Number(millis) => millis.as_i64(),
                serde_json::Value::Object(wrapper) if wrapper.len() == 1 => wrapper
                    .get("$numberLong")
                    .and_then(|value| value.as_str())
                    .and_then(|value| value.parse::<i64>().ok()),
                _ => None,
            }?;
            let display = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis)
                .map(|date| date.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
                .unwrap_or_else(|| millis.to_string());
            Some((display, "date_time"))
        }
        "$binary" => {
            let binary = inner.as_object()?;
            let base64 = binary.get("base64")?.as_str()?;
            let mut display = String::new();
            match bson::Bson::try_from(value.clone()) {
                Ok(bson::Bson::Binary(binary)) => {
                    let _ = engine_mongo_script::shell_value::write_binary(
                        &mut display,
                        u8::from(binary.subtype),
                        &binary.bytes,
                        Some(64),
                    );
                }
                _ => display = format!("<binary {} bytes>", base64_decoded_len(base64)),
            }
            Some((display, "binary"))
        }
        "$timestamp" => {
            let timestamp = inner.as_object()?;
            let seconds = u32::try_from(timestamp.get("t")?.as_u64()?).ok()?;
            let increment = u32::try_from(timestamp.get("i")?.as_u64()?).ok()?;
            let mut display = String::new();
            let _ = engine_mongo_script::shell_value::write_timestamp(
                &mut display,
                seconds,
                increment,
            );
            Some((display, "timestamp"))
        }
        "$symbol" => Some((format!("Symbol({})", inner.as_str()?), "symbol")),
        "$code" => Some((format!("Code({})", inner.as_str()?), "javascript")),
        "$minKey" => Some(("MinKey()".into(), "min_key")),
        "$maxKey" => Some(("MaxKey()".into(), "max_key")),
        "$undefined" => Some(("undefined".into(), "undefined")),
        "$regularExpression" => {
            let regex = inner.as_object()?;
            let pattern = regex.get("pattern")?.as_str()?;
            let options = regex
                .get("options")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            Some((format!("/{pattern}/{options}"), "regex"))
        }
        _ => None,
    }
}

fn base64_decoded_len(base64: &str) -> usize {
    let trimmed = base64.trim_end_matches('=');
    trimmed.len() * 3 / 4
}

/// Row fields of the current selection, read from the driver on a background
/// executor, so event handlers and detail builders on the UI thread never call
/// into the driver. Keyed by the result page and the page-local source row; a
/// hit hands out a shared `Arc` (no allocation, no copy of the document).
#[derive(Debug, Clone, Default)]
pub struct RowFieldsCache {
    page_id: Option<u64>,
    rows: std::collections::HashMap<u64, Arc<engine_driver_api::RowFields>>,
    /// Insertion order, oldest first, for eviction.
    order: std::collections::VecDeque<u64>,
    /// The selection `(page, first row, last row)` already fetched or being
    /// fetched, so an unchanged selection costs one comparison per frame.
    requested: Option<(u64, usize, usize)>,
    /// The selection whose failed read was already retried once.
    retried: Option<(u64, usize, usize)>,
}

impl RowFieldsCache {
    /// Most rows kept (and prefetched for one selection).
    pub const CAPACITY: usize = 512;

    pub fn get(&self, page_id: u64, source_row: u64) -> Option<&Arc<engine_driver_api::RowFields>> {
        if self.page_id != Some(page_id) {
            return None;
        }
        self.rows.get(&source_row)
    }

    /// Whether rows `first..=last` of `page_id` still have to be requested:
    /// `false` when that exact selection already was. Read-only: the request
    /// is recorded by [`Self::mark_requested`] once a read is actually started,
    /// so a frame that could not start one (no session yet) asks again.
    pub fn wants(&self, page_id: u64, first: usize, last: usize) -> bool {
        self.requested != Some((page_id, first, last))
    }

    /// Records that rows `first..=last` of `page_id` are fetched or being fetched.
    pub fn mark_requested(&mut self, page_id: u64, first: usize, last: usize) {
        self.requested = Some((page_id, first, last));
    }

    /// A read of the requested selection left rows unread: forgets the request
    /// so the next frame reads again. Once per selection; the retry's own
    /// failure stands until the selection changes (no read-fail-read loop).
    pub fn read_failed(&mut self, page_id: u64, first: usize, last: usize) {
        let selection = Some((page_id, first, last));
        if self.requested == selection && self.retried != selection {
            self.retried = selection;
            self.requested = None;
        }
    }

    /// Source rows of `rows` that are not cached yet.
    pub fn missing(&self, page_id: u64, rows: impl Iterator<Item = u64>) -> Vec<u64> {
        rows.filter(|row| self.get(page_id, *row).is_none()).collect()
    }

    /// Stores fetched rows. A page change (a new result) drops the old page's
    /// rows first; the oldest rows go when the cache is full.
    pub fn insert(&mut self, page_id: u64, fetched: impl IntoIterator<Item = (u64, engine_driver_api::RowFields)>) {
        if self.page_id != Some(page_id) {
            self.page_id = Some(page_id);
            self.rows.clear();
            self.order.clear();
        }
        for (source_row, fields) in fetched {
            if self.rows.insert(source_row, Arc::new(fields)).is_none() {
                self.order.push_back(source_row);
            }
            while self.order.len() > Self::CAPACITY {
                if let Some(oldest) = self.order.pop_front() {
                    self.rows.remove(&oldest);
                }
            }
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.rows.len()
    }
}

#[derive(Debug, Clone, Default)]
pub struct MongoResultState {
    pub view: MongoResultView,
    pub document_view: MongoDocumentView,
    pub filter: String,
    pub sort: String,
    pub project: String,
    pub known_columns: Vec<String>,
    pub visible_columns: HashSet<String>,
    pub projection_columns: HashSet<String>,
    pub projection_draft_columns: HashSet<String>,
    pub column_open: bool,
    pub column_tab: MongoColumnTab,
    pub column_search: String,
    pub find_expanded: bool,
    pub sort_expanded: bool,
    pub query_preview_open: bool,
    /// FILTER share of the toolbar query capsule (0.15..=0.85). SORT gets the rest.
    pub filter_sort_ratio: f64,
    pub filter_rules: Vec<MongoFilterRule>,
    pub applied_filter: Option<serde_json::Value>,
    pub filter_error: Option<String>,
    pub filter_open: bool,
    pub filter_menu: Option<MongoFilterMenu>,
    pub inspector: MongoInspectorState,
    /// The tab re-ran because the driver dropped its page; cleared once a
    /// document of the new page loads, so a page that is stale again does not
    /// loop.
    pub stale_refresh: bool,
    pub mutations: MongoMutationHistory,
    pub commit_status: MongoCommitStatus,
    pub row_fields: RowFieldsCache,
}

impl MongoResultState {
    pub fn new() -> Self {
        Self {
            filter: "{}".into(),
            sort: r#"{"_id":1}"#.into(),
            filter_sort_ratio: 0.5,
            ..Default::default()
        }
    }

    pub fn observe_result_columns(&mut self, columns: impl IntoIterator<Item = String>) {
        let result_columns = columns.into_iter().collect::<Vec<_>>();
        for name in &result_columns {
            if !self.known_columns.iter().any(|known| known == name) {
                self.known_columns.push(name.clone());
            }
        }

        if !result_columns.is_empty() {
            if self.visible_columns.is_empty() {
                self.visible_columns = result_columns.iter().cloned().collect();
            } else {
                self.visible_columns =
                    selection_for_columns(&self.visible_columns, &result_columns);
            }
        }

        if self.project.trim().is_empty() {
            self.projection_columns = self.known_columns.iter().cloned().collect();
            self.projection_draft_columns = self.projection_columns.clone();
        } else if self.projection_columns.is_empty() {
            self.projection_columns =
                selection_for_columns(&self.projection_columns, &self.known_columns);
            self.projection_draft_columns = self.projection_columns.clone();
        }
    }

    pub fn visible_source_indices(&self, source_columns: &[String]) -> Vec<usize> {
        let visible = source_columns
            .iter()
            .enumerate()
            .filter_map(|(index, name)| self.visible_columns.contains(name).then_some(index))
            .collect::<Vec<_>>();
        if visible.is_empty() && !source_columns.is_empty() {
            vec![0]
        } else {
            visible
        }
    }

    pub fn set_visible_column(&mut self, name: &str, selected: bool) {
        if selected {
            self.visible_columns.insert(name.into());
        } else if self.visible_columns.len() > 1 {
            self.visible_columns.remove(name);
        }
    }

    pub fn reset_projection_draft(&mut self) {
        self.projection_draft_columns = self.projection_columns.clone();
    }

    pub fn set_projection_draft_column(&mut self, name: &str, selected: bool) {
        if selected {
            self.projection_draft_columns.insert(name.into());
        } else if self.projection_draft_columns.len() > 1 {
            self.projection_draft_columns.remove(name);
        }
    }

    pub fn apply_projection_draft(&mut self) -> Option<String> {
        let selected = self
            .known_columns
            .iter()
            .filter(|name| self.projection_draft_columns.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        if selected.is_empty() {
            return None;
        }
        self.projection_columns = selected.iter().cloned().collect();
        self.projection_draft_columns = self.projection_columns.clone();
        self.project = mongo_projection_json(&selected);
        Some(self.project.clone())
    }

    pub fn reset_documents(&mut self) {
        self.inspector.reset();
    }

    pub fn set_find_expanded(&mut self, expanded: bool) {
        self.find_expanded = expanded;
    }

    pub fn set_sort_expanded(&mut self, expanded: bool) {
        self.sort_expanded = expanded;
    }

    pub fn set_query_preview_open(&mut self, open: bool) {
        self.query_preview_open = open;
    }

    pub fn set_filter_sort_ratio(&mut self, ratio: f64) {
        self.filter_sort_ratio = ratio.clamp(0.15, 0.85);
    }

    pub fn filter_sort_ratio_clamped(&self) -> f64 {
        if self.filter_sort_ratio <= 0.0 {
            0.5
        } else {
            self.filter_sort_ratio.clamp(0.15, 0.85)
        }
    }

    pub fn ensure_filter_rule(&mut self, field: impl Into<String>) {
        if self.filter_rules.is_empty() {
            self.filter_rules.push(MongoFilterRule::new(field));
        }
    }

    pub fn add_filter_rule(&mut self, field: impl Into<String>) {
        let id = self
            .filter_rules
            .iter()
            .map(|rule| rule.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut rule = MongoFilterRule::new(field);
        rule.id = id;
        self.filter_rules.push(rule);
    }

    pub fn mark_filter_draft(&mut self) {
        self.applied_filter = None;
        self.filter_error = None;
        self.filter_menu = None;
    }

    pub fn clear_filters(&mut self) {
        self.filter = "{}".into();
        self.filter_rules.clear();
        self.applied_filter = None;
        self.filter_error = None;
        self.filter_menu = None;
    }

    #[cfg(test)]
    pub fn toggle_filter_menu(&mut self, menu: MongoFilterMenu) {
        self.filter_menu = (self.filter_menu != Some(menu)).then_some(menu);
    }

    pub fn close_filter_menu(&mut self) {
        self.filter_menu = None;
    }

    pub fn toggle_document_view(&mut self) {
        self.document_view = match self.document_view {
            MongoDocumentView::Json => MongoDocumentView::Tree,
            MongoDocumentView::Tree => MongoDocumentView::Json,
        };
    }

    pub fn has_pending_mutations(&self) -> bool {
        !self.mutations.is_empty()
    }
}

fn selection_for_columns(
    selected: &HashSet<String>,
    ordered_columns: &[String],
) -> HashSet<String> {
    let selection = ordered_columns
        .iter()
        .filter(|name| selected.contains(*name))
        .cloned()
        .collect::<HashSet<_>>();
    if !selection.is_empty() {
        return selection;
    }
    ordered_columns.first().cloned().into_iter().collect()
}

pub fn mongo_query_preview(
    collection: &str,
    filter: &str,
    sort: &str,
    project: &str,
) -> Option<String> {
    let collection = collection.trim();
    if collection.is_empty() {
        return None;
    }

    let filter = mongo_preview_document(filter, "{}");
    let sort = mongo_preview_document(sort, "");
    let project = mongo_preview_document(project, "");
    let mut query = format!(
        "db{}.find({}",
        mongo_collection_reference(collection),
        filter
    );
    if !project.is_empty() {
        query.push_str(", ");
        query.push_str(&project);
    }
    query.push(')');
    if !sort.is_empty() {
        query.push_str(".sort(");
        query.push_str(&sort);
        query.push(')');
    }
    Some(query)
}

fn mongo_collection_reference(collection: &str) -> String {
    let mut chars = collection.chars();
    let is_identifier = chars
        .next()
        .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
    if is_identifier {
        format!(".{collection}")
    } else {
        format!(
            "[{}]",
            serde_json::to_string(collection).expect("collection serialization cannot fail")
        )
    }
}

fn mongo_preview_document(value: &str, default: &str) -> String {
    let value = value.trim();
    if value.is_empty() || value == "{}" {
        default.to_string()
    } else {
        value.to_string()
    }
}

pub fn mongo_projection_json(selected_columns: &[String]) -> String {
    let mut fields = Vec::with_capacity(selected_columns.len() + 1);
    if !selected_columns.iter().any(|name| name == "_id") {
        fields.push(r#""_id":0"#.into());
    }
    fields.extend(selected_columns.iter().map(|name| {
        format!(
            "{}:1",
            serde_json::to_string(name).expect("field name serialization cannot fail")
        )
    }));
    format!("{{{}}}", fields.join(","))
}

pub fn duplicate_document_without_id(document: &serde_json::Value) -> Option<serde_json::Value> {
    let serde_json::Value::Object(mut document) = document.clone() else {
        return None;
    };
    document.remove("_id");
    Some(serde_json::Value::Object(document))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_node_resolves_row_paths_and_the_copy_text_of_scalars() {
        let document = serde_json::json!({
            "_id": {"$oid": "000000000000000000000002"},
            "logs": [{"status": "ok"}, {"status": "a b", "a/b": 1}],
            "name": "x\"y",
            "bin": {"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn8=", "subType": "00"}},
        });
        let (keys, value) = tree_node(&document, "/logs/1/status").unwrap();
        assert_eq!(keys, ["logs", "1", "status"]);
        assert_eq!(value, "a b");
        let (keys, _) = tree_node(&document, "/logs/1/a/b").unwrap();
        assert_eq!(keys, ["logs", "1", "a/b"], "a name with a slash resolves too");
        assert!(tree_node(&document, "/logs/2").is_none());
        assert!(tree_node(&document, "/nam").is_none(), "a name must match whole");
        assert_eq!(tree_node(&document, "").unwrap().0.len(), 0);

        assert_eq!(tree_scalar_text(&document["name"]).as_deref(), Some("x\"y"), "no quotes, no escapes");
        assert_eq!(tree_scalar_text(&document["_id"]).as_deref(), Some("000000000000000000000002"));
        assert_eq!(tree_scalar_text(&document["logs"]), None, "containers copy as JSON");
        let shown = ejson_scalar(&document["bin"]).unwrap().0;
        let copied = tree_scalar_text(&document["bin"]).unwrap();
        assert!(copied.len() > shown.len(), "the row's cut-short binary copies whole: {shown} / {copied}");
    }

    #[test]
    fn mongo_query_preview_builds_find_sort_and_projection() {
        assert_eq!(
            mongo_query_preview("users", "", "{}", "").as_deref(),
            Some("db.users.find({})")
        );
        assert_eq!(
            mongo_query_preview(
                "sales-orders",
                "{id: \"abc\"}",
                "{createdAt: -1}",
                "{\"name\":1}"
            )
            .as_deref(),
            Some("db[\"sales-orders\"].find({id: \"abc\"}, {\"name\":1}).sort({createdAt: -1})")
        );
    }

    #[test]
    fn mongo_query_preview_escapes_collection_names_and_rejects_empty_collection() {
        assert_eq!(
            mongo_query_preview("a\\b\"c", "{}", "", "").as_deref(),
            Some("db[\"a\\\\b\\\"c\"].find({})")
        );
        assert_eq!(mongo_query_preview("", "{}", "", ""), None);
    }

    #[test]
    fn mongo_columns_preserve_source_order_and_fallback_to_one_column() {
        let mut state = MongoResultState::new();
        let source_columns = vec!["_id".into(), "name".into(), "status".into()];

        state.observe_result_columns(source_columns.clone());
        assert_eq!(state.visible_source_indices(&source_columns), vec![0, 1, 2]);

        state.visible_columns.remove("name");
        assert_eq!(state.visible_source_indices(&source_columns), vec![0, 2]);

        state.visible_columns.clear();
        assert_eq!(state.visible_source_indices(&source_columns), vec![0]);
    }

    #[test]
    fn mongo_projection_json_escapes_field_names() {
        let columns = vec![r#"a"b"#.to_string(), "name".to_string()];

        assert_eq!(
            mongo_projection_json(&columns),
            r#"{"_id":0,"a\"b":1,"name":1}"#
        );
        assert_eq!(
            mongo_projection_json(&["_id".into(), "name".into()]),
            r#"{"_id":1,"name":1}"#
        );
    }

    #[test]
    fn empty_results_preserve_visible_column_selection() {
        let mut state = MongoResultState::new();
        state.observe_result_columns(vec!["_id".into(), "name".into(), "status".into()]);
        state.visible_columns.remove("name");

        state.observe_result_columns(Vec::<String>::new());

        assert_eq!(
            state.visible_columns,
            HashSet::from(["_id".into(), "status".into()])
        );
    }

    #[test]
    fn mongo_column_selection_keeps_one_visible_column_and_applies_projection_in_order() {
        let mut state = MongoResultState::new();
        let source_columns = vec!["_id".into(), "name".into(), "status".into()];
        state.observe_result_columns(source_columns);

        state.set_visible_column("name", false);
        state.set_visible_column("_id", false);
        state.set_visible_column("status", false);
        assert_eq!(state.visible_columns, HashSet::from(["status".into()]));

        state.reset_projection_draft();
        state.set_projection_draft_column("name", false);
        assert_eq!(
            state.apply_projection_draft(),
            Some(r#"{"_id":1,"status":1}"#.into())
        );
    }

    fn request(kind: &str) -> MutationRequest {
        MutationRequest {
            operation_id: engine_driver_api::OperationId(1),
            kind: kind.into(),
            database: Some("app".into()),
            object: Some("users".into()),
            identity: serde_json::json!({"_id": {"$oid": "66d4f84d0000000000000003"}}),
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn mongo_state_defaults_to_table_with_empty_filter_and_id_sort() {
        let state = MongoResultState::new();

        assert_eq!(state.view, MongoResultView::Table);
        assert_eq!(state.document_view, MongoDocumentView::Json);
        assert_eq!(state.filter, "{}");
        assert_eq!(state.sort, r#"{"_id":1}"#);
    }

    #[test]
    fn mongo_find_editor_can_toggle_expanded_state() {
        let mut state = MongoResultState::new();

        assert!(!state.find_expanded);
        state.set_find_expanded(true);
        assert!(state.find_expanded);
        state.set_find_expanded(false);
        assert!(!state.find_expanded);
    }

    #[test]
    fn mongo_document_view_toggle_remembers_json_or_tree() {
        let mut state = MongoResultState::new();

        state.toggle_document_view();
        assert_eq!(state.document_view, MongoDocumentView::Tree);

        state.view = MongoResultView::Table;
        state.view = MongoResultView::Document;
        assert_eq!(state.document_view, MongoDocumentView::Tree);

        state.toggle_document_view();
        assert_eq!(state.document_view, MongoDocumentView::Json);
    }

    #[test]
    fn filter_sort_ratio_is_clamped() {
        let mut state = MongoResultState::new();
        assert_eq!(state.filter_sort_ratio_clamped(), 0.5);

        state.set_filter_sort_ratio(2.0);
        assert_eq!(state.filter_sort_ratio_clamped(), 0.85);

        state.set_filter_sort_ratio(-1.0);
        assert_eq!(state.filter_sort_ratio_clamped(), 0.15);

        state.set_filter_sort_ratio(0.6);
        assert_eq!(state.filter_sort_ratio_clamped(), 0.6);
    }

    #[test]
    fn filter_menu_toggle_is_exclusive_and_can_close() {
        let mut state = MongoResultState::new();

        state.toggle_filter_menu(MongoFilterMenu::Field(1));
        assert_eq!(state.filter_menu, Some(MongoFilterMenu::Field(1)));

        state.toggle_filter_menu(MongoFilterMenu::Mode(1));
        assert_eq!(state.filter_menu, Some(MongoFilterMenu::Mode(1)));

        state.close_filter_menu();
        assert_eq!(state.filter_menu, None);
    }

    #[test]
    fn structured_filter_builds_typed_object_id_condition() {
        let rule = MongoFilterRule::new("_id");
        let rule = MongoFilterRule {
            value_type: MongoFilterValueType::ObjectId,
            value: "66d4f84d0000000000000003".into(),
            ..rule
        };

        assert_eq!(
            build_mongo_filter_condition(&rule).expect("object id filter"),
            serde_json::json!({"_id": {"$oid": "66d4f84d0000000000000003"}})
        );
    }

    #[test]
    fn structured_filter_escapes_contains_values() {
        let rule = MongoFilterRule {
            field: "name".into(),
            mode: MongoFilterMode::Contains,
            value: "Ada.+".into(),
            ..MongoFilterRule::new("name")
        };

        assert_eq!(
            build_mongo_filter_condition(&rule).expect("contains filter"),
            serde_json::json!({"name": {"$regex": "Ada\\.\\+", "$options": "i"}})
        );
    }

    #[test]
    fn structured_filters_merge_with_manual_filter_using_and() {
        let rule = MongoFilterRule {
            field: "status".into(),
            value: "active".into(),
            ..MongoFilterRule::new("status")
        };

        assert_eq!(
            merge_mongo_filters(r#"{"tenant":"acme"}"#, &[rule]).expect("merged filter"),
            serde_json::json!({
                "$and": [
                    {"tenant": "acme"},
                    {"status": "active"}
                ]
            })
        );
    }

    #[test]
    fn structured_filter_rejects_invalid_typed_values() {
        let rule = MongoFilterRule {
            value_type: MongoFilterValueType::Boolean,
            value: "maybe".into(),
            ..MongoFilterRule::new("active")
        };

        assert!(build_mongo_filter_condition(&rule).is_err());
    }

    #[test]
    fn history_supports_undo_redo_and_discards_redo_after_new_stage() {
        let mut history = MongoMutationHistory::default();
        history.stage(PendingMongoMutation::Insert {
            request: request("insert"),
            document: serde_json::json!({"name": "Ada"}),
        });
        history.stage(PendingMongoMutation::Delete {
            request: request("delete"),
            row: 2,
        });

        assert!(history.undo());
        assert!(history.can_redo());
        history.stage(PendingMongoMutation::Insert {
            request: request("insert"),
            document: serde_json::json!({"name": "Grace"}),
        });
        assert!(!history.can_redo());
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn history_returns_latest_cell_update_and_tracks_deleted_rows() {
        let mut history = MongoMutationHistory::default();
        history.stage(PendingMongoMutation::Update {
            request: request("update"),
            row: 0,
            field: "name".into(),
            value: serde_json::json!("Ada"),
            display_text: "Ada".into(),
        });
        history.stage(PendingMongoMutation::Update {
            request: request("update"),
            row: 0,
            field: "name".into(),
            value: serde_json::json!("Grace"),
            display_text: "Grace".into(),
        });
        history.stage(PendingMongoMutation::Delete {
            request: request("delete"),
            row: 3,
        });

        let update = history.update_for(0, "name").expect("latest update");
        assert!(
            matches!(update, PendingMongoMutation::Update { display_text, .. } if display_text == "Grace")
        );
        assert!(history.is_deleted(3));
        assert!(!history.is_deleted(0));
        assert!(history.remove_updates_for(0, "name"));
        assert!(history.update_for(0, "name").is_none());
        assert_eq!(history.len(), 1);
    }

    fn set_of(row: usize, field: &str) -> PendingMongoMutation {
        PendingMongoMutation::Update { request: request("update"), row, field: field.into(), value: serde_json::json!(1), display_text: "1".into() }
    }

    fn unset_of(row: usize, field: &str) -> PendingMongoMutation {
        PendingMongoMutation::Unset { request: request("update"), row, field: field.into() }
    }

    #[test]
    fn a_group_of_changes_is_one_undo_step_and_one_redo_step() {
        let mut history = MongoMutationHistory::default();
        history.stage(set_of(0, "before"));
        history.stage_group([set_of(1, "a"), set_of(1, "b"), unset_of(1, "c")]);
        history.stage(set_of(2, "after"));
        assert_eq!(history.len(), 5);
        // After first, then the three, then one: undone one step at a time.
        assert!(history.undo());
        assert_eq!(history.len(), 4);
        assert!(history.undo());
        assert_eq!(history.len(), 1, "the three come back together");
        assert!(history.can_undo() && history.can_redo());
        assert!(history.redo());
        assert_eq!(history.len(), 4, "and go again together");
        assert!(history.redo() && !history.redo());
        assert_eq!(history.len(), 5);
        assert!(history.undo() && history.undo() && history.undo());
        assert!(history.is_empty() && !history.undo());
        // An empty group is nothing: no step, no change of the revision.
        let revision = history.revision();
        history.stage_group(std::iter::empty());
        assert_eq!((history.len(), history.revision()), (0, revision));
        // Staging after an undo drops the redo tail, groups included.
        history.stage_group([set_of(3, "x"), set_of(3, "y")]);
        history.undo();
        history.stage(set_of(4, "z"));
        assert!(!history.can_redo());
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn a_commit_that_stops_inside_a_group_keeps_the_rest_of_it_as_one_step() {
        let mut history = MongoMutationHistory::default();
        history.stage(set_of(0, "first"));
        history.stage_group([set_of(1, "a"), set_of(1, "b"), set_of(1, "c")]);
        history.remove_committed_prefix(2);
        assert_eq!(history.len(), 2, "b and c are left");
        assert!(history.undo());
        assert!(history.is_empty(), "the rest of the group is one step");
        assert!(!history.undo());
        assert!(history.redo() && history.len() == 2 && !history.redo());
    }

    #[test]
    fn taking_the_staged_updates_of_a_field_away_keeps_the_other_steps_whole() {
        let mut history = MongoMutationHistory::default();
        history.stage_group([set_of(0, "name"), set_of(0, "other")]);
        history.stage(set_of(0, "name"));
        history.stage_group([set_of(1, "x"), set_of(1, "y")]);
        assert!(history.remove_updates_for(0, "name"));
        assert_eq!(history.len(), 3, "other, x and y are left");
        assert!(history.undo());
        assert_eq!(history.len(), 1, "x and y are one step");
        assert!(history.undo());
        assert!(history.is_empty(), "other is the step before; the one with only `name` is gone");
        assert!(history.redo() && history.redo() && !history.redo());
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn rollback_clears_active_and_redo_entries() {
        let mut history = MongoMutationHistory::default();
        history.stage(PendingMongoMutation::Insert {
            request: request("insert"),
            document: serde_json::json!({}),
        });
        history.undo();
        history.rollback();

        assert!(history.is_empty());
        assert!(!history.can_redo());
    }

    #[test]
    fn duplicate_removes_only_top_level_id() {
        let duplicate = duplicate_document_without_id(
            &serde_json::json!({"_id": 1, "name": "Ada", "nested": {"_id": 2}}),
        )
        .expect("document");

        assert_eq!(
            duplicate,
            serde_json::json!({"name": "Ada", "nested": {"_id": 2}})
        );
    }

    #[test]
    fn selecting_document_invalidates_previous_snapshot() {
        let first = MongoDocumentKey {
            page_id: 7,
            row_index: 1,
        };
        let second = MongoDocumentKey {
            page_id: 7,
            row_index: 2,
        };
        let mut inspector = MongoInspectorState::default();

        inspector.select(first);
        assert!(inspector.accept_snapshot(MongoDocumentSnapshot::new(
            first,
            serde_json::json!({"name": "Ada"}),
        )));
        inspector.select(second);

        assert_eq!(inspector.selected, Some(second));
        assert!(inspector.snapshot.is_none());
        assert!(inspector.loading);
    }

    #[test]
    fn stale_snapshot_is_rejected_after_selection_changes() {
        let selected = MongoDocumentKey {
            page_id: 9,
            row_index: 4,
        };
        let stale = MongoDocumentKey {
            page_id: 9,
            row_index: 3,
        };
        let mut inspector = MongoInspectorState::default();
        inspector.select(selected);

        assert!(!inspector.accept_snapshot(MongoDocumentSnapshot::new(
            stale,
            serde_json::json!({"stale": true}),
        )));
        assert!(inspector.snapshot.is_none());
        assert!(inspector.loading);
    }

    #[test]
    fn history_revision_changes_with_every_active_set_change() {
        let update = |row: usize| PendingMongoMutation::Update {
            request: request("update"),
            row,
            field: "name".into(),
            value: serde_json::json!("x"),
            display_text: "x".into(),
        };
        let mut history = MongoMutationHistory::default();
        let mut seen = vec![history.revision()];
        let expect_change = |history: &MongoMutationHistory, seen: &mut Vec<u64>| {
            let revision = history.revision();
            assert_ne!(Some(&revision), seen.last(), "revision must change");
            seen.push(revision);
        };

        history.stage(update(0));
        expect_change(&history, &mut seen);
        history.stage(update(1));
        expect_change(&history, &mut seen);
        assert!(history.undo());
        expect_change(&history, &mut seen);
        assert!(history.redo());
        expect_change(&history, &mut seen);
        assert!(history.remove_updates_for(1, "name"));
        expect_change(&history, &mut seen);
        history.remove_committed_prefix(1);
        expect_change(&history, &mut seen);
        history.stage(update(2));
        expect_change(&history, &mut seen);
        history.rollback();
        expect_change(&history, &mut seen);

        // Failed operations and reads leave it alone; equality ignores it.
        let before = history.revision();
        assert!(!history.undo());
        assert!(!history.redo());
        assert!(!history.remove_updates_for(9, "missing"));
        assert_eq!(history.revision(), before);
        assert_eq!(history, MongoMutationHistory::default());
    }

    #[test]
    fn snapshot_pretty_json_is_generated_once_from_the_document() {
        let snapshot = MongoDocumentSnapshot::new(
            MongoDocumentKey {
                page_id: 1,
                row_index: 0,
            },
            serde_json::json!({"name": "Ada", "active": true}),
        );

        assert_eq!(
            snapshot.json.as_ref(),
            "{\n  \"name\": \"Ada\",\n  \"active\": true\n}"
        );
    }

    #[test]
    fn tree_rows_distinguish_scalars_from_containers() {
        let rows = flatten_tree(
            &serde_json::json!({"profile": {"name": "Ada"}, "active": true}),
            &HashSet::new(),
        );

        assert_eq!(rows[0].kind, MongoTreeRowKind::Container);
        assert_eq!(rows[1].kind, MongoTreeRowKind::Scalar);
    }

    #[test]
    fn expanded_tree_rows_include_children_without_brace_rows() {
        let rows = flatten_tree(
            &serde_json::json!({"profile": {"name": "Ada"}}),
            &HashSet::from(["/profile".to_string()]),
        );

        assert_eq!(
            rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
            vec![MongoTreeRowKind::Container, MongoTreeRowKind::Scalar]
        );
        assert_eq!(rows[0].value, "{1 fields}");
        assert_eq!(rows[1].value, "\"Ada\"");
    }

    #[test]
    fn tree_flattening_hides_collapsed_descendants() {
        let document = serde_json::json!({
            "wallet": {
                "currency": "VND",
                "owner": {"name": "Ada"},
            },
            "active": true,
        });
        let rows = flatten_tree(&document, &Default::default());

        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            vec!["/wallet", "/active"]
        );
    }

    #[test]
    fn tree_flattening_includes_only_expanded_descendants() {
        let document = serde_json::json!({
            "wallet": {
                "currency": "VND",
                "owner": {"name": "Ada"},
            },
        });
        let expanded = HashSet::from(["/wallet".to_string(), "/wallet/owner".to_string()]);
        let rows = flatten_tree(&document, &expanded);

        assert_eq!(
            rows.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
            vec![
                "/wallet",
                "/wallet/currency",
                "/wallet/owner",
                "/wallet/owner/name",
            ]
        );
    }

    #[test]
    fn ejson_wrappers_become_typed_scalars() {
        use serde_json::json;
        let cases = [
            (json!({"$oid": "68a1"}), "68a1", "object_id"),
            (
                json!({"$date": {"$numberLong": "1752033379687"}}),
                "2025-07-09T03:56:19.687Z",
                "date_time",
            ),
            (
                json!({"$date": "2025-07-09T03:56:19Z"}),
                "2025-07-09T03:56:19Z",
                "date_time",
            ),
            (json!({"$date": 0}), "1970-01-01T00:00:00Z", "date_time"),
            (json!({"$numberLong": "42"}), "42", "int64"),
            (json!({"$numberInt": "7"}), "7", "int32"),
            (json!({"$numberDouble": "1.5"}), "1.5", "double"),
            (json!({"$numberDecimal": "9.25"}), "9.25", "decimal128"),
            (json!({"$numberDouble": "5.0"}), "5.0", "double"),
            (json!({"$numberDouble": "NaN"}), "NaN", "double"),
            (json!({"$numberDouble": "1000000000000000000000.0"}), "1e21", "double"),
            (
                json!({"$binary": {"base64": "aGVsbG8=", "subType": "00"}}),
                "BinData(0, 'aGVsbG8=')",
                "binary",
            ),
            (
                json!({"$binary": {"base64": "AAECAwQFBgcICQoLDA0ODw==", "subType": "04"}}),
                "UUID('00010203-0405-0607-0809-0a0b0c0d0e0f')",
                "binary",
            ),
            (
                json!({"$timestamp": {"t": 1700000000_u64, "i": 3_u64}}),
                "Timestamp({ t: 1700000000, i: 3 })",
                "timestamp",
            ),
            (json!({"$minKey": 1}), "MinKey()", "min_key"),
            (json!({"$undefined": true}), "undefined", "undefined"),
            (
                json!({"$regularExpression": {"pattern": "^a.*", "options": "i"}}),
                "/^a.*/i",
                "regex",
            ),
        ];
        for (value, display, type_name) in cases {
            assert_eq!(
                ejson_scalar(&value),
                Some((display.to_string(), type_name)),
                "{value}"
            );
        }
    }

    #[test]
    fn ejson_requires_exactly_the_wrapper_key() {
        use serde_json::json;
        assert!(ejson_scalar(&json!({"$oid": "68a1", "extra": 1})).is_none());
        assert!(ejson_scalar(&json!({"$oid": 5})).is_none());
        assert!(ejson_scalar(&json!({"name": "Ada"})).is_none());
        assert!(ejson_scalar(&json!({"$date": {"$numberLong": "1", "x": 2}})).is_none());
        assert!(ejson_scalar(&json!("$oid")).is_none());
        assert!(ejson_scalar(&json!([1])).is_none());
    }

    #[test]
    fn tree_renders_ejson_wrappers_as_non_expandable_scalars() {
        let document = serde_json::json!({
            "_id": {"$oid": "68a1"},
            "createdAt": {"$date": {"$numberLong": "1752033379687"}},
            "orders": [{"total": {"$numberInt": "3"}}],
        });
        let expanded = HashSet::from(["/createdAt".to_string(), "/_id".to_string()]);
        let rows = flatten_tree(&document, &expanded);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].value, "68a1");
        assert_eq!(rows[0].type_name, "object_id");
        assert_eq!(rows[1].value, "2025-07-09T03:56:19.687Z");
        assert_eq!(rows[1].type_name, "date_time");
        assert!(rows[0..2].iter().all(|row| !row.container));
        assert_eq!(rows[2].value, "[1 items]");
        assert_eq!(rows[2].type_name, "array");
        assert!(rows[2].container);
    }

    #[test]
    fn tree_expands_arrays_of_documents_with_typed_children() {
        let document = serde_json::json!({
            "orders": [{"id": {"$numberLong": "9"}, "note": "hi"}],
        });
        let expanded = HashSet::from(["/orders".to_string(), "/orders/0".to_string()]);
        let rows = flatten_tree(&document, &expanded);

        assert_eq!(
            rows.iter()
                .map(|row| (row.key.as_str(), row.depth, row.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("orders", 0, "[1 items]"),
                ("0", 1, "{2 fields}"),
                ("id", 2, "9"),
                ("note", 2, "\"hi\""),
            ]
        );
        assert_eq!(rows[2].type_name, "int64");
        assert_eq!(rows[3].type_name, "string");
    }

    #[test]
    fn staged_updates_change_only_the_selected_document_snapshot() {
        let mut history = MongoMutationHistory::default();
        history.stage(PendingMongoMutation::Update {
            request: request("update"),
            row: 1,
            field: "name".into(),
            value: serde_json::json!("Grace"),
            display_text: "Grace".into(),
        });

        let document = serde_json::json!({"name": "Ada", "active": true});
        assert_eq!(
            apply_staged_updates(&document, 1, &history),
            serde_json::json!({"name": "Ada", "active": true})
        );
        assert_eq!(
            apply_staged_updates(
                &document,
                1,
                &MongoMutationHistory {
                    revision: Default::default(),
                    entries: vec![PendingMongoMutation::Update {
                        request: MutationRequest {
                            payload: serde_json::json!({"$set": {"name": "Grace"}}),
                            ..request("update")
                        },
                        row: 1,
                        field: "name".into(),
                        value: serde_json::json!("Grace"),
                        display_text: "Grace".into(),
                    }],
                    steps: vec![1],
                    cursor: 1,
                },
            ),
            serde_json::json!({"name": "Grace", "active": true})
        );
    }

    #[test]
    fn a_staged_unset_removes_the_field_in_order_with_the_sets_around_it() {
        let set = |value: serde_json::Value| PendingMongoMutation::Update {
            request: MutationRequest {
                payload: serde_json::json!({"$set": {"name": value.clone()}}),
                ..request("update")
            },
            row: 2,
            field: "name".into(),
            value,
            display_text: String::new(),
        };
        let unset = |row: usize| PendingMongoMutation::Unset {
            request: MutationRequest {
                payload: serde_json::json!({"$unset": {"name": ""}}),
                ..request("update")
            },
            row,
            field: "name".into(),
        };
        let document = serde_json::json!({"name": "Ada", "active": true});
        let mut history = MongoMutationHistory::default();
        history.stage(unset(2));
        history.stage(unset(7));
        assert_eq!(
            apply_staged_updates(&document, 2, &history),
            serde_json::json!({"active": true}),
            "only the selected row loses the field"
        );
        assert_eq!(apply_staged_updates(&document, 3, &history), document);
        history.stage(set(serde_json::json!("Grace")));
        assert_eq!(
            apply_staged_updates(&document, 2, &history),
            serde_json::json!({"name": "Grace", "active": true}),
            "a later set brings the field back"
        );
        assert!(history.undo());
        assert_eq!(
            apply_staged_updates(&document, 2, &history),
            serde_json::json!({"active": true}),
            "undo restores the unset"
        );
    }

    #[test]
    fn a_staged_unset_keeps_the_order_of_the_remaining_fields() {
        let document = serde_json::json!({"a": 1, "b": 2, "c": 3, "d": 4});
        let mut history = MongoMutationHistory::default();
        history.stage(PendingMongoMutation::Unset {
            request: MutationRequest {
                payload: serde_json::json!({"$unset": {"b": ""}}),
                ..request("update")
            },
            row: 0,
            field: "b".into(),
        });
        let preview = apply_staged_updates(&document, 0, &history);
        assert_eq!(
            preview.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["a", "c", "d"],
            "removing a middle field does not move the last one into its place"
        );
        assert!(history.is_unset(0, "b") && !history.is_unset(0, "a") && !history.is_unset(1, "b"));
    }
}
