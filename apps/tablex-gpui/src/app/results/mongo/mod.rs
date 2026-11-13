use super::super::*;

pub(in crate::app) mod controls;
pub(in crate::app) mod document_edit;
pub(in crate::app) mod editors;
pub(in crate::app) mod mutations;
pub(in crate::app) mod popovers;
pub(in crate::app) mod query;
pub(in crate::app) mod toolbar;
mod tree_text;
pub(in crate::app) mod view;

pub(in crate::app) fn mongo_columns_button_label(
    visible_columns: &HashSet<String>,
    known_columns: &[String],
) -> String {
    columns_button_label(visible_columns.len(), known_columns.len())
}

pub(in crate::app) fn mongo_query_payload(
    filter: &str,
    sort: &str,
    project: &str,
) -> serde_json::Value {
    // Blank filter / sort / projection text means "none": the filter travels as
    // the empty document (a filter is always required), the others as "".
    serde_json::json!({
        "filter": if filter.trim().is_empty() { "{}" } else { filter },
        "sort": sort.trim(),
        "project": project.trim(),
    })
}

pub(in crate::app) fn mongo_query_signature(runtime: &crate::workspace::TabRuntime) -> String {
    mongo_query_signature_from_parts(
        &runtime.mongo.filter,
        &runtime.mongo.sort,
        &runtime.mongo.project,
    )
}

pub(in crate::app) fn mongo_query_signature_from_parts(
    filter: &str,
    sort: &str,
    project: &str,
) -> String {
    format!("{filter}\u{1f}{sort}\u{1f}{project}")
}

/// A typed Mongo cell value that could not be read, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct MongoInputError(String);

impl std::fmt::Display for MongoInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MongoInputError {}

impl From<MongoInputError> for String {
    fn from(error: MongoInputError) -> Self {
        error.0
    }
}

impl MongoInputError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Why a Mongo cell cannot be edited. Each variant says what is wrong and how
/// to fix it; nothing is ever sent for such a cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum MongoEditRefusal {
    /// The row has no `_id` in the page, so no write can be addressed to it.
    NoIdentity,
    /// `_id` is immutable.
    IdField,
    /// `$set` would read the name as a path or an operator.
    FieldName(String),
}

impl std::fmt::Display for MongoEditRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoIdentity => formatter.write_str(
                "This row has no _id in the page (the projection leaves it out), so a write \
                 cannot be addressed to it. Include _id in the projection and run the query again.",
            ),
            Self::IdField => formatter.write_str("_id cannot be changed."),
            Self::FieldName(name) => write!(
                formatter,
                "\"{name}\" has a '.' in its name or starts with '$', which MongoDB reads as a \
                 path or an operator, so the grid does not edit it. Use an update with $setField \
                 in the shell."
            ),
        }
    }
}

/// `None` when `field` of the inspected row can be written; otherwise why not.
/// A field the document lacks (`field_name` absent from the row) is created by
/// the write, so only its name and the row's identity matter.
pub(in crate::app) fn mongo_edit_refusal(
    row: &engine_driver_api::RowFields,
    field_name: &str,
) -> Option<MongoEditRefusal> {
    mongo_field_refusal(!row.mutation_identity.is_null(), field_name)
}

/// [`mongo_edit_refusal`] for a row known only by whether the driver gave it a
/// write identity.
pub(in crate::app) fn mongo_field_refusal(
    has_identity: bool,
    field_name: &str,
) -> Option<MongoEditRefusal> {
    if !has_identity {
        Some(MongoEditRefusal::NoIdentity)
    } else if field_name == "_id" {
        Some(MongoEditRefusal::IdField)
    } else if field_name.is_empty() || field_name.contains('.') || field_name.starts_with('$') {
        Some(MongoEditRefusal::FieldName(field_name.to_string()))
    } else {
        None
    }
}

/// The BSON type label (the driver's `native_type` vocabulary) of `value`.
fn mongo_native_type(value: &bson::Bson) -> &'static str {
    use bson::Bson;
    match value {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "document",
        Bson::Boolean(_) => "boolean",
        Bson::RegularExpression(_) => "regex",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::JavaScriptCodeWithScope(_) => "javascript_scope",
        Bson::Array(_) => "array",
        Bson::Timestamp(_) => "timestamp",
        Bson::Binary(_) => "binary",
        Bson::ObjectId(_) => "object_id",
        Bson::DateTime(_) => "date_time",
        Bson::Decimal128(_) => "decimal128",
        Bson::Symbol(_) => "symbol",
        Bson::DbPointer(_) => "db_pointer",
        Bson::Undefined => "undefined",
        Bson::Null => "null",
        Bson::Int32(_) => "int32",
        Bson::Int64(_) => "int64",
        Bson::MinKey => "min_key",
        Bson::MaxKey => "max_key",
    }
}

/// What the user typed for a cell of BSON type `native_type`, as canonical
/// Extended JSON. The cell keeps its type: a plain number typed into an Int64
/// cell is an Int64, a Double cell takes `5` as `5.0`. Strings are the text as
/// typed. Documents, arrays, null, missing fields and the types with no plain
/// text form (binary, regex, timestamp ...) are written in shell syntax
/// (`{ "a": Long('1') }`, `"text"`, `ObjectId('…')`, `ISODate('…')`, `/re/i`),
/// where a plain number is typed like mongosh types it (Int32 when it is an
/// integer in range, otherwise Double).
pub(in crate::app) fn mongo_value_from_input(
    input: &str,
    native_type: &str,
) -> std::result::Result<serde_json::Value, MongoInputError> {
    use bson::Bson;
    use engine_mongo_script::shell_value::{self, NumberTyping};
    let trimmed = input.trim();
    let native_type = native_type.to_ascii_lowercase();
    let shell = || {
        shell_value::parse_value(trimmed, NumberTyping::Mongosh)
            .map_err(|error| MongoInputError::new(error.to_string()))
    };
    let plain = match native_type.as_str() {
        "string" | "text" => return Ok(serde_json::Value::String(input.to_string())),
        "null" if trimmed.eq_ignore_ascii_case("null") => return Ok(serde_json::Value::Null),
        "null" | "missing" => {
            return shell()
                .map(Bson::into_canonical_extjson)
                .map_err(|error| {
                    MongoInputError::new(format!(
                        "{error}. Write a value in shell syntax: \"text\", 42, 1.5, true, null, \
                         ObjectId('…'), ISODate('…'), Long('5'), {{ \"a\": 1 }}."
                    ))
                });
        }
        "object_id" => {
            let hex = trimmed
                .strip_prefix("ObjectId(")
                .and_then(|value| value.strip_suffix(')'))
                .map_or(trimmed, |inner| inner.trim().trim_matches(['"', '\'']));
            bson::oid::ObjectId::parse_str(hex)
                .map(Bson::ObjectId)
                .map_err(|error| MongoInputError::new(error.to_string()))
        }
        "bool" | "boolean" => trimmed
            .parse::<bool>()
            .map(Bson::Boolean)
            .map_err(|_| MongoInputError::new("Boolean values must be true or false")),
        "int32" => trimmed
            .parse::<i32>()
            .map(Bson::Int32)
            .map_err(|_| MongoInputError::new("Value must fit a 32-bit signed integer")),
        "int64" => trimmed
            .parse::<i64>()
            .map(Bson::Int64)
            .map_err(|_| MongoInputError::new("Value must fit a 64-bit signed integer")),
        "double" => trimmed
            .parse::<f64>()
            .map(Bson::Double)
            .map_err(|_| MongoInputError::new("Numeric value is invalid")),
        "decimal128" => trimmed
            .parse::<bson::Decimal128>()
            .map(Bson::Decimal128)
            .map_err(|error| MongoInputError::new(error.to_string())),
        "date_time" => shell_value::parse_iso_date(trimmed)
            .map(Bson::DateTime)
            .ok_or_else(|| {
                MongoInputError::new("Date must look like 2024-09-01T00:00:00.000Z")
            }),
        _ => shell(),
    };
    let value = match plain {
        Ok(value) => value,
        // `Long('7')` or `ISODate('…')` typed into a cell of that type.
        Err(plain_error) => match shell() {
            Ok(value) if mongo_native_type(&value) == native_type => value,
            _ => return Err(plain_error),
        },
    };
    if mongo_native_type(&value) != native_type {
        return Err(MongoInputError::new(format!(
            "Value must retain BSON type {native_type}"
        )));
    }
    Ok(value.into_canonical_extjson())
}

/// The text the cell editors and the Value box show for `field`: the stored
/// value in the form [`mongo_value_from_input`] reads back to the identical
/// BSON value and type. Scalars are plain text (the cell's type is kept), a
/// Double always has a decimal point or an exponent, everything else is shell
/// syntax.
pub(in crate::app) fn mongo_detail_value(field: &engine_driver_api::RowField) -> String {
    let value = &field.value;
    let wrapped = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
    let plain = match field.native_type.as_str() {
        "string" => value.as_str().map(str::to_string),
        "boolean" | "bool" => value.as_bool().map(|value| value.to_string()),
        "int32" => wrapped("$numberInt").map(str::to_string),
        "int64" => wrapped("$numberLong").map(str::to_string),
        "decimal128" => wrapped("$numberDecimal").map(str::to_string),
        "object_id" => wrapped("$oid").map(str::to_string),
        "double" => wrapped("$numberDouble")
            .and_then(|text| text.parse::<f64>().ok())
            .map(|number| {
                let mut text = String::new();
                let _ = engine_mongo_script::shell_value::write_double(&mut text, number);
                text
            }),
        "date_time" => value
            .get("$date")
            .and_then(|date| date.get("$numberLong"))
            .and_then(serde_json::Value::as_str)
            .and_then(|millis| millis.parse::<i64>().ok())
            .and_then(engine_mongo_script::shell_value::iso_date_text),
        _ => None,
    };
    plain
        .or_else(|| crate::cell_value::shell_text(value, true))
        .unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default())
}

/// The editor text of a duplicated row: the document without `_id`, in shell
/// syntax, so every field keeps its BSON type when the copy is read back (an
/// Int64 stays an Int64, a negative Int32 an Int32, a NaN a NaN).
pub(in crate::app) fn mongo_duplicate_text(fields: &[engine_driver_api::RowField]) -> Option<String> {
    let document = serde_json::Value::Object(
        fields
            .iter()
            .map(|field| (field.name.clone(), field.value.clone()))
            .collect(),
    );
    let document = crate::mongo_result::duplicate_document_without_id(&document)?;
    crate::cell_value::shell_text(&document, true)
}

/// Compact single-line display of a Mongo field value: Extended JSON wrappers
/// collapse to their typed scalar and containers to `{N fields}` / `[N items]`
/// (the Row tab tree expands them lazily, so no pretty-printing here).
pub(in crate::app) fn mongo_json_display_value(value: &serde_json::Value) -> String {
    if let Some((display, _)) = crate::mongo_result::ejson_scalar(value) {
        return display;
    }
    match value {
        serde_json::Value::Null => "NULL".into(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Object(fields) => format!("{{{} fields}}", fields.len()),
        serde_json::Value::Array(values) => format!("[{} items]", values.len()),
        value => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// Whether `input` is a document the driver will read: JSON5, shell
/// constructors and Extended JSON are all accepted. Blank text is valid: an
/// empty filter, sort or projection means "none" (see [`mongo_query_payload`]).
pub(in crate::app) fn mongo_json_object_is_valid(input: &str) -> bool {
    input.trim().is_empty()
        || engine_mongo_script::shell_value::parse_document(
            input,
            engine_mongo_script::shell_value::NumberTyping::Filter,
        )
        .is_ok()
}

/// What a grid cell shows while its field is staged for `$unset`.
pub(in crate::app) const UNSET_DISPLAY: &str = "(unset)";

pub(in crate::app) fn rebuild_mongo_pending_overlay(runtime: &mut crate::workspace::TabRuntime) {
    runtime.pending_deleted_rows.clear();
    runtime
        .pending_deleted_rows
        .extend(
            runtime
                .mongo
                .mutations
                .active()
                .iter()
                .filter_map(|mutation| match mutation {
                    PendingMongoMutation::Delete { row, .. } => Some(*row),
                    _ => None,
                }),
        );
    let source_columns = (0..runtime.grid.column_count())
        .filter_map(|column| runtime.grid.column_name(column).map(str::to_string))
        .collect::<Vec<_>>();
    let visible_source_columns = if runtime.mongo.known_columns.is_empty() {
        (0..source_columns.len()).collect::<Vec<_>>()
    } else {
        runtime.mongo.visible_source_indices(&source_columns)
    };
    let pending = runtime
        .mongo
        .mutations
        .active()
        .iter()
        .filter_map(|mutation| {
            let (row, field, display_text) = match mutation {
                PendingMongoMutation::Update {
                    row,
                    field,
                    display_text,
                    ..
                } => (row, field, display_text.as_str()),
                PendingMongoMutation::Unset { row, field, .. } => (row, field, UNSET_DISPLAY),
                _ => return None,
            };
            let source_column = (0..runtime.grid.column_count())
                .find(|column| runtime.grid.column_name(*column) == Some(field.as_str()))?;
            let column = visible_source_columns
                .iter()
                .position(|column| *column == source_column)?;
            Some((CellPosition { row: *row, column }, Arc::<str>::from(display_text)))
        })
        .collect::<Vec<_>>();

    runtime.clear_pending_cell_texts();
    for (position, text) in pending {
        runtime.set_pending_cell_text(position, text);
    }
}

/// The one place a write's filter is checked on the app side too: exactly
/// `{ "_id": <value> }`, never anything else.
fn check_mongo_identity(identity: &serde_json::Value) -> Result<(), MongoInputError> {
    if identity
        .as_object()
        .is_none_or(|fields| fields.len() != 1 || !fields.contains_key("_id"))
    {
        return Err(MongoInputError::new(MongoEditRefusal::NoIdentity.to_string()));
    }
    Ok(())
}

pub(in crate::app) fn build_inline_mongo_update(
    payload: &InlineEditPayload,
    row: usize,
    text: &str,
) -> Result<crate::mongo_result::PendingMongoMutation, MongoInputError> {
    let InlineEditPayload::Mongo {
        native_type,
        identity,
        database,
        collection,
        field,
    } = payload
    else {
        return Err(MongoInputError::new("not a mongo payload"));
    };
    check_mongo_identity(identity)?;
    let value = mongo_value_from_input(text, native_type)?;
    let scalar_text = matches!(
        native_type.as_str(),
        "string" | "text" | "boolean" | "bool" | "int32" | "int64" | "double" | "decimal128"
            | "object_id" | "date_time"
    );
    let display_text = if value.is_null() {
        "NULL".to_string()
    } else if scalar_text {
        text.to_string()
    } else {
        crate::cell_value::shell_text(&value, false).unwrap_or_else(|| text.to_string())
    };
    let request = engine_driver_api::MutationRequest {
        operation_id: crate::mongo_result::STAGED_OPERATION_ID,
        kind: "update".into(),
        database: Some(database.clone()),
        object: Some(collection.clone()),
        identity: identity.clone(),
        payload: serde_json::json!({"$set": {field.clone(): value.clone()}}),
    };
    Ok(crate::mongo_result::PendingMongoMutation::Update {
        request,
        row,
        field: field.clone(),
        value,
        display_text,
    })
}

pub(in crate::app) fn mongo_filter_element_id(
    prefix: &'static str,
    tab_id: u64,
    rule_id: u64,
) -> gpui::ElementId {
    gpui::ElementId::from((gpui::ElementId::from((prefix, tab_id)), rule_id.to_string()))
}

pub(in crate::app) fn mongo_filter_dropdown_button(
    id: gpui::ElementId,
    label: impl Into<gpui::SharedString>,
    width: gpui::Pixels,
    label_selector: impl Into<String>,
) -> Button {
    let label_selector = label_selector.into();
    Button::new(id)
        .with_size(gpui_component::Size::XSmall)
        .compact()
        .outline()
        .dropdown_caret(true)
        .h(px(32.0))
        .w(width)
        .child(
            div()
                .debug_selector(move || label_selector)
                .w(width - px(28.0))
                .flex_shrink_0()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_left()
                .child(label.into()),
        )
}

pub(in crate::app) fn mongo_filter_mode_label(
    mode: crate::mongo_result::MongoFilterMode,
) -> &'static str {
    match mode {
        crate::mongo_result::MongoFilterMode::Equals => "Equals",
        crate::mongo_result::MongoFilterMode::NotEquals => "Does not equal",
        crate::mongo_result::MongoFilterMode::Contains => "Contains",
        crate::mongo_result::MongoFilterMode::NotContains => "Does not contain",
        crate::mongo_result::MongoFilterMode::GreaterThan => "Greater than",
        crate::mongo_result::MongoFilterMode::GreaterThanOrEqual => "Greater than or equal",
        crate::mongo_result::MongoFilterMode::LessThan => "Less than",
        crate::mongo_result::MongoFilterMode::LessThanOrEqual => "Less than or equal",
        crate::mongo_result::MongoFilterMode::IsNull => "Is NULL",
        crate::mongo_result::MongoFilterMode::IsNotNull => "Is not NULL",
    }
}

pub(in crate::app) fn mongo_filter_type_label(
    value_type: crate::mongo_result::MongoFilterValueType,
) -> &'static str {
    match value_type {
        crate::mongo_result::MongoFilterValueType::Auto => "Auto",
        crate::mongo_result::MongoFilterValueType::String => "String",
        crate::mongo_result::MongoFilterValueType::Number => "Number",
        crate::mongo_result::MongoFilterValueType::Boolean => "Boolean",
        crate::mongo_result::MongoFilterValueType::ObjectId => "ObjectId",
        crate::mongo_result::MongoFilterValueType::Date => "Date",
        crate::mongo_result::MongoFilterValueType::Int32 => "Int32",
        crate::mongo_result::MongoFilterValueType::Int64 => "Int64",
        crate::mongo_result::MongoFilterValueType::Decimal128 => "Decimal128",
        crate::mongo_result::MongoFilterValueType::Json => "JSON",
    }
}

pub(in crate::app) fn mongo_filter_conjunction_label(
    conjunction: crate::mongo_result::MongoFilterConjunction,
) -> &'static str {
    match conjunction {
        crate::mongo_result::MongoFilterConjunction::And => "AND",
        crate::mongo_result::MongoFilterConjunction::Or => "OR",
    }
}

/// Tree value color from the row's BSON type, shared with the cell-detail
/// tree. Guessing from the display text colored an all-digit ObjectId as a
/// number and a hex one as plain text.
pub(in crate::app) fn mongo_tree_value_color(type_name: &str, container: bool) -> gpui::Rgba {
    super::detail_view::row_tree_value_color(type_name, container)
}

/// Single-line label for a document's `_id` in the document list: ObjectId as
/// plain hex (the convention shared with the relaxed JSON views), Extended JSON
/// wrappers by their scalar display, everything else verbatim.
pub(in crate::app) fn mongo_document_preview(value: &str) -> String {
    let value = value.trim();
    let preview = if let Some(object_id) = value
        .strip_prefix("ObjectId(")
        .and_then(|value| value.strip_suffix(')'))
    {
        object_id.to_string()
    } else if value.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(value)
            .ok()
            .and_then(|json| crate::mongo_result::ejson_scalar(&json))
            .map(|(display, _)| display)
            .unwrap_or_else(|| value.to_string())
    } else {
        value.to_string()
    };
    let mut chars = preview.chars();
    let truncated = chars.by_ref().take(48).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

/// Mongo toolbar editors, keyed by tab id (entities that live as long as the tab).
#[derive(Default)]
pub(in crate::app) struct MongoUiState {
    pub(in crate::app) filter_editors: HashMap<u64, gpui::Entity<InputState>>,
    pub(in crate::app) sort_editors: HashMap<u64, gpui::Entity<InputState>>,
    pub(in crate::app) column_search_editors: HashMap<u64, gpui::Entity<InputState>>,
    pub(in crate::app) expanded_find_editors: HashMap<u64, gpui::Entity<EditorState>>,
    pub(in crate::app) expanded_sort_editors: HashMap<u64, gpui::Entity<EditorState>>,
    pub(in crate::app) filter_field_editors: HashMap<(u64, u64), gpui::Entity<InputState>>,
    pub(in crate::app) filter_value_editors: HashMap<(u64, u64), gpui::Entity<InputState>>,
}

/// Mongo toolbar / document-view state one tab's `TabView` owns.
#[derive(Default)]
pub(in crate::app) struct MongoTabUi {
    pub(in crate::app) find_enter_held: bool,
    pub(in crate::app) sort_enter_held: bool,
    pub(in crate::app) filter_sort_drag: Option<FilterSortDragState>,
    pub(in crate::app) filter_trigger_bounds:
        std::rc::Rc<std::cell::RefCell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// Shell-syntax / canonical Extended JSON in the document view's JSON mode.
    pub(in crate::app) document_json_mode: crate::cell_value::EjsonMode,
    /// The open document editor of the Document view (see `document_edit`).
    pub(in crate::app) document_edit: Option<document_edit::MongoDocumentEdit>,
}

#[derive(Clone, Copy)]
pub(in crate::app) struct FilterSortDragState {
    pub(in crate::app) start_x: f32,
    pub(in crate::app) start_ratio: f64,
    pub(in crate::app) viewport_width: f32,
}

impl AppRoot {
    pub(in crate::app) fn begin_filter_sort_resize(
        &mut self,
        tab_id: u64,
        start_x: gpui::Pixels,
        window: &Window,
    ) {
        let start_ratio = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter_sort_ratio_clamped())
            .unwrap_or(0.5);
        self.view_mut(tab_id).mongo.filter_sort_drag = Some(FilterSortDragState {
            start_x: f32::from(start_x),
            start_ratio,
            viewport_width: f32::from(window.viewport_size().width),
        });
    }

    pub(in crate::app) fn update_filter_sort_resize(
        &mut self,
        tab_id: u64,
        current_x: gpui::Pixels,
    ) {
        let Some(drag) = self.view(tab_id).mongo.filter_sort_drag else {
            return;
        };
        // GPUI does not expose capsule width during dragging; use viewport width.
        let capsule_width = (drag.viewport_width - 500.0).max(300.0);
        let ratio =
            drag.start_ratio + f64::from((f32::from(current_x) - drag.start_x) / capsule_width);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.mongo.set_filter_sort_ratio(ratio);
        }
    }

    pub(in crate::app) fn end_filter_sort_resize(&mut self, tab_id: u64) {
        self.view_mut(tab_id).mongo.filter_sort_drag = None;
    }

    pub(in crate::app) fn render_mongo_find_popover(
        &mut self,
        tab_id: u64,
        open: bool,
        can_run: bool,
        editor: Option<gpui::Entity<EditorState>>,
        trigger: Button,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = cx.entity().downgrade();
        let panel = if let Some(editor) = editor {
            let editor_for_key = editor.clone();
            let editor_for_enter = editor.clone();
            let editor_area = div()
                .debug_selector(|| "mongo-find-editor-area".into())
                .flex_1()
                .min_h(px(0.0))
                .p(px(10.0))
                .key_context("mongo-find")
                .on_action(cx.listener(move |this, _: &MongoFindEnter, _, cx| {
                    // An open completion menu owns Enter: let the editor accept the item.
                    if editor_for_enter.read(cx).completion_menu_state().open {
                        cx.propagate();
                        return;
                    }
                    if can_run && !this.view(tab_id).mongo.find_enter_held {
                        this.view_mut(tab_id).mongo.find_enter_held = true;
                        this.run_tab_query(tab_id, cx);
                    }
                }))
                .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, _, _| {
                    if event.keystroke.key == "enter" {
                        this.view_mut(tab_id).mongo.find_enter_held = false;
                    }
                }))
                .capture_key_down(cx.listener(
                    move |_this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.is_held || event.keystroke.key != "enter" {
                            return;
                        }
                        cx.stop_propagation();
                        if event.keystroke.modifiers.shift {
                            editor_for_key.update(cx, |state, cx| state.replace("\n", window, cx));
                        }
                    },
                ))
                .child(Editor::new(&editor).appearance(true).h_full());
            let close = Button::new(("mongo-find-close", tab_id))
                .label("Close")
                .compact()
                .ghost()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_mongo_find_expanded(tab_id, false, window, cx);
                }));
            let execute = Button::new(("mongo-find-execute", tab_id))
                .label("Execute")
                .compact()
                .primary()
                .disabled(!can_run)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_tab_query(tab_id, cx);
                }));
            div()
                .id(("mongo-find-panel", tab_id))
                .debug_selector(|| "mongo-find-panel".into())
                .w(px(520.0))
                .h(px(220.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(theme::surface())
                .child(
                    div()
                        .debug_selector(|| "mongo-find-header".into())
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(13.0)).child("Find Query"))
                        .child(close),
                )
                .child(div().h(px(1.0)).w_full().bg(theme::border()))
                .child(editor_area)
                .child(
                    div()
                        .debug_selector(|| "mongo-find-footer".into())
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_t_1()
                        .border_color(theme::border())
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Enter executes. Shift+Enter inserts a newline."),
                        )
                        .child(execute),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };
        Popover::new(("mongo-find-popover", tab_id))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, window, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_mongo_find_expanded(tab_id, *open, window, cx);
                });
            })
            .child(panel)
            .into_any_element()
    }

    pub(in crate::app) fn render_mongo_sort_popover(
        &mut self,
        tab_id: u64,
        open: bool,
        can_run: bool,
        editor: Option<gpui::Entity<EditorState>>,
        trigger: Button,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = cx.entity().downgrade();
        let panel = if let Some(editor) = editor {
            let editor_for_key = editor.clone();
            let editor_for_enter = editor.clone();
            let editor_area = div()
                .debug_selector(|| "mongo-sort-editor-area".into())
                .flex_1()
                .min_h(px(0.0))
                .p(px(10.0))
                .key_context("mongo-sort")
                .on_action(cx.listener(move |this, _: &MongoSortEnter, _, cx| {
                    // An open completion menu owns Enter: let the editor accept the item.
                    if editor_for_enter.read(cx).completion_menu_state().open {
                        cx.propagate();
                        return;
                    }
                    if can_run && !this.view(tab_id).mongo.sort_enter_held {
                        this.view_mut(tab_id).mongo.sort_enter_held = true;
                        this.run_tab_query(tab_id, cx);
                    }
                }))
                .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, _, _| {
                    if event.keystroke.key == "enter" {
                        this.view_mut(tab_id).mongo.sort_enter_held = false;
                    }
                }))
                .capture_key_down(cx.listener(
                    move |_this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.is_held || event.keystroke.key != "enter" {
                            return;
                        }
                        cx.stop_propagation();
                        if event.keystroke.modifiers.shift {
                            editor_for_key.update(cx, |state, cx| state.replace("\n", window, cx));
                        }
                    },
                ))
                .child(Editor::new(&editor).appearance(true).h_full());
            let close = Button::new(("mongo-sort-close", tab_id))
                .label("Close")
                .compact()
                .ghost()
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_mongo_sort_expanded(tab_id, false, window, cx);
                }));
            let execute = Button::new(("mongo-sort-execute", tab_id))
                .label("Execute")
                .compact()
                .primary()
                .disabled(!can_run)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.run_tab_query(tab_id, cx);
                }));
            div()
                .id(("mongo-sort-panel", tab_id))
                .debug_selector(|| "mongo-sort-panel".into())
                .w(px(520.0))
                .h(px(220.0))
                .flex()
                .flex_col()
                .overflow_hidden()
                .bg(theme::surface())
                .child(
                    div()
                        .debug_selector(|| "mongo-sort-header".into())
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(div().text_size(px(13.0)).child("Sort Query"))
                        .child(close),
                )
                .child(div().h(px(1.0)).w_full().bg(theme::border()))
                .child(editor_area)
                .child(
                    div()
                        .debug_selector(|| "mongo-sort-footer".into())
                        .h(px(40.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_t_1()
                        .border_color(theme::border())
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Enter executes. Shift+Enter inserts a newline."),
                        )
                        .child(execute),
                )
                .into_any_element()
        } else {
            div().into_any_element()
        };
        Popover::new(("mongo-sort-popover", tab_id))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, window, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_mongo_sort_expanded(tab_id, *open, window, cx);
                });
            })
            .child(panel)
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub(in crate::app) fn render_mongo_query_preview_popover(
        &mut self,
        tab_id: u64,
        collection: String,
        filter: String,
        sort: String,
        project: String,
        open: bool,
        trigger: Button,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let command =
            crate::mongo_result::mongo_query_preview(&collection, &filter, &sort, &project);
        let command_for_copy = command.clone();
        let command_text = command
            .clone()
            .unwrap_or_else(|| "No collection selected".into());
        let copy = Button::new(("mongo-query-preview-copy", tab_id))
            .icon(Icon::new(IconName::Copy).size(px(14.0)))
            .compact()
            .ghost()
            .disabled(command.is_none())
            .tooltip("Copy query")
            .on_click(cx.listener(move |_, _, _, cx| {
                if let Some(command) = command_for_copy.clone() {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(command));
                }
            }));
        let close = Button::new(("mongo-query-preview-close", tab_id))
            .icon(Icon::new(IconName::Close).size(px(14.0)))
            .compact()
            .ghost()
            .tooltip("Close preview")
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_mongo_query_preview_open(tab_id, false, cx);
            }));
        let panel = div()
            .id(("mongo-query-panel", tab_id))
            .debug_selector(|| "mongo-query-panel".into())
            .w(px(520.0))
            .h(px(180.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme::surface())
            .child(
                div()
                    .h(px(40.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(13.0)).child("Mongo Query Preview"))
                    .child(
                        div()
                            .debug_selector(|| "mongo-query-preview-close".into())
                            .child(close),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h(px(0.0))
                    .p(px(8.0))
                    .bg(theme::background())
                    .border_1()
                    .border_color(theme::border())
                    .rounded(px(4.0))
                    .child(
                        div()
                            .debug_selector(|| "mongo-query-preview-copy".into())
                            .absolute()
                            .top(px(4.0))
                            .right(px(4.0))
                            .child(copy),
                    )
                    .child(
                        div()
                            .debug_selector(|| "mongo-query-command".into())
                            .id(("mongo-query-code", tab_id))
                            .size_full()
                            .pt(px(30.0))
                            .overflow_x_scroll()
                            .overflow_y_scroll()
                            .font_family("Menlo")
                            .text_size(px(12.0))
                            .line_height(px(18.0))
                            .text_color(theme::text())
                            .whitespace_nowrap()
                            .child(match &command {
                                // Highlighted like the pipeline editors, and selectable so part of
                                // it (the sort, say) can be copied.
                                Some(command) => gpui_component::text::TextView::markdown(
                                    ("mongo-query-code-text", tab_id),
                                    crate::app::assistant::ask::fenced("javascript", command),
                                )
                                .selectable(true)
                                .into_any_element(),
                                None => command_text.into_any_element(),
                            }),
                    ),
            );
        let root = cx.entity().downgrade();
        Popover::new(("mongo-query-preview-popover", tab_id))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, _, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_mongo_query_preview_open(tab_id, *open, cx);
                });
            })
            .child(panel)
            .into_any_element()
    }
}

#[cfg(test)]
mod tree_color_tests {
    use super::mongo_tree_value_color;
    use crate::theme;

    fn colors(document: serde_json::Value) -> Vec<(String, gpui::Rgba)> {
        crate::mongo_result::flatten_tree(&document, &Default::default())
            .into_iter()
            .map(|row| (row.key.clone(), mongo_tree_value_color(&row.type_name, row.container)))
            .collect()
    }

    #[test]
    fn tree_values_are_colored_by_bson_type_not_by_how_the_text_looks() {
        let digits = colors(serde_json::json!({"_id": {"$oid": "000000000000000000000001"}}));
        let hex = colors(serde_json::json!({"_id": {"$oid": "0000000000000000000000ef"}}));
        assert_eq!(digits[0].1, hex[0].1, "every ObjectId gets one color");
        assert_ne!(digits[0].1, theme::warning(), "an all-digit ObjectId is not a number");

        let row = colors(serde_json::json!({
            "s": "123", "n": 5, "b": true, "z": null, "d": {"x": 1}
        }));
        let color = |key: &str| row.iter().find(|(k, _)| k == key).unwrap().1;
        assert_eq!(color("s"), theme::success(), "a numeric-looking string is still a string");
        assert_eq!(color("n"), theme::warning());
        assert_eq!(color("b"), theme::accent());
        assert_eq!(color("z"), theme::text_faint());
        assert_eq!(color("d"), theme::text_muted());
    }
}
