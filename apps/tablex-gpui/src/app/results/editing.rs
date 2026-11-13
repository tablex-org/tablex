use super::super::*;
use super::column_type::{base_type_head, value_kind, ValueKind};

/// Why typed text is not a value of its column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum MutationInputError {
    Integer,
    Decimal,
    Float,
    Boolean,
    Binary,
}

impl std::fmt::Display for MutationInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Integer => "Enter a whole number: digits with an optional sign",
            Self::Decimal => "Enter a number: digits with an optional sign and decimal point, or NaN / Infinity",
            Self::Float => "Enter a number: digits with an optional sign, decimal point and exponent, or NaN / Infinity",
            Self::Boolean => "Boolean values are true, false, t, f, 1 or 0",
            Self::Binary => "Binary values are written 0x followed by an even number of hex digits",
        })
    }
}

impl std::error::Error for MutationInputError {}

impl From<MutationInputError> for String {
    fn from(error: MutationInputError) -> Self {
        error.to_string()
    }
}

/// `NaN`, `Infinity`, `-Infinity` (and `inf`), in any case, with an optional sign.
fn is_special_number(unsigned: &str) -> bool {
    ["nan", "inf", "infinity"]
        .iter()
        .any(|word| unsigned.eq_ignore_ascii_case(word))
}

/// Digits with an optional decimal point (`12`, `12.`, `.5`, `12.50`).
fn is_plain_decimal(text: &str) -> bool {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
    digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
}

fn is_exponent(text: &str) -> bool {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// `0x` followed by an even number of hex digits (`0x` alone is zero bytes).
pub(in crate::app) fn is_hex_bytes(text: &str) -> bool {
    text.strip_prefix("0x").is_some_and(|digits| {
        digits.len() % 2 == 0 && digits.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

/// The value of a stored cell's text in a column of `data_type`, for keys and
/// clones: the kind comes from the BASE type ([`value_kind`]); numbers and
/// booleans are checked but numbers travel as the exact text, everything else
/// is text for the server to coerce. The text is never NULL by its spelling
/// (`null` in a text column is a string): NULL is the cell's null flag.
pub(in crate::app) fn mutation_value_from_cell(
    text: &str,
    data_type: &str,
) -> std::result::Result<MutationValue, MutationInputError> {
    let kind = value_kind(data_type);
    let trimmed = text.trim();
    match kind {
        ValueKind::Integer => {
            let digits = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| MutationValue::Number(trimmed.to_string()))
                .ok_or(MutationInputError::Integer)
        }
        ValueKind::Decimal | ValueKind::Float => {
            let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
            let exponents = kind == ValueKind::Float;
            let (mantissa, exponent) = match unsigned.find(['e', 'E']).filter(|_| exponents) {
                Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
                None => (unsigned, None),
            };
            let valid = is_special_number(unsigned)
                || (is_plain_decimal(mantissa) && exponent.is_none_or(is_exponent));
            valid
                .then(|| MutationValue::Number(trimmed.to_string()))
                .ok_or(if exponents {
                    MutationInputError::Float
                } else {
                    MutationInputError::Decimal
                })
        }
        ValueKind::Boolean => match trimmed.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Ok(MutationValue::Boolean(true)),
            "false" | "f" | "0" => Ok(MutationValue::Boolean(false)),
            _ => Err(MutationInputError::Boolean),
        },
        ValueKind::Binary => is_hex_bytes(trimmed)
            .then(|| MutationValue::Text(trimmed.to_string()))
            .ok_or(MutationInputError::Binary),
        ValueKind::Temporal | ValueKind::Json | ValueKind::Text => {
            Ok(MutationValue::Text(text.to_string()))
        }
    }
}

/// The value typed into an editor of a column of `data_type`: as
/// [`mutation_value_from_cell`], except that an EMPTY editor in a numeric,
/// boolean or date-like column is NULL (the caller refuses it where the column is not nullable). No
/// spelling of NULL makes the text NULL.
pub(in crate::app) fn mutation_value_from_input(
    input: &str,
    data_type: &str,
) -> std::result::Result<MutationValue, MutationInputError> {
    let clears = matches!(
        value_kind(data_type),
        ValueKind::Integer
            | ValueKind::Decimal
            | ValueKind::Float
            | ValueKind::Boolean
            | ValueKind::Temporal
    );
    if clears && input.trim().is_empty() {
        return Ok(MutationValue::Null);
    }
    mutation_value_from_cell(input, data_type)
}

/// Why a text that is not valid UTF-8 is read-only.
pub(in crate::app) const INVALID_UTF8_REASON: &str =
    "This text is not valid UTF-8: it is shown as hex and cannot be edited or cloned.";

/// Why a MySQL GEOMETRY value is read-only.
pub(in crate::app) const GEOMETRY_REASON: &str =
    "Geometry values are shown as WKT text and cannot be edited or cloned.";

/// Why a column the driver delivers as the server's text is read-only.
pub(in crate::app) const SERVER_TEXT_REASON: &str =
    "This column is shown as the server's text: it cannot be edited, cloned or used as a key.";

/// Why a ClickHouse Array, Map, Tuple, JSON, Variant, Dynamic or aggregate
/// column is read-only.
pub(in crate::app) const CLICKHOUSE_COMPOSITE_REASON: &str =
    "ClickHouse Array, Map, Tuple, JSON, Variant, Dynamic and aggregate values cannot be edited, cloned or used as a key here.";

/// The label suffix of a column the driver delivers as the server's own text.
const READ_ONLY_SUFFIX: &str = "(read-only)";

/// Whether `type_name` is a ClickHouse type with no text form to write back.
fn is_clickhouse_composite(type_name: &str) -> bool {
    matches!(
        base_type_head(type_name).as_str(),
        "array"
            | "array[]"
            | "map"
            | "tuple"
            | "json"
            | "variant"
            | "dynamic"
            | "aggregatefunction"
    )
}

/// A result cell in the Row tab's document: the value (numbers that
/// `serde_json` would print differently stay strings of their digits) and the
/// JSON text of the cell, with its numbers and nested JSON exactly as stored.
pub(in crate::app) struct RowJsonCell {
    pub(in crate::app) value: serde_json::Value,
    pub(in crate::app) literal: String,
}

pub(in crate::app) fn row_json_cell(value: &str, type_name: &str, is_null: bool) -> RowJsonCell {
    let text = |value: &str| RowJsonCell {
        value: serde_json::Value::String(value.to_string()),
        literal: serde_json::to_string(value).unwrap_or_default(),
    };
    if is_null {
        return RowJsonCell {
            value: serde_json::Value::Null,
            literal: "null".into(),
        };
    }
    let trimmed = value.trim();
    match value_kind(type_name) {
        ValueKind::Boolean => match trimmed {
            "true" | "t" => RowJsonCell {
                value: true.into(),
                literal: "true".into(),
            },
            "false" | "f" => RowJsonCell {
                value: false.into(),
                literal: "false".into(),
            },
            _ => text(value),
        },
        ValueKind::Integer | ValueKind::Decimal | ValueKind::Float
            if crate::json_text::is_json_number(trimmed) =>
        {
            match crate::json_text::exact_value(trimmed) {
                Some(value) => RowJsonCell {
                    value,
                    literal: trimmed.to_string(),
                },
                None => text(value),
            }
        }
        ValueKind::Json => match crate::json_text::exact_value(value) {
            Some(parsed) => RowJsonCell {
                value: parsed,
                literal: value.trim().to_string(),
            },
            None => text(value),
        },
        _ => text(value),
    }
}

pub(in crate::app) fn row_value_to_json(
    value: &str,
    type_name: &str,
    is_null: bool,
) -> serde_json::Value {
    row_json_cell(value, type_name, is_null).value
}

pub(in crate::app) fn cell_editor_should_float(
    window: &Window,
    text: &str,
    column_width: f32,
) -> bool {
    let available_width = px((column_width - 16.0).max(1.0));
    let font = window.text_style().font();
    text.split('\n').any(|line| {
        if line.is_empty() {
            return false;
        }
        let run = gpui::TextRun {
            len: line.len(),
            font: font.clone(),
            color: theme::text().into(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        window
            .text_system()
            .shape_line(line.to_string().into(), px(12.0), &[run], None)
            .width
            > available_width
    }) || text.contains('\n')
}

#[derive(Debug, Clone)]
pub(in crate::app) enum InlineEditPayload {
    Sql {
        primary_key: crate::catalog::CatalogColumn,
        key_value: MutationValue,
        /// Every primary-key column of the edited row, read from the grid when
        /// the editor opened (`key_value` is the first column's).
        key: crate::workspace::RowKey,
        column_name: String,
        data_type: String,
    },
    Mongo {
        native_type: String,
        identity: serde_json::Value,
        database: String,
        collection: String,
        field: String,
    },
    ReadOnly,
}

pub(in crate::app) struct InlineCellEdit {
    pub(in crate::app) position: CellPosition,
    pub(in crate::app) editor: gpui_component::input::AnyInputState,
    pub(in crate::app) editor_start_text: gpui::SharedString,
    pub(in crate::app) original_text: gpui::SharedString,
    pub(in crate::app) payload: InlineEditPayload,
    pub(in crate::app) floating: bool,
    pub(in crate::app) read_only: bool,
    pub(in crate::app) error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum InlineEditExit {
    Submit,
    FocusLoss,
}

impl InlineEditPayload {
    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    pub(in crate::app) fn store(
        self,
        this: &mut AppRoot,
        tab_id: u64,
        position: CellPosition,
        editor: gpui_component::input::AnyInputState,
        original_text: gpui::SharedString,
        floating: bool,
        read_only: bool,
        window: &mut Window,
        cx: &mut Context<AppRoot>,
    ) {
        let editor_start_text = editor.value(cx);
        match &editor {
            gpui_component::input::AnyInputState::Input(state) => {
                state.update(cx, |state, cx| state.focus(window, cx));
                cx.subscribe_in(
                    state,
                    window,
                    move |this: &mut AppRoot,
                          source: &gpui::Entity<InputState>,
                          event: &gpui_component::input::InputEvent,
                          window: &mut Window,
                          cx: &mut Context<AppRoot>| {
                        this.handle_inline_edit_event(
                            tab_id,
                            source.entity_id(),
                            event,
                            window,
                            cx,
                        );
                    },
                )
                .detach();
            }
            gpui_component::input::AnyInputState::Textarea(state) => {
                state.update(cx, |state, cx| state.focus(window, cx));
                cx.subscribe_in(
                    state,
                    window,
                    move |this: &mut AppRoot,
                          source: &gpui::Entity<TextareaState>,
                          event: &gpui_component::input::InputEvent,
                          window: &mut Window,
                          cx: &mut Context<AppRoot>| {
                        this.handle_inline_edit_event(
                            tab_id,
                            source.entity_id(),
                            event,
                            window,
                            cx,
                        );
                    },
                )
                .detach();
            }
            _ => unreachable!("inline editing uses Input or Textarea"),
        }
        this.view_mut(tab_id).grid_ui.inline_edit = Some(InlineCellEdit {
            position,
            editor,
            editor_start_text,
            original_text,
            payload: self,
            floating,
            read_only,
            error: None,
        });
        cx.notify();
    }
}

impl AppRoot {
    fn handle_inline_edit_event(
        &mut self,
        tab_id: u64,
        source_id: gpui::EntityId,
        event: &gpui_component::input::InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.view(tab_id).grid_ui.inline_edit.as_ref() else {
            return;
        };
        let editor_id = match &edit.editor {
            gpui_component::input::AnyInputState::Input(state) => state.entity_id(),
            gpui_component::input::AnyInputState::Textarea(state) => state.entity_id(),
            _ => return,
        };
        if editor_id != source_id {
            return;
        }
        match event {
            gpui_component::input::InputEvent::Focus => {
                edit.editor.focus_handle(cx).dispatch_action(
                    &gpui_component::input::SelectAll,
                    window,
                    cx,
                );
            }
            gpui_component::input::InputEvent::PressEnter { .. } => {
                self.commit_inline_edit(tab_id, InlineEditExit::Submit, Some(window), cx);
            }
            gpui_component::input::InputEvent::Blur => {
                self.commit_inline_edit(tab_id, InlineEditExit::FocusLoss, None, cx);
            }
            gpui_component::input::InputEvent::Change => {
                if let Some(edit) = self.view_mut(tab_id).grid_ui.inline_edit.as_mut() {
                    if edit.error.take().is_some() {
                        self.notify_tab(tab_id, cx);
                    }
                }
            }
        }
    }

    pub(in crate::app) fn pending_cell_text(
        &self,
        tab_id: u64,
        position: CellPosition,
    ) -> Option<gpui::SharedString> {
        self.workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.pending_cell_text(position))
            .cloned()
            .map(gpui::SharedString::new)
    }

    /// Whether the change staged on the cell at display `position` sets NULL.
    pub(in crate::app) fn pending_is_null(&self, tab_id: u64, position: CellPosition) -> bool {
        self.workspace.runtime(tab_id).is_some_and(|runtime| {
            runtime.pending_edits.iter().any(|edit| {
                edit.row == position.row
                    && edit.column == position.column
                    && matches!(edit.request.value, MutationValue::Null)
            })
        })
    }

    /// The text an editor on the cell at display `position` opens with. A NULL
    /// (stored or staged) opens EMPTY, so the string
    /// `NULL` can be typed into it; committing the empty editor changes nothing.
    pub(in crate::app) fn cell_edit_text(
        &self,
        tab_id: u64,
        position: CellPosition,
    ) -> Option<gpui::SharedString> {
        if let Some(text) = self.pending_cell_text(tab_id, position) {
            return Some(if self.pending_is_null(tab_id, position) {
                gpui::SharedString::default()
            } else {
                text
            });
        }
        let source = CellPosition {
            row: position.row,
            column: self.source_column_for_display(tab_id, position.column)?,
        };
        let grid = self.grid(tab_id);
        if grid.cell_is_null(source) {
            return Some(gpui::SharedString::default());
        }
        grid.full_cell_text(source).map(gpui::SharedString::from)
    }

    /// The type an edit of the cell at (display `row`, `source_column`) is classified
    /// by: the column's resolved type; a byte-string column of a SQL result is
    /// `binary` whatever its catalog name (and a catalog `blob` holding text is
    /// text), so what is typed is checked against what the cell really is. A
    /// cell of a SQLite `mixed` column is classified by ITS storage class, not
    /// by the column: TEXT stays a string even when it looks numeric.
    pub(in crate::app) fn sql_edit_data_type(
        &self,
        tab_id: u64,
        source_column: usize,
        row: usize,
    ) -> String {
        if let Some(class) = self.cell_class(tab_id, row, source_column) {
            return match class {
                crate::grid_arrow::CellClass::Integer => "int64",
                crate::grid_arrow::CellClass::Real => "float",
                crate::grid_arrow::CellClass::Blob => "binary",
                crate::grid_arrow::CellClass::Text
                | crate::grid_arrow::CellClass::InvalidUtf8Text => "text",
            }
            .into();
        }
        let resolved = self
            .resolved_column_type(tab_id, source_column)
            .unwrap_or_else(|| "text".into());
        let arrow_binary = self.grid(tab_id).column_type(source_column) == Some("binary");
        match (arrow_binary, value_kind(&resolved) == ValueKind::Binary) {
            (true, false) => "binary".into(),
            (false, true) if self.grid(tab_id).column_type(source_column) != Some("mixed") => {
                "text".into()
            }
            _ => resolved,
        }
    }

    /// The wire value of the key cell at display `row`. A key cell of a SQLite `mixed` column is
    /// read by ITS class (a TEXT `'1'` stays a string); one that is not valid
    /// UTF-8 cannot address a row.
    pub(in crate::app) fn key_cell_value(
        &self,
        tab_id: u64,
        row: usize,
        source_column: usize,
        text: &str,
        catalog_type: &str,
    ) -> Result<MutationValue, SqlEditError> {
        if let Some(reason) = self.sql_column_read_only_reason(tab_id, source_column) {
            return Err(SqlEditError::KeyNotWritable(reason));
        }
        let data_type = match self.cell_class(tab_id, row, source_column) {
            Some(crate::grid_arrow::CellClass::InvalidUtf8Text) => {
                return Err(SqlEditError::KeyNotWritable(INVALID_UTF8_REASON))
            }
            Some(_) => self.sql_edit_data_type(tab_id, source_column, row),
            None => catalog_type.to_string(),
        };
        mutation_value_from_cell(text, &data_type).map_err(SqlEditError::InvalidKey)
    }

    /// The storage class of the cell at DISPLAY `row` (after any local page
    /// sort) and `source_column` of a SQLite `mixed` column. The one place a
    /// display row becomes a source row for a class lookup: every caller of
    /// the per-cell helpers below passes the row the user sees.
    pub(in crate::app) fn cell_class(
        &self,
        tab_id: u64,
        row: usize,
        source_column: usize,
    ) -> Option<crate::grid_arrow::CellClass> {
        let grid = self.grid(tab_id);
        match grid.source() {
            GridContentSource::Arrow(source) => {
                source.cell_class(grid.source_row(row), source_column)
            }
            _ => None,
        }
    }

    /// The kind of the connection a tab was opened on.
    pub(in crate::app) fn tab_connection_kind(&self, tab_id: u64) -> Option<ConnectionKind> {
        let tab = self.workspace.tab(tab_id)?;
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))
            .map(|saved| saved.kind)
    }

    /// Why no cell of `source_column` can be written back, whatever its value:
    /// a column the driver delivers as the server's text (label suffix
    /// `(read-only)`) and the ClickHouse composite kinds, which the driver
    /// refuses with `mutation.value_not_editable` for updates and keys.
    pub(in crate::app) fn sql_column_read_only_reason(
        &self,
        tab_id: u64,
        source_column: usize,
    ) -> Option<&'static str> {
        let native = self.grid(tab_id).native_type(source_column);
        if native.is_some_and(|native| native.trim_end().ends_with(READ_ONLY_SUFFIX)) {
            return Some(SERVER_TEXT_REASON);
        }
        (self.tab_connection_kind(tab_id) == Some(ConnectionKind::Clickhouse)
            && (native.is_some_and(is_clickhouse_composite)
                || self
                    .resolved_column_type(tab_id, source_column)
                    .is_some_and(|resolved| is_clickhouse_composite(&resolved))))
        .then_some(CLICKHOUSE_COMPOSITE_REASON)
    }

    /// Why the value of the cell at DISPLAY `row` and `source_column` cannot
    /// be written back, when the value itself is the reason: text that is not
    /// valid UTF-8 (shown as hex: sending `0x…` would store that literal
    /// text), MySQL GEOMETRY (WKT text).
    pub(in crate::app) fn sql_cell_read_only_reason(
        &self,
        tab_id: u64,
        row: usize,
        source_column: usize,
    ) -> Option<&'static str> {
        let grid = self.grid(tab_id);
        let native = grid.native_type(source_column);
        if matches!(
            self.cell_class(tab_id, row, source_column),
            Some(crate::grid_arrow::CellClass::InvalidUtf8Text)
        ) || (grid.column_type(source_column) == Some("binary")
            && native.is_some_and(|native| native.ends_with("(invalid utf-8)")))
        {
            return Some(INVALID_UTF8_REASON);
        }
        let mysql_family = matches!(
            self.tab_connection_kind(tab_id),
            Some(ConnectionKind::Mysql | ConnectionKind::Mariadb)
        );
        (mysql_family && native.is_some_and(|native| native.eq_ignore_ascii_case("geometry")))
            .then_some(GEOMETRY_REASON)
    }

    #[cfg(test)]
    pub(in crate::app) fn selection_as_tsv(
        &self,
        tab_id: u64,
        selection: &crate::grid::GridSelection,
    ) -> String {
        let Some(range) = selection.range else {
            return String::new();
        };
        let range = range.normalized();
        (range.start.row..=range.end.row)
            .map(|row| {
                (range.start.column..=range.end.column)
                    .map(|column| {
                        let position = CellPosition { row, column };
                        let pending = self
                            .pending_cell_text(tab_id, position)
                            .map(|text| text.to_string());
                        pending
                            .or_else(|| {
                                self.source_column_for_display(tab_id, column).and_then(
                                    |source_column| {
                                        self.grid(tab_id).full_cell_text(CellPosition {
                                            row,
                                            column: source_column,
                                        })
                                    },
                                )
                            })
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
                    .join("\t")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(in crate::app) fn begin_inline_edit(
        &mut self,
        tab_id: u64,
        row: usize,
        column: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id).cloned() else {
            return;
        };
        // A Redis element is edited in a dialog of its own.
        if tab.query_mode.is_redis_key() {
            self.select_grid_cell(tab_id, CellPosition { row, column });
            self.open_redis_element_editor(tab_id, window, cx);
            return;
        }
        let position = CellPosition { row, column };
        if self
            .view(tab_id)
            .grid_ui
            .inline_edit
            .as_ref()
            .is_some_and(|edit| edit.position == position)
        {
            return;
        }
        // A read-only connection opens no editor: the status row says why.
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        let is_mongo = tab.query_mode.is_mongo_find();
        if is_mongo {
            let Ok(row_fields) = self.mongo_row_fields(tab.id, row) else {
                return;
            };
            let Some(source_column) = self.source_column_for_display(tab.id, column) else {
                return;
            };
            let Some(column_name) = self
                .grid(tab.id)
                .column_name(source_column)
                .map(str::to_string)
            else {
                return;
            };
            // Never silent: the user double-clicked expecting an editor.
            if let Some(refusal) = mongo_edit_refusal(&row_fields, &column_name) {
                self.refuse_cell_edit(tab.id, position, refusal.to_string());
                self.notify_tab(tab.id, cx);
                return;
            }
            self.view_mut(tab.id).detail_ui.error = None;
            self.view_mut(tab.id).detail_ui.cell_refusal = None;
            // A field the document lacks is created by the write: its editor
            // opens empty and a value typed in shell syntax gives it its type.
            // A field a staged `$unset` removes is missing, whatever the page holds.
            let unset = self.workspace.runtime(tab.id).is_some_and(|runtime| {
                runtime.mongo.mutations.is_unset(row, &column_name)
            });
            let field = row_fields
                .fields
                .iter()
                .find(|field| field.name == column_name)
                .filter(|_| !unset);
            if field.is_some_and(|field| !field.editable) {
                return;
            }
            let native_type = field.map_or("missing", |field| field.native_type.as_str());
            let current_value = if unset {
                gpui::SharedString::default()
            } else {
                self.pending_cell_text(tab.id, position)
                    .unwrap_or_else(|| field.map(mongo_detail_value).unwrap_or_default().into())
            };
            let original_text = current_value.clone();
            let floating = cell_editor_should_float(
                window,
                current_value.as_ref(),
                self.view(tab.id)
                    .grid_ui
                    .layout
                    .widths
                    .get(column)
                    .copied()
                    .unwrap_or(crate::grid::ColumnLayout::DEFAULT_WIDTH),
            );
            let read_only = false;
            let editor = if floating {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(1, 6)
                        .soft_wrap(true)
                        .default_value(current_value.clone())
                })
                .into()
            } else {
                cx.new(|cx| InputState::new(window, cx).default_value(current_value.clone()))
                    .into()
            };
            let payload = if read_only {
                InlineEditPayload::ReadOnly
            } else {
                InlineEditPayload::Mongo {
                    native_type: native_type.to_string(),
                    identity: row_fields.mutation_identity.clone(),
                    database: tab.database.clone(),
                    collection: tab.collection.clone(),
                    field: column_name,
                }
            };
            payload.store(
                self,
                tab.id,
                position,
                editor,
                original_text,
                floating,
                read_only,
                window,
                cx,
            )
        } else {
            // A refusal is shown in the status row, never silent: the user
            // double-clicked expecting an editor.
            let (primary_key, key) = match self.sql_edit_target(tab.id, row, cx) {
                Ok(target) => target,
                Err(error) => {
                    self.view_mut(tab.id).detail_ui.error = Some(error.to_string());
                    self.notify_tab(tab.id, cx);
                    return;
                }
            };
            self.view_mut(tab.id).detail_ui.error = None;
            // The grid hands over a DISPLAY column; everything below the
            // editor's text (name, full text, type) is read at its source column.
            let Some(source_column) = self.source_column_for_display(tab.id, column) else {
                return;
            };
            let grid = self.grid(tab.id);
            let Some(key_column) = (0..grid.column_count())
                .find(|index| grid.column_name(*index) == Some(primary_key.name.as_str()))
            else {
                return;
            };
            let Some(key_text) = grid.full_cell_text(CellPosition {
                row,
                column: key_column,
            }) else {
                return;
            };
            let Ok(key_value) =
                self.key_cell_value(tab.id, row, key_column, &key_text, &primary_key.data_type)
            else {
                return;
            };
            let Some(column_name) = grid.column_name(source_column).map(str::to_string) else {
                return;
            };
            let position = CellPosition { row, column };
            let source_position = CellPosition {
                row,
                column: source_column,
            };
            let Some(original_text) = grid
                .full_cell_text(source_position)
                .map(gpui::SharedString::from)
            else {
                return;
            };
            let data_type = self.sql_edit_data_type(tab.id, source_column, row);
            // A read-only column opens no editor, NULL cells included; a
            // read-only VALUE only matters while it is the one on screen.
            let refusal = self
                .sql_column_read_only_reason(tab.id, source_column)
                .or_else(|| {
                    (!grid.cell_is_null(source_position)
                        && self.pending_cell_text(tab.id, position).is_none())
                    .then(|| self.sql_cell_read_only_reason(tab.id, row, source_column))
                    .flatten()
                });
            if let Some(reason) = refusal {
                self.view_mut(tab.id).detail_ui.error = Some(reason.into());
                self.notify_tab(tab.id, cx);
                return;
            }
            let current_value = self
                .cell_edit_text(tab.id, position)
                .unwrap_or_else(|| original_text.clone());
            let floating = cell_editor_should_float(
                window,
                current_value.as_ref(),
                self.view(tab.id)
                    .grid_ui
                    .layout
                    .widths
                    .get(column)
                    .copied()
                    .unwrap_or(crate::grid::ColumnLayout::DEFAULT_WIDTH),
            );
            let editor = if floating {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(1, 6)
                        .soft_wrap(true)
                        .default_value(current_value.clone())
                })
                .into()
            } else {
                cx.new(|cx| InputState::new(window, cx).default_value(current_value.clone()))
                    .into()
            };
            InlineEditPayload::Sql {
                primary_key,
                key_value,
                key,
                column_name,
                data_type,
            }
            .store(
                self,
                tab.id,
                position,
                editor,
                original_text,
                floating,
                false,
                window,
                cx,
            )
        };
    }

    pub(in crate::app) fn commit_inline_edit(
        &mut self,
        tab_id: u64,
        exit: InlineEditExit,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut edit) = self.view_mut(tab_id).grid_ui.inline_edit.take() else {
            return;
        };
        let text = edit.editor.value(cx);
        let row = edit.position.row;
        if text == edit.editor_start_text {
            if exit == InlineEditExit::Submit {
                self.restore_grid_focus(tab_id, window, cx);
            }
            self.notify_tab(tab_id, cx);
            return;
        }
        match &edit.payload {
            InlineEditPayload::Sql { .. } => {
                if self.workspace.runtime(tab_id).is_none() {
                    return;
                }
                if let Err(error) = self.try_stage_sql_edit(
                    tab_id,
                    edit.position,
                    &edit.payload,
                    edit.original_text.as_ref(),
                    text.as_ref(),
                ) {
                    // Never silent: a submit keeps the editor open with the
                    // reason in the status row; a blur closes it, and the
                    // reason stays in the status row.
                    let message = error.to_string();
                    if exit == InlineEditExit::Submit {
                        edit.error = Some(message);
                        self.view_mut(tab_id).grid_ui.inline_edit = Some(edit);
                    } else {
                        self.view_mut(tab_id).detail_ui.error = Some(message);
                    }
                    self.notify_tab(tab_id, cx);
                    return;
                }
                self.view_mut(tab_id).detail_ui.error = None;
            }
            InlineEditPayload::Mongo { field, .. } => {
                if text == edit.original_text {
                    if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                        runtime.mongo.mutations.remove_updates_for(row, field);
                        rebuild_mongo_pending_overlay(runtime);
                    }
                } else {
                    match build_inline_mongo_update(&edit.payload, row, text.as_ref()) {
                        Ok(mutation) => self.stage_mongo_mutation(tab_id, mutation, cx),
                        Err(error) => {
                            if exit == InlineEditExit::Submit {
                                edit.error = Some(error.to_string());
                                self.view_mut(tab_id).grid_ui.inline_edit = Some(edit);
                            }
                            self.notify_tab(tab_id, cx);
                            return;
                        }
                    }
                }
            }
            InlineEditPayload::ReadOnly => {}
        }
        if exit == InlineEditExit::Submit {
            self.restore_grid_focus(tab_id, window, cx);
        }
        self.refresh_cell_detail(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn cancel_inline_edit(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view_mut(tab_id).grid_ui.inline_edit.take().is_none() {
            return;
        }
        self.restore_grid_focus(tab_id, Some(window), cx);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn restore_grid_focus(
        &mut self,
        tab_id: u64,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if self
            .workspace
            .active_tab()
            .is_some_and(|tab| tab.id == tab_id)
        {
            if let Some(window) = window.filter(|window| window.is_window_active()) {
                self.view_mut(tab_id).grid_focus(cx).focus(window, cx);
            }
        }
    }

    pub(in crate::app) fn discard_pending_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.sql_apply_running)
        {
            self.view_mut(tab_id).detail_ui.error =
                Some("Wait for SQL Apply to finish before discarding.".into());
            self.notify_tab(tab_id, cx);
            return;
        }
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.pending_edits.clear();
            runtime.pending_sql_rows.clear();
            runtime.pending_deleted_rows.clear();
            runtime.clear_pending_cell_texts();
        }
        self.refresh_cell_detail(tab_id);
        self.notify_tab(tab_id, cx);
    }

    pub(in crate::app) fn apply_pending_edit(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        // Changes staged before the connection became read-only never land.
        if self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        if self.scenario_ui.service.is_some() {
            let Some(tab) = self.workspace.tab(tab_id).cloned() else {
                return;
            };
            if self
                .workspace
                .runtime(tab_id)
                .is_some_and(|runtime| !runtime.pending_sql_rows.is_empty())
            {
                self.view_mut(tab_id).detail_ui.error =
                    Some("Fixture cannot apply staged SQL row operations".into());
                self.notify_tab(tab_id, cx);
                return;
            }
            let mut requests = self
                .workspace
                .runtime(tab_id)
                .map(|runtime| {
                    runtime
                        .pending_edits
                        .iter()
                        .map(|pending| pending.request.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            for request in &mut requests {
                request.database = tab.database.clone();
                request.schema = tab.schema.clone().unwrap_or_default();
                request.table = tab.collection.clone();
            }
            let result = self.scenario_ui.service.as_mut().map(|service| {
                service.apply_sql_updates(
                    &tab.connection_id,
                    &tab.database,
                    tab.schema.as_deref().unwrap_or(""),
                    &tab.collection,
                    &requests,
                )
            });
            match result {
                Some(Ok(_)) => self.view_mut(tab_id).detail_ui.error = None,
                Some(Err(error)) => {
                    self.view_mut(tab_id).detail_ui.error = Some(error);
                    self.notify_tab(tab_id, cx);
                    return;
                }
                None => return,
            }
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.pending_edits.clear();
                runtime.clear_pending_cell_texts();
            }
            self.rerun_shown_result(tab_id, false, cx);
            return;
        }
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        if runtime.sql_apply_running
            || (runtime.pending_edits.is_empty() && runtime.pending_sql_rows.is_empty())
        {
            return;
        }
        // The running query's result would replace the rows being applied.
        if !runtime.query.can_run() {
            self.view_mut(tab_id).detail_ui.error = Some(SqlEditError::QueryRunning.to_string());
            self.notify_tab(tab_id, cx);
            return;
        }
        let submitted_edits = runtime.pending_edits.clone();
        let submitted_rows = runtime.pending_sql_rows.clone();
        let Some(tab) = self
            .workspace
            .tabs()
            .iter()
            .find(|tab| tab.id == tab_id)
            .cloned()
        else {
            return;
        };
        self.apply_sql_batch_through_driver(tab, submitted_edits, submitted_rows, cx);
    }

    /// The staged inserts and deletes all target `tab`'s table.
    fn staged_sql_rows_match_tab(
        &self,
        tab: &Tab,
        rows: &[crate::workspace::PendingSqlRow],
    ) -> bool {
        let expected_schema = self.sql_row_schema(tab);
        rows.iter().all(|pending| {
            let (database, schema, table) = match pending {
                crate::workspace::PendingSqlRow::Insert(request) => {
                    (&request.database, &request.schema, &request.table)
                }
                crate::workspace::PendingSqlRow::Delete { request, .. } => {
                    (&request.database, &request.schema, &request.table)
                }
            };
            database == &tab.database && table == &tab.collection && schema == &expected_schema
        })
    }

    /// A finished Apply, whichever engine ran it: the submitted entries leave
    /// the queue on success and the table reloads once nothing else is staged;
    /// a failure keeps everything staged and goes to the Error Log.
    pub(in crate::app) fn finish_sql_apply(
        &mut self,
        tab_id: u64,
        submitted_edits: &[crate::workspace::PendingCellEdit],
        submitted_rows: &[crate::workspace::PendingSqlRow],
        result: Result<(), SqlApplyError>,
        cx: &mut Context<Self>,
    ) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_apply_running = false;
            if result.is_ok() {
                reconcile_submitted_sql_batch(runtime, submitted_edits, submitted_rows);
            }
        }
        match result {
            Ok(()) => {
                self.view_mut(tab_id).detail_ui.error = None;
                if self.workspace.runtime(tab_id).is_some_and(|runtime| {
                    runtime.pending_edits.is_empty() && runtime.pending_sql_rows.is_empty()
                }) {
                    self.rerun_shown_result(tab_id, false, cx);
                }
            }
            // The user's own cancel is not a failure: nothing to log.
            Err(SqlApplyError::Cancelled) => {
                self.view_mut(tab_id).detail_ui.error =
                    Some("SQL Apply was cancelled; staged changes were preserved.".into());
            }
            Err(SqlApplyError::Failed(error)) => {
                let connection = self
                    .workspace
                    .tab(tab_id)
                    .map(|tab| tab.connection_id.clone());
                self.report_error(Area::Mutation, connection.as_deref(), &error);
                self.view_mut(tab_id).detail_ui.error = Some(apply_failure_message(&error));
            }
            // Part of the batch is committed and stays committed: it leaves
            // the staging area; the failed and never-attempted changes stay.
            Err(SqlApplyError::PartiallyApplied(partial)) => {
                let (edit_changes, row_changes) =
                    batch_change_indexes(submitted_edits, submitted_rows);
                let applied_edits: Vec<_> = submitted_edits
                    .iter()
                    .zip(&edit_changes)
                    .filter(|(_, change)| partial.applied.contains(change))
                    .map(|(edit, _)| edit.clone())
                    .collect();
                let applied_rows: Vec<_> = submitted_rows
                    .iter()
                    .zip(&row_changes)
                    .filter(|(_, change)| partial.applied.contains(change))
                    .map(|(row, _)| row.clone())
                    .collect();
                let delete_row = |row: &crate::workspace::PendingSqlRow| match row {
                    crate::workspace::PendingSqlRow::Delete { row, .. } => Some(*row),
                    crate::workspace::PendingSqlRow::Insert(_) => None,
                };
                let committed_rows: Vec<usize> = applied_edits
                    .iter()
                    .map(|edit| edit.row)
                    .chain(applied_rows.iter().filter_map(delete_row))
                    .collect();
                let unknown_rows: Vec<usize> = submitted_edits
                    .iter()
                    .zip(&edit_changes)
                    .filter(|(_, change)| partial.unknown.contains(change))
                    .map(|(edit, _)| edit.row)
                    .chain(
                        submitted_rows
                            .iter()
                            .zip(&row_changes)
                            .filter(|(_, change)| partial.unknown.contains(change))
                            .filter_map(|(row, _)| delete_row(row)),
                    )
                    .collect();
                let nothing_left = self.workspace.runtime_mut(tab_id).is_some_and(|runtime| {
                    reconcile_submitted_sql_batch(runtime, &applied_edits, &applied_rows);
                    let nothing_left =
                        runtime.pending_edits.is_empty() && runtime.pending_sql_rows.is_empty();
                    // The page cannot be reloaded under the changes that stay
                    // staged: the rows that were committed are marked instead.
                    if !nothing_left {
                        runtime.mark_rows_applied(committed_rows, unknown_rows);
                    }
                    nothing_left
                });
                let connection = self
                    .workspace
                    .tab(tab_id)
                    .map(|tab| tab.connection_id.clone());
                self.report_error(Area::Mutation, connection.as_deref(), &partial.message);
                let unknown_note = if partial.unknown.is_empty() {
                    ""
                } else {
                    " Rows marked ? may or may not have been written: the driver could not tell. They stay staged, but applying them again may insert a row twice or fail to delete one that is already gone; reload to see what is stored first."
                };
                self.view_mut(tab_id).detail_ui.error = Some(format!(
                    "Not rolled back: {} change(s) were applied and left the queue; {} failed{} and {} were not attempted and stay staged. {}{}",
                    partial.applied.len(),
                    partial.failed.len(),
                    if partial.unknown.is_empty() {
                        String::new()
                    } else {
                        format!(", {} are unknown", partial.unknown.len())
                    },
                    partial.not_attempted.len(),
                    if nothing_left {
                        ""
                    } else {
                        "Rows marked \u{21bb} were applied and still show their old values: apply or discard the staged changes, then reload to see the stored values."
                    },
                    unknown_note
                ));
                if nothing_left {
                    self.rerun_shown_result(tab_id, false, cx);
                }
            }
        }
        self.notify_tab(tab_id, cx);
    }

    /// The key column row edits are anchored on: the first column of the
    /// table's primary key (the whole key is captured from the row when the
    /// change is staged).
    pub(in crate::app) fn sql_edit_key(
        &self,
        tab_id: u64,
    ) -> Result<&crate::catalog::CatalogColumn, SqlEditError> {
        self.workspace
            .runtime(tab_id)
            .ok_or(SqlEditError::StructureUnavailable)?
            .structure_columns
            .iter()
            .find(|column| column.primary_key)
            .ok_or(SqlEditError::NoPrimaryKey)
    }

    /// `try_stage_inline_sql_edit` with the key rules of `tab_id`'s engine. The
    /// row at `position` must still be the one the payload's key was read from
    /// (a result that replaced the grid in between would otherwise stage the
    /// edit on whichever row now sits at that index), and no query may be
    /// running: its result is about to replace the rows. `position` is a
    /// DISPLAY cell; whether the cell was NULL is read from its source cell.
    pub(in crate::app) fn try_stage_sql_edit(
        &mut self,
        tab_id: u64,
        position: CellPosition,
        payload: &InlineEditPayload,
        original_text: &str,
        text: &str,
    ) -> Result<(), SqlEditError> {
        let InlineEditPayload::Sql { key, .. } = payload else {
            return Err(SqlEditError::NotSqlCell);
        };
        if self.connection_is_read_only(tab_id) {
            return Err(SqlEditError::ReadOnlyConnection);
        }
        if self.sql_row_key(tab_id, position.row).as_ref() != Ok(key) {
            return Err(SqlEditError::StaleRow);
        }
        let original_is_null = self
            .source_column_for_display(tab_id, position.column)
            .is_some_and(|column| {
                self.grid(tab_id).cell_is_null(CellPosition {
                    row: position.row,
                    column,
                })
            });
        let runtime = self
            .workspace
            .runtime_mut(tab_id)
            .ok_or(SqlEditError::StructureUnavailable)?;
        try_stage_inline_sql_edit(
            runtime,
            position,
            payload,
            (original_text, original_is_null),
            text,
        )
    }

    /// [`Self::try_stage_sql_edit`] for callers that only need to know whether
    /// the edit was staged.
    pub(in crate::app) fn stage_sql_edit(
        &mut self,
        tab_id: u64,
        position: CellPosition,
        payload: &InlineEditPayload,
        original_text: &str,
        text: &str,
    ) -> bool {
        self.try_stage_sql_edit(tab_id, position, payload, original_text, text)
            .is_ok()
    }

    /// Grid edits as ONE `batch` mutation on the connection's shared driver session (a table-data
    /// tab has no session of its own), off the UI thread. The driver applies it in one transaction
    /// (a SAVEPOINT inside a `BEGIN` somebody opened on that shared session): all changes land or
    /// none do. The request runs under an operation registered for the tab, so the tab's Cancel
    /// reaches it.
    fn apply_sql_batch_through_driver(
        &mut self,
        tab: Tab,
        submitted_edits: Vec<crate::workspace::PendingCellEdit>,
        submitted_rows: Vec<crate::workspace::PendingSqlRow>,
        cx: &mut Context<Self>,
    ) {
        let tab_id = tab.id;
        let request = if !self.staged_sql_rows_match_tab(&tab, &submitted_rows) {
            Err(SqlEditError::TargetMismatch)
        } else if !self
            .driver_runtime
            .sessions
            .contains_key(&tab.connection_id)
        {
            Err(SqlEditError::ConnectionUnavailable)
        } else {
            self.driver_sql_batch_request(&tab, &submitted_edits, &submitted_rows)
        };
        let mut request = match request {
            Ok(request) => request,
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error.to_string());
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let prepared = match self.begin_driver_query(&tab.connection_id) {
            Ok(prepared) => prepared,
            // No operation could be registered (the session was disconnected):
            // the same failure a rejected request is.
            Err(error) => {
                self.note_driver_connection_lost(tab_id, &error);
                let result = Err(SqlApplyError::from(error));
                self.finish_sql_apply(tab_id, &submitted_edits, &submitted_rows, result, cx);
                return;
            }
        };
        let PreparedDriverQuery {
            session,
            runner,
            operation,
        } = prepared;
        let operation_id = operation.operation_id;
        request.operation_id = operation_id;
        self.driver_runtime
            .query_operations
            .insert(tab_id, operation);
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.sql_apply_running = true;
        }
        // Lock-free: a slow batch must not queue the connection's navigator
        // calls, describes or page adoption behind the session lock. The
        // runner's operation stays registered (and cancellable) until the
        // driver has answered.
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                Ok::<_, DriverQueryError>(session.mutate(runner.operation(), request)?)
            })
            .await
        });
        let connection = tab.connection_id.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                // Only this Apply's own operation: a Cancel already took it, and a
                // query started since may have registered another.
                if this
                    .driver_runtime
                    .query_operations
                    .get(&tab_id)
                    .is_some_and(|operation| operation.operation_id == operation_id)
                {
                    this.driver_runtime.query_operations.remove(&tab_id);
                }
                if let Err(error) = &result {
                    this.note_driver_connection_lost(tab_id, error);
                }
                if let Ok(done) = &result {
                    this.invalidate_driver_pages(
                        &connection,
                        &done.invalidated_page_ids,
                        Some(tab_id),
                        cx,
                    );
                }
                let result = result.map(|_| ()).map_err(SqlApplyError::from);
                this.finish_sql_apply(tab_id, &submitted_edits, &submitted_rows, result, cx);
            });
        });
        task.detach();
        self.notify_tab(tab_id, cx);
    }

    /// The `batch` request for the staged changes of `tab`: updates first (one
    /// change per row, merging its edited columns), then inserts and deletes
    /// in staging order. Updates and deletes are keyed by the whole primary key
    /// captured when they were staged, never re-read from the grid: a refresh
    /// or reorder since then must not move a change onto another row.
    pub(in crate::app) fn driver_sql_batch_request(
        &self,
        tab: &Tab,
        edits: &[crate::workspace::PendingCellEdit],
        rows: &[crate::workspace::PendingSqlRow],
    ) -> Result<engine_driver_api::MutationRequest, SqlEditError> {
        use engine_driver_api::sql::{SqlBatch, SqlRowChange, SqlRowOp, SqlTableIdentity};
        use engine_driver_api::Capability;

        let capabilities = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(&tab.connection_id))
            .and_then(|saved| self.driver_capabilities(&tab.connection_id, saved.kind.driver_id()))
            .ok_or(SqlEditError::ConnectionUnavailable)?;
        if !capabilities.has(Capability::MutationBatch) {
            return Err(SqlEditError::BatchUnsupported);
        }

        let column_type = |column: &str| {
            self.workspace
                .runtime(tab.id)
                .and_then(|runtime| {
                    runtime
                        .structure_columns
                        .iter()
                        .find(|candidate| candidate.name == column)
                })
                .map_or("", |column| column.data_type.as_str())
        };
        let mut changes: Vec<SqlRowChange> = Vec::with_capacity(edits.len() + rows.len());
        // One update per row; the key's JSON text identifies the row.
        let mut update_of_row: HashMap<String, usize> = HashMap::new();
        for edit in edits {
            if edit.key.is_empty() {
                return Err(SqlEditError::MissingStagedKey { row: edit.row });
            }
            let identity = serde_json::to_string(&edit.key)?;
            let index = match update_of_row.entry(identity) {
                std::collections::hash_map::Entry::Occupied(slot) => *slot.get(),
                std::collections::hash_map::Entry::Vacant(slot) => {
                    changes.push(SqlRowChange {
                        op: SqlRowOp::Update,
                        key: edit.key.clone(),
                        values: Default::default(),
                    });
                    *slot.insert(changes.len() - 1)
                }
            };
            changes[index].values.insert(
                edit.request.column.clone(),
                mutation_value_to_json_for(&edit.request.value, column_type(&edit.request.column)),
            );
        }
        for pending in rows {
            changes.push(match pending {
                crate::workspace::PendingSqlRow::Insert(request) => SqlRowChange {
                    op: SqlRowOp::Insert,
                    key: Default::default(),
                    values: request
                        .values
                        .iter()
                        .map(|(column, value)| {
                            (
                                column.clone(),
                                mutation_value_to_json_for(value, column_type(column)),
                            )
                        })
                        .collect(),
                },
                crate::workspace::PendingSqlRow::Delete { row, key, .. } => {
                    if key.is_empty() {
                        return Err(SqlEditError::MissingStagedKey { row: *row });
                    }
                    SqlRowChange {
                        op: SqlRowOp::Delete,
                        key: key.clone(),
                        values: Default::default(),
                    }
                }
            });
        }
        if let Some(op) = changes
            .iter()
            .map(|change| change.op)
            .find(|op| !capabilities.supports_mutation_kind(op.kind()))
        {
            return Err(SqlEditError::KindUnsupported(op.kind()));
        }
        let identity = SqlTableIdentity {
            schema: tab.schema.clone().filter(|schema| !schema.is_empty()),
        };
        Ok(engine_driver_api::MutationRequest {
            // Replaced by a host-allocated id when sent.
            operation_id: crate::mongo_result::STAGED_OPERATION_ID,
            kind: "batch".into(),
            database: Some(tab.database.clone()).filter(|database| !database.is_empty()),
            object: Some(tab.collection.clone()),
            identity: serde_json::to_value(identity)?,
            payload: serde_json::to_value(SqlBatch { changes })?,
        })
    }

    /// What a cell edit on `row` of `tab_id` is anchored on: the key column and
    /// the row's whole key. A table whose structure is not loaded (a restored
    /// tab, a failed load) starts loading it, so the next attempt can succeed.
    fn sql_edit_target(
        &mut self,
        tab_id: u64,
        row: usize,
        cx: &mut Context<Self>,
    ) -> Result<(crate::catalog::CatalogColumn, crate::workspace::RowKey), SqlEditError> {
        let runtime = self
            .workspace
            .runtime(tab_id)
            .ok_or(SqlEditError::StructureUnavailable)?;
        // Staged changes are tied to the rows on screen: a running query is
        // about to replace them.
        if !runtime.query.can_run() {
            return Err(SqlEditError::QueryRunning);
        }
        if runtime.structure_columns.is_empty() {
            self.ensure_table_structure(tab_id, cx);
            return Err(match self.workspace.runtime(tab_id) {
                Some(runtime) if runtime.structure_loading => SqlEditError::StructureLoading,
                Some(runtime) => match &runtime.structure_error {
                    Some(error) => SqlEditError::StructureFailed(error.clone()),
                    None => SqlEditError::StructureUnavailable,
                },
                None => SqlEditError::StructureUnavailable,
            });
        }
        let key_column = self.sql_edit_key(tab_id)?.clone();
        let key = self.sql_row_key(tab_id, row)?;
        Ok((key_column, key))
    }

    /// The whole primary key of grid row `row` as a column -> value map, read
    /// from the grid on screen.
    pub(in crate::app) fn sql_row_key(
        &self,
        tab_id: u64,
        row: usize,
    ) -> Result<crate::workspace::RowKey, SqlEditError> {
        let runtime = self
            .workspace
            .runtime(tab_id)
            .ok_or(SqlEditError::StructureUnavailable)?;
        let grid = &runtime.grid;
        let key_columns = runtime
            .structure_columns
            .iter()
            .filter(|column| column.primary_key);
        let mut key = crate::workspace::RowKey::new();
        for column in key_columns {
            let source = (0..grid.column_count())
                .find(|index| grid.column_name(*index) == Some(column.name.as_str()))
                .ok_or_else(|| SqlEditError::KeyColumnMissing(column.name.clone()))?;
            let position = CellPosition {
                row,
                column: source,
            };
            let text = grid
                .full_cell_text(position)
                .filter(|_| !grid.cell_is_null(position))
                .ok_or_else(|| SqlEditError::KeyValueMissing {
                    row,
                    column: column.name.clone(),
                })?;
            let value = self.key_cell_value(tab_id, row, source, &text, &column.data_type)?;
            key.insert(
                column.name.clone(),
                mutation_value_to_json_for(&value, &column.data_type),
            );
        }
        if key.is_empty() {
            return Err(SqlEditError::NoPrimaryKey);
        }
        Ok(key)
    }
}

/// Why a SQL row edit, delete or staged batch cannot be built.
#[derive(Debug, PartialEq)]
pub(in crate::app) enum SqlEditError {
    NoActiveTab,
    NotDeletable,
    ApplyRunning,
    QueryRunning,
    StructureUnavailable,
    /// The table's keys and types are being fetched.
    StructureLoading,
    StructureFailed(String),
    NoPrimaryKey,
    CompositeKey,
    KeyColumnMissing(String),
    KeyValueMissing {
        row: usize,
        column: String,
    },
    /// The key cell's text does not parse as its column type.
    InvalidKey(MutationInputError),
    /// The key cell is shown but is not a value that can address the row.
    KeyNotWritable(&'static str),
    /// The grid no longer shows the row the change was prepared on.
    StaleRow,
    /// A staged change without the key captured at staging time.
    MissingStagedKey {
        row: usize,
    },
    TargetMismatch,
    ConnectionUnavailable,
    BatchUnsupported,
    KindUnsupported(&'static str),
    /// The request could not be encoded as JSON.
    Encode(String),
    /// The typed text is not a value of the column.
    InvalidInput(MutationInputError),
    /// The edit would set NULL in a column that is not nullable.
    NullNotAllowed(String),
    /// A key column is never edited in place.
    KeyColumn(String),
    RowStagedForDeletion,
    ReadOnlyConnection,
    /// The payload is not a SQL cell's.
    NotSqlCell,
}

impl std::fmt::Display for SqlEditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoActiveTab => f.write_str("No active table"),
            Self::NotDeletable => f.write_str("This SQL row cannot be deleted"),
            Self::ApplyRunning => f.write_str("Wait for SQL Apply to finish"),
            Self::QueryRunning => f.write_str("Wait for the running query to finish"),
            Self::StructureUnavailable => f.write_str("Table structure unavailable"),
            Self::StructureLoading => {
                f.write_str("Loading the table structure; try again in a moment")
            }
            Self::StructureFailed(reason) => {
                write!(f, "Table structure unavailable: {reason}")
            }
            Self::NoPrimaryKey => f.write_str("A single primary key is required"),
            Self::CompositeKey => {
                f.write_str("Composite primary keys are not supported for cell editing")
            }
            Self::KeyColumnMissing(column) => {
                write!(f, "Primary key column {column} is not in this result")
            }
            Self::KeyValueMissing { row, column } => {
                write!(f, "Row {} has no value for key {column}", row + 1)
            }
            Self::KeyNotWritable(reason) => write!(f, "The row's key cannot be used: {reason}"),
            Self::InvalidKey(error) => write!(f, "The row's key is not a valid value: {error}"),
            Self::StaleRow => f.write_str("The result changed; select the row again"),
            Self::MissingStagedKey { row } => write!(
                f,
                "Staged change for row {} has no key; discard it and stage it again.",
                row + 1
            ),
            Self::TargetMismatch => {
                f.write_str("Staged SQL row target no longer matches this result tab.")
            }
            Self::ConnectionUnavailable => {
                f.write_str("SQL connection is unavailable; staged changes were preserved")
            }
            Self::BatchUnsupported => f.write_str("This driver cannot apply batched row changes."),
            Self::KindUnsupported(kind) => write!(f, "This driver cannot {kind} rows."),
            Self::Encode(reason) => f.write_str(reason),
            Self::InvalidInput(error) => write!(f, "{error}"),
            Self::NullNotAllowed(column) => write!(f, "{column} cannot be NULL"),
            Self::KeyColumn(column) => {
                write!(
                    f,
                    "{column} is a primary key column and cannot be edited here"
                )
            }
            Self::RowStagedForDeletion => f.write_str("This row is staged for deletion"),
            Self::ReadOnlyConnection => f.write_str("This connection is read-only"),
            Self::NotSqlCell => f.write_str("This cell cannot be edited as a SQL value"),
        }
    }
}

impl std::error::Error for SqlEditError {}

impl From<MutationInputError> for SqlEditError {
    fn from(error: MutationInputError) -> Self {
        Self::InvalidInput(error)
    }
}

impl From<serde_json::Error> for SqlEditError {
    fn from(error: serde_json::Error) -> Self {
        Self::Encode(error.to_string())
    }
}

/// Why a staged SQL batch did not commit.
#[derive(Debug)]
pub(in crate::app) enum SqlApplyError {
    /// The user's own cancel (`mutation.cancelled`).
    Cancelled,
    /// The engine's message (a driver rejection keeps its
    /// `driver rejected the request: <code>: <message>` form for the Error Log).
    Failed(String),
    /// A batch on a table without transactions failed part way: the changes
    /// before the failing one ARE applied and were not rolled back.
    PartiallyApplied(PartialApply),
}

/// The outcome of a `mutation.partially_applied` answer: the batch's change
/// indexes (updates first, one per row; then inserts and deletes in staging
/// order) that were applied, failed and never attempted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct PartialApply {
    pub(in crate::app) message: String,
    pub(in crate::app) applied: Vec<usize>,
    pub(in crate::app) failed: Vec<usize>,
    /// Changes the driver cannot say were applied or not: they stay staged.
    pub(in crate::app) unknown: Vec<usize>,
    pub(in crate::app) not_attempted: Vec<usize>,
}

/// `(code, message)` of the stringified `driver rejected the request` error.
fn driver_rejection(error: &str) -> Option<(&str, &str)> {
    error
        .strip_prefix("driver rejected the request: ")?
        .split_once(": ")
}

/// The change numbers of the list `label: [1, 2]` that ENDS `message`, and
/// the message before it. The tail is read from the end: the server's own
/// error text in front of it may hold anything, even `applied: [9]`.
fn tail_list<'a>(message: &'a str, label: &str) -> Option<(Vec<usize>, &'a str)> {
    let message = message.trim_end().trim_end_matches('.');
    let body = message.strip_suffix(']')?;
    let at = body.rfind(&format!("{label}: ["))?;
    let list = &body[at + label.len() + 3..];
    let indexes = list
        .split(',')
        .map(str::trim)
        .filter(|index| !index.is_empty())
        .map(|index| index.parse().ok())
        .collect::<Option<Vec<usize>>>()?;
    let before = message[..at].trim_end().trim_end_matches(';').trim_end();
    Some((indexes, before))
}

impl PartialApply {
    /// `… applied: [..]; failed: [..]; unknown: [..]; not attempted: [..]`;
    /// `unknown` is absent from older drivers' replies.
    pub(in crate::app) fn parse(message: &str) -> Option<Self> {
        let (not_attempted, rest) = tail_list(message, "not attempted")?;
        let (unknown, rest) = tail_list(rest, "unknown").unwrap_or((Vec::new(), rest));
        let (failed, rest) = tail_list(rest, "failed")?;
        let (applied, _) = tail_list(rest, "applied")?;
        Some(Self {
            message: message.to_string(),
            applied,
            failed,
            unknown,
            not_attempted,
        })
    }
}

impl From<DriverQueryError> for SqlApplyError {
    /// A `*.confirmation_required` answer is not special-cased: the contract
    /// defines it for queries only, and the user already confirmed this batch
    /// in the Apply dialog, so it surfaces as the failure it is (with its code).
    fn from(error: DriverQueryError) -> Self {
        if error.is_cancelled() {
            return Self::Cancelled;
        }
        let text = error.to_string();
        match driver_rejection(&text) {
            Some((engine_driver_api::sql::codes::MUTATION_PARTIALLY_APPLIED, message)) => {
                PartialApply::parse(message)
                    .map_or(Self::Failed(text.clone()), Self::PartiallyApplied)
            }
            _ => Self::Failed(text),
        }
    }
}

/// What the status row says about a failed Apply: the driver's value
/// refusals in plain words (the Error Log keeps the raw text with its code).
pub(in crate::app) fn apply_failure_message(error: &str) -> String {
    use engine_driver_api::sql::codes;
    match driver_rejection(error) {
        Some((codes::MUTATION_INVALID_VALUE, message)) => format!(
            "A value cannot be written to its column: {message}. Nothing was written; the changes stay staged."
        ),
        Some((codes::MUTATION_VALUE_NOT_EDITABLE, message)) => format!(
            "This column cannot be edited: {message}. Nothing was written; discard that change."
        ),
        _ => crate::sessions::redact_secrets(error),
    }
}

/// For each staged cell edit and each staged row operation, the index of the
/// batch change it became (`driver_sql_batch_request` merges the edits of one
/// row into one update, in first-seen order; inserts and deletes follow).
fn batch_change_indexes(
    edits: &[crate::workspace::PendingCellEdit],
    rows: &[crate::workspace::PendingSqlRow],
) -> (Vec<usize>, Vec<usize>) {
    let mut update_of_row: HashMap<String, usize> = HashMap::new();
    let edit_changes: Vec<usize> = edits
        .iter()
        .map(|edit| {
            let next = update_of_row.len();
            *update_of_row
                .entry(serde_json::to_string(&edit.key).unwrap_or_default())
                .or_insert(next)
        })
        .collect();
    let first_row = update_of_row.len();
    (edit_changes, (first_row..first_row + rows.len()).collect())
}

/// The wire value of a staged cell. A whole number stays a JSON number when
/// it reads back as exactly the typed text; every other number (a decimal, a
/// float, `+5`, `000123`, `NaN`, a 30-digit value) travels as a JSON string
/// holding its exact text, which the driver binds as a parameter and the
/// server coerces. A JSON number would pass through a float: PostgreSQL's
/// driver refuses one with more than 15 significant digits for `numeric` and
/// `money`, so even a plain `12345678.12345678` must not be sent as one.
pub(in crate::app) fn mutation_value_to_json(value: &MutationValue) -> serde_json::Value {
    match value {
        MutationValue::Null => serde_json::Value::Null,
        MutationValue::Boolean(flag) => serde_json::Value::Bool(*flag),
        MutationValue::Text(text) => serde_json::Value::String(text.clone()),
        MutationValue::Number(text) => {
            if let Some(number) = text
                .parse::<i64>()
                .ok()
                .filter(|number| number.to_string() == *text)
            {
                return number.into();
            }
            if let Some(number) = text
                .parse::<u64>()
                .ok()
                .filter(|number| number.to_string() == *text)
            {
                return number.into();
            }
            serde_json::Value::String(text.clone())
        }
    }
}

/// [`mutation_value_to_json`] for a value of a column of `data_type`: every
/// number of a `decimal` / `numeric` / `money` column is its exact text, a
/// whole number too (the driver reads a JSON number there through a float).
pub(in crate::app) fn mutation_value_to_json_for(
    value: &MutationValue,
    data_type: &str,
) -> serde_json::Value {
    match value {
        MutationValue::Number(text)
            if value_kind(data_type) == ValueKind::Decimal
                || base_type_head(data_type) == "money" =>
        {
            serde_json::Value::String(text.clone())
        }
        _ => mutation_value_to_json(value),
    }
}

pub(in crate::app) fn reconcile_submitted_sql_batch(
    runtime: &mut crate::workspace::TabRuntime,
    submitted_edits: &[crate::workspace::PendingCellEdit],
    submitted_rows: &[crate::workspace::PendingSqlRow],
) {
    for submitted in submitted_edits {
        if let Some(index) = runtime
            .pending_edits
            .iter()
            .position(|edit| edit == submitted)
        {
            runtime.pending_edits.remove(index);
        }
    }
    runtime.clear_pending_cell_texts();
    for edit in runtime.pending_edits.clone() {
        runtime.set_pending_cell_text(
            CellPosition {
                row: edit.row,
                column: edit.column,
            },
            edit.display_text,
        );
    }
    for submitted in submitted_rows {
        if let Some(index) = runtime
            .pending_sql_rows
            .iter()
            .position(|row| row == submitted)
        {
            runtime.pending_sql_rows.remove(index);
        }
    }
    runtime.pending_deleted_rows = runtime
        .pending_sql_rows
        .iter()
        .filter_map(|row| {
            if let crate::workspace::PendingSqlRow::Delete { row, .. } = row {
                Some(*row)
            } else {
                None
            }
        })
        .collect();
}

/// Stages one edited cell of a SQL result at display `position`: the payload's
/// key is the first column of the primary key, which may span several columns
/// (the whole key is in the payload). `original` is the cell's text and null
/// flag as the grid holds them: typing `NULL` over a NULL is a change, typing
/// the stored text over the stored text is not.
pub(in crate::app) fn try_stage_inline_sql_edit(
    runtime: &mut crate::workspace::TabRuntime,
    position: CellPosition,
    payload: &InlineEditPayload,
    original: (&str, bool),
    text: &str,
) -> Result<(), SqlEditError> {
    let CellPosition { row, column } = position;
    let InlineEditPayload::Sql {
        primary_key,
        key_value,
        key,
        column_name,
        data_type,
    } = payload
    else {
        return Err(SqlEditError::NotSqlCell);
    };
    // A running query is about to replace the rows an edit would be tied to.
    if runtime.sql_apply_running {
        return Err(SqlEditError::ApplyRunning);
    }
    if !runtime.query.can_run() {
        return Err(SqlEditError::QueryRunning);
    }
    if runtime.pending_deleted_rows.contains(&row) {
        return Err(SqlEditError::RowStagedForDeletion);
    }
    if runtime
        .structure_columns
        .iter()
        .find(|column| column.primary_key)
        .is_none_or(|column| column.name != primary_key.name)
    {
        return Err(SqlEditError::StaleRow);
    }
    let value = mutation_value_from_input(text, data_type)?;
    let target = runtime
        .structure_columns
        .iter()
        .find(|column| column.name == *column_name);
    if primary_key.name == *column_name || target.is_some_and(|column| column.primary_key) {
        return Err(SqlEditError::KeyColumn(column_name.clone()));
    }
    if matches!(value, MutationValue::Null) && target.is_some_and(|column| !column.nullable) {
        return Err(SqlEditError::NullNotAllowed(column_name.clone()));
    }
    let cell = CellPosition { row, column };
    let (original_text, original_is_null) = runtime
        .pending_edits
        .iter()
        .find(|pending| pending.row == row && pending.column == column)
        .map(|pending| (pending.original_text.clone(), pending.original_is_null))
        .unwrap_or_else(|| (std::sync::Arc::<str>::from(original.0), original.1));
    let unchanged = if original_is_null {
        matches!(value, MutationValue::Null)
    } else {
        !matches!(value, MutationValue::Null) && text == original_text.as_ref()
    };
    if unchanged {
        runtime
            .pending_edits
            .retain(|pending| pending.row != row || pending.column != column);
        runtime.remove_pending_cell_text(cell);
        return Ok(());
    }
    let display_text = std::sync::Arc::<str>::from(if matches!(value, MutationValue::Null) {
        "NULL"
    } else {
        text
    });
    let request = crate::staged_sql::UpdateCellRequest {
        database: String::new(),
        schema: "public".into(),
        table: String::new(),
        key_column: primary_key.name.clone(),
        key_value: key_value.clone(),
        column: column_name.clone(),
        value,
    };
    if let Some(pending) = runtime
        .pending_edits
        .iter_mut()
        .find(|pending| pending.row == row && pending.column == column)
    {
        pending.display_text = display_text.clone();
        pending.request = request;
        pending.key = key.clone();
    } else {
        runtime
            .pending_edits
            .push(crate::workspace::PendingCellEdit {
                row,
                column,
                original_text,
                original_is_null,
                display_text: display_text.clone(),
                request,
                key: key.clone(),
            });
    }
    runtime.set_pending_cell_text(cell, display_text);
    Ok(())
}

/// [`try_stage_inline_sql_edit`] on a cell that was not NULL, as a flag.
#[cfg(test)]
pub(in crate::app) fn stage_inline_sql_edit(
    runtime: &mut crate::workspace::TabRuntime,
    row: usize,
    column: usize,
    payload: &InlineEditPayload,
    original_text: &str,
    text: &str,
) -> bool {
    try_stage_inline_sql_edit(
        runtime,
        CellPosition { row, column },
        payload,
        (original_text, false),
        text,
    )
    .is_ok()
}

/// Why an Insert Row / Clone Row draft is not a set of column values.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) enum InsertValuesError {
    Json(crate::json_text::ObjectError),
    UnknownColumn(String),
    InvalidValue {
        column: String,
        error: MutationInputError,
    },
    NullNotAllowed(String),
}

impl std::fmt::Display for InsertValuesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(error) => write!(f, "Insert values must be a JSON object: {error}"),
            Self::UnknownColumn(name) => write!(f, "Unknown column: {name}"),
            Self::InvalidValue { column, error } => write!(f, "{column}: {error}"),
            Self::NullNotAllowed(column) => write!(f, "{column} cannot be NULL"),
        }
    }
}

impl std::error::Error for InsertValuesError {}

impl From<InsertValuesError> for String {
    fn from(error: InsertValuesError) -> Self {
        error.to_string()
    }
}

/// The column values of an Insert Row / Clone Row draft. Numbers keep the
/// literal digits as written (`12345678901234567890.123456789` reaches a
/// numeric column exactly), strings are values of their column's kind and never
/// NULL by spelling; only JSON `null` is NULL.
pub(in crate::app) fn insert_values_from_json(
    input: &str,
    columns: &[crate::catalog::CatalogColumn],
) -> std::result::Result<Vec<(String, MutationValue)>, InsertValuesError> {
    use crate::json_text::Member;
    crate::json_text::object_members(input)
        .map_err(InsertValuesError::Json)?
        .into_iter()
        .map(|(name, member)| {
            let column = columns
                .iter()
                .find(|column| column.name == name)
                .ok_or_else(|| InsertValuesError::UnknownColumn(name.clone()))?;
            let value = match member {
                Member::Null => MutationValue::Null,
                Member::Bool(value) => MutationValue::Boolean(value),
                Member::Number(literal) => MutationValue::Number(literal.to_string()),
                Member::String(value) => mutation_value_from_cell(&value, &column.data_type)
                    .map_err(|error| InsertValuesError::InvalidValue {
                        column: name.clone(),
                        error,
                    })?,
                Member::Json(raw) => MutationValue::Text(raw.to_string()),
            };
            if matches!(value, MutationValue::Null) && !column.nullable {
                return Err(InsertValuesError::NullNotAllowed(column.name.clone()));
            }
            Ok((name, value))
        })
        .collect()
}
