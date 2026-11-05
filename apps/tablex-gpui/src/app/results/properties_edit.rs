//! Structure edit mode of a table tab's Properties (the Columns and Keys sub-tabs): the pending
//! change-set of the table's columns and primary key, held in memory on the tab.
//!
//! One [`PropertiesEdit`] entity per tab that is in edit mode. It owns the shared
//! [`EditSession`] (draft, undo history, review state), the [`EditContext`] AppRoot hands it and
//! the rows of the Columns grid with their state against the draft ([`EditRow`]); the rows are
//! computed once per change of the draft or of the described table, never while rendering. The
//! grid stays one virtualized `uniform_list`: it reads `rows` / `filtered` and draws only the
//! visible ones. The review dialog and Apply are the shared ones (`structure_edit`), AppRoot runs
//! the statements (`ApplyTarget::Properties`) and answers through [`PropertiesEdit::apply_finished`].
//!
//! Cells: the four text cells (Name, Type, Default, Comment) open one shared [`EditorState`] (the
//! code editor is the one official input with a completion menu, so the Type cell can list the
//! dialect's `common_types` and still take free text). Click, Enter or F2 opens a cell; Enter, Tab
//! and a lost focus commit it into the draft through `Draft::push` (rename, alter or, on a column
//! the draft adds, its `AddColumn`); Escape cancels. What the dialect refuses of typed text is
//! shown on the row (`!` marker) and blocks Apply through the plan, as in the diagram.
//!
//! The Indexes and Foreign keys sub-tabs and the Keys marker of the pending primary key live in
//! [`lists`]; they share this entity's draft, history, review dialog and Apply.
//!
//! The draft belongs to one table (connection, database, schema, table): when the tab shows
//! another table the entity is dropped (`AppRoot::sync_properties_edit`).

pub(in crate::app) mod lists;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use super::super::*;
use gpui::{EventEmitter, FocusHandle, SharedString};
use gpui_component::input::{
    CompletionProvider, Enter as InputEnter, Escape as InputEscape, IndentInline, InputEvent, OutdentInline, Rope, RopeExt,
};
use lsp_types::{CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit, CompletionTriggerKind, InsertTextFormat, Range as LspRange, TextEdit};
use super::columns_view::{HEADER_ROW_HEIGHT, ROW_HEIGHT};
use crate::app::structure_edit::{
    self, live, apply::TableOrigin, ApplyRequest, ApplyTarget, DraftEdit, EditContext, EditSession, NotSent, ReviewHost, Wording,
};
use crate::catalog::TableProperties;
use crate::columns_panel::{ColumnWidths, PropertiesTab, CELL_PADDING};
use gpui::UniformListScrollHandle;
use lists::{EditLists, Form, FormRender};
use crate::properties_model::PropertiesKey;
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::schema::{name_clashes, stale, LiveSchema, LiveTable};
use crate::sql_ddl::{Change, ColumnSpec, Planned, PrimaryKeyRef, TableId};

/// Where a column stands against the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum RowKind {
    Existing,
    /// Added by the draft.
    New,
    /// Altered, renamed or (de)selected as key by the draft.
    Changed,
    /// Dropped by the draft (stays listed, restorable).
    Dropped,
}

impl RowKind {
    /// The marker of the row's first cell.
    pub(in crate::app) fn marker(self) -> &'static str {
        match self {
            Self::Existing => "",
            Self::New => "+",
            Self::Changed => "*",
            Self::Dropped => "-",
        }
    }
}

/// One row of the Columns grid in edit mode: the column as the draft would leave it.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct EditRow {
    pub(in crate::app) kind: RowKind,
    pub(in crate::app) spec: ColumnSpec,
    pub(in crate::app) primary_key: bool,
}

impl EditRow {
    /// A key column holds no NULL, whatever its spec says.
    pub(in crate::app) fn nullable(&self) -> bool {
        self.spec.nullable && !self.primary_key
    }
}

/// A text cell of the Columns grid, in the order Tab walks them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Field {
    Name,
    Type,
    Default,
    Comment,
}

impl Field {
    pub(in crate::app) const ALL: [Self; 4] = [Self::Name, Self::Type, Self::Default, Self::Comment];

    fn index(self) -> usize {
        Self::ALL.iter().position(|field| *field == self).unwrap_or_default()
    }

    pub(in crate::app) fn slug(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Type => "type",
            Self::Default => "default",
            Self::Comment => "comment",
        }
    }

    /// The text of `spec` this cell shows and edits.
    fn text_str(self, spec: &ColumnSpec) -> &str {
        match self {
            Self::Name => &spec.name,
            Self::Type => &spec.data_type,
            Self::Default => spec.default.as_deref().unwrap_or_default(),
            Self::Comment => spec.comment.as_deref().unwrap_or_default(),
        }
    }

    fn text(self, spec: &ColumnSpec) -> String {
        self.text_str(spec).to_owned()
    }
}

/// How an open cell editor was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Exit {
    /// Enter, or Tab: a refused value keeps the editor open with the reason.
    Enter,
    Tab,
    /// The focus went elsewhere (another cell, the toolbar...): a refused value is dropped.
    Blur,
}

/// The dialect's `common_types` offered to the Type cell's editor: the one official control that
/// keeps free text (`varchar(320)`) next to suggestions is the editor's completion menu.
struct TypeSuggestions {
    types: &'static [&'static str],
    /// Only the Type cell asks for suggestions.
    active: Rc<Cell<bool>>,
}

impl CompletionProvider for TypeSuggestions {
    /// Every type when the menu is opened, those that start like the typed text while typing.
    /// A choice replaces the whole text.
    fn completions(
        &self,
        text: &Rope,
        _: usize,
        context: CompletionContext,
        _: &mut Window,
        _: &mut gpui::App,
    ) -> gpui::Task<gpui::Result<CompletionResponse>> {
        let typed = text.to_string().trim().to_lowercase();
        let all = context.trigger_kind == CompletionTriggerKind::INVOKED || typed.is_empty();
        let range = LspRange { start: text.offset_to_position(0), end: text.offset_to_position(text.len()) };
        let matching = self.types.iter().filter(|name| all || name.to_lowercase().starts_with(&typed));
        // The type that is already typed in full comes first (Enter keeps it).
        let (exact, rest): (Vec<_>, Vec<_>) = matching.partition(|name| !all && name.eq_ignore_ascii_case(&typed));
        let items = exact
            .into_iter()
            .chain(rest)
            .map(|name: &&str| CompletionItem {
                label: (*name).to_owned(),
                filter_text: Some((*name).to_owned()),
                kind: Some(CompletionItemKind::CLASS),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit { range, new_text: (*name).to_owned() })),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                ..CompletionItem::default()
            })
            .collect();
        gpui::Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut gpui::App) -> bool {
        self.active.get() && new_text.chars().nth(1).is_none()
    }
}

/// The catalog the checks and the plan read: the one described table.
fn live_schema(table: &TableId, live: &LiveTable) -> LiveSchema {
    let mut schema = LiveSchema::new(false);
    schema.present(table.clone(), live.clone());
    schema
}

/// The message of the first statement that did not generate, per column it is about.
fn row_errors(changes: &[Change], planned: &Planned) -> HashMap<String, String> {
    let mut errors = HashMap::new();
    for entry in &planned.entries {
        let Err(error) = &entry.statements else { continue };
        let names: Vec<&str> = match &changes[entry.change] {
            Change::AddColumn { column, .. } => vec![&column.name],
            Change::AlterColumn { to, .. } => vec![&to.name],
            Change::RenameColumn { to, .. } => vec![to],
            Change::SetPrimaryKey { columns, .. } => columns.iter().map(String::as_str).collect(),
            _ => Vec::new(),
        };
        for name in names {
            errors.entry(name.to_owned()).or_insert_with(|| error.to_string());
        }
    }
    errors
}

/// The rows of the Columns grid: the described columns in catalog order with what the draft does
/// to them, then the columns the draft adds. Linear in the columns plus the pending changes.
pub(in crate::app) fn build_rows(properties: &TableProperties, live: &LiveTable, draft: &Draft, table: &TableId) -> Vec<EditRow> {
    let live_key: Vec<&str> = live.primary_key.iter().flat_map(|key| key.columns.iter().map(String::as_str)).collect();
    let mut key: Vec<&str> = live_key.clone();
    let mut key_pending = false;
    let mut dropped: HashSet<&str> = HashSet::new();
    let mut renamed: HashMap<&str, &str> = HashMap::new();
    let mut altered: HashMap<&str, &ColumnSpec> = HashMap::new();
    let mut added: Vec<&ColumnSpec> = Vec::new();
    for change in draft.changes.iter().filter(|change| change.table() == Some(table)) {
        match change {
            Change::DropColumn { column, .. } => {
                dropped.insert(column);
            }
            Change::RenameColumn { from, to, .. } => {
                renamed.insert(from, to);
            }
            Change::AlterColumn { to, .. } => {
                altered.insert(&to.name, to);
            }
            Change::AddColumn { column, .. } => added.push(column),
            Change::SetPrimaryKey { columns, .. } => {
                key = columns.iter().map(String::as_str).collect();
                key_pending = true;
            }
            _ => {}
        }
    }
    // The catalog's key as the renames leave it (a pending key already names the final columns).
    if !key_pending {
        key = live_key.iter().map(|column| renamed.get(column).copied().unwrap_or(*column)).collect();
    }
    let described = properties.columns.iter().map(|column| {
        let mut spec = live::column_spec(column);
        let original = column.name.as_str();
        // A dropped column shows what the catalog has: a column renamed onto its name (the drop runs
        // first) must not lend it its alterations or key flag.
        if dropped.contains(original) {
            return EditRow { kind: RowKind::Dropped, spec, primary_key: live_key.contains(&original) };
        }
        let mut kind = RowKind::Existing;
        if let Some(name) = renamed.get(original) {
            spec.name = (*name).to_owned();
            kind = RowKind::Changed;
        }
        if let Some(to) = altered.get(spec.name.as_str()) {
            spec = (*to).clone();
            kind = RowKind::Changed;
        }
        let primary_key = key.contains(&spec.name.as_str());
        if primary_key != live_key.contains(&original) {
            kind = RowKind::Changed;
        }
        EditRow { kind, spec, primary_key }
    });
    let new = added.into_iter().map(|column| EditRow { kind: RowKind::New, spec: column.clone(), primary_key: key.contains(&column.name.as_str()) });
    described.chain(new).collect()
}

/// What the edit mode tells AppRoot.
pub(in crate::app) enum PropertiesEditEvent {
    /// Apply in the review dialog: run these changes on the connection.
    Apply(Box<ApplyRequest>),
    /// Cancel in the progress of a running apply.
    CancelApply,
    /// The foreign key form chose this table: describe it for its columns.
    Describe(String),
}

/// The edit mode of one tab's Properties.
pub(in crate::app) struct PropertiesEdit {
    /// The table the draft belongs to (the engine session is not part of its identity).
    key: PropertiesKey,
    table: TableId,
    context: EditContext,
    gate: EditGate,
    session: EditSession<DraftEdit>,
    /// The described table the draft starts from, and the `PropertiesState::revision` it came with.
    properties: Arc<TableProperties>,
    revision: u64,
    live: LiveTable,
    /// `live` as the staleness checks and the plan read it; the session holds the same one.
    live_schema: Arc<LiveSchema>,
    /// The Columns grid's rows, rebuilt when the draft or the described table changes.
    rows: Arc<Vec<EditRow>>,
    /// Indices into `rows` that match `filter`, in order.
    filtered: Arc<Vec<usize>>,
    filter: String,
    /// The selected column, by its name in `rows`.
    selected: Option<String>,
    /// The cell of the selected column the arrow keys, Enter and F2 act on.
    field: Field,
    /// A short status line (Apply's outcome, copied SQL).
    notice: Option<(String, bool)>,
    /// The one text editor of the tab, reused by whichever cell is open (never one per cell).
    editor: gpui::Entity<EditorState>,
    /// The cell the editor is in: the index into `rows`, and which text.
    editing: Option<(usize, Field)>,
    /// The column the open editor belongs to: rows come and go under it, the name finds its row.
    editing_name: Option<String>,
    /// Whether the editor offers the dialect's types (the Type cell is open).
    suggesting: Rc<Cell<bool>>,
    /// Where the focus goes when the editor closes.
    grid_focus: FocusHandle,
    /// What the dialect refuses of each row, by column name, computed with `rows`.
    errors: Arc<HashMap<String, String>>,
    /// The Indexes and Foreign keys rows and the pending key, computed with `rows`.
    lists: Arc<EditLists>,
    /// What the dialect refuses of each new index / foreign key, by sub-tab and name.
    list_errors: Arc<HashMap<(PropertiesTab, String), String>>,
    /// Indices into `lists` rows that match `filter`.
    index_filtered: Arc<Vec<usize>>,
    foreign_key_filtered: Arc<Vec<usize>>,
    index_scroll: UniformListScrollHandle,
    foreign_key_scroll: UniformListScrollHandle,
    /// The open "add index" / "add foreign key" form (its entities live while it is open).
    form: Option<Form>,
}

impl EventEmitter<PropertiesEditEvent> for PropertiesEdit {}

/// What the Columns grid and the toolbar read of the edit mode while rendering (cheap to clone).
#[derive(Clone)]
pub(in crate::app) struct EditRender {
    pub(in crate::app) rows: Arc<Vec<EditRow>>,
    pub(in crate::app) filtered: Arc<Vec<usize>>,
    pub(in crate::app) selected: Option<String>,
    pub(in crate::app) field: Field,
    pub(in crate::app) gate: EditGate,
    pub(in crate::app) pending: usize,
    pub(in crate::app) can_undo: bool,
    pub(in crate::app) can_redo: bool,
    pub(in crate::app) notice: Option<(String, bool)>,
    pub(in crate::app) editor: gpui::Entity<EditorState>,
    pub(in crate::app) editing: Option<(usize, Field)>,
    pub(in crate::app) errors: Arc<HashMap<String, String>>,
    pub(in crate::app) lists: Arc<EditLists>,
    pub(in crate::app) list_errors: Arc<HashMap<(PropertiesTab, String), String>>,
    pub(in crate::app) index_filtered: Arc<Vec<usize>>,
    pub(in crate::app) foreign_key_filtered: Arc<Vec<usize>>,
    pub(in crate::app) index_scroll: UniformListScrollHandle,
    pub(in crate::app) foreign_key_scroll: UniformListScrollHandle,
    pub(in crate::app) form: Option<FormRender>,
}

/// What the connection and the dialect allow of the Columns grid, with the reasons for what they do
/// not worded once (not on every row of every frame).
#[derive(Clone)]
pub(in crate::app) struct EditGate {
    /// Why every edit is off right now (read-only connection, the draft's table is gone).
    pub(in crate::app) blocked: Option<SharedString>,
    alter_column: bool,
    primary_key_edit: bool,
    column_comments: bool,
    no_alter: SharedString,
    no_comments: SharedString,
    no_key_edit: SharedString,
}

impl EditGate {
    fn of(context: &EditContext) -> Self {
        let dialect = context.scope.dialect();
        let caps = dialect.capabilities();
        let label = dialect.label();
        Self {
            blocked: context.disabled.clone().map(Into::into),
            alter_column: caps.alter_column,
            primary_key_edit: caps.primary_key_edit,
            column_comments: caps.column_comments,
            no_alter: format!("{label} cannot change an existing column without rebuilding the table.").into(),
            no_comments: format!("{label} has no column comments.").into(),
            no_key_edit: format!(
                "{label} cannot change the primary key of an existing table{}.",
                if label == "ClickHouse" { " (the sorting key is fixed)" } else { " without rebuilding it" }
            )
            .into(),
        }
    }

    /// Why the cell `field` of `row` cannot be edited, if it cannot.
    pub(in crate::app) fn field_block(&self, row: &EditRow, field: Field) -> Option<SharedString> {
        if let Some(reason) = &self.blocked {
            return Some(reason.clone());
        }
        if row.kind == RowKind::Dropped {
            return Some(DROPPED.clone());
        }
        if field == Field::Comment && !self.column_comments {
            return Some(self.no_comments.clone());
        }
        if field != Field::Name && row.kind != RowKind::New && !self.alter_column {
            return Some(self.no_alter.clone());
        }
        (matches!(field, Field::Type | Field::Default) && row.spec.generated)
            .then(|| SharedString::new_static("The type and the default of a generated column cannot be edited."))
    }

    /// Why the Nullable box of `row` cannot be used, if it cannot.
    pub(in crate::app) fn nullable_block(&self, row: &EditRow) -> Option<SharedString> {
        if let Some(reason) = &self.blocked {
            return Some(reason.clone());
        }
        if row.kind == RowKind::Dropped {
            return Some(DROPPED.clone());
        }
        if row.primary_key {
            return Some(SharedString::new_static("A primary key column cannot be NULL."));
        }
        (!self.alter_column && row.kind != RowKind::New).then(|| self.no_alter.clone())
    }

    /// Why the Key box of `row` cannot be used, if it cannot.
    pub(in crate::app) fn key_block(&self, row: &EditRow) -> Option<SharedString> {
        if let Some(reason) = &self.blocked {
            return Some(reason.clone());
        }
        if row.kind == RowKind::Dropped {
            return Some(DROPPED.clone());
        }
        (!self.primary_key_edit).then(|| self.no_key_edit.clone())
    }
}

const DROPPED: SharedString = SharedString::new_static("The column is dropped: restore it first.");

impl EditRender {
    pub(in crate::app) fn field_block(&self, row: &EditRow, field: Field) -> Option<SharedString> {
        self.gate.field_block(row, field)
    }

    pub(in crate::app) fn nullable_block(&self, row: &EditRow) -> Option<SharedString> {
        self.gate.nullable_block(row)
    }

    pub(in crate::app) fn key_block(&self, row: &EditRow) -> Option<SharedString> {
        self.gate.key_block(row)
    }
}

impl PropertiesEdit {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::app) fn new(
        key: PropertiesKey,
        table: TableId,
        context: EditContext,
        properties: Arc<TableProperties>,
        revision: u64,
        grid_focus: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let suggesting = Rc::new(Cell::new(false));
        let suggestions = TypeSuggestions { types: context.scope.dialect().common_types(), active: Rc::clone(&suggesting) };
        let editor = cx.new(|cx| {
            // A one-line text field: the code editor is the one input with completion; its code
            // niceties (brackets, indentation, folding, search) would only get in the way.
            let mut editor = EditorState::new(window, cx)
                .line_number(false)
                .soft_wrap(false)
                .searchable(false)
                .folding(false)
                .indent_guides(false)
                .auto_close(false)
                .smart_indent(false);
            editor.lsp_mut().completion_provider = Some(Rc::new(suggestions));
            editor
        });
        cx.subscribe_in(&editor, window, |this: &mut Self, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::Blur) {
                this.editor_blurred(window, cx);
            }
        })
        .detach();
        let live = live::live_table(&properties);
        let live_schema = Arc::new(live_schema(&table, &live));
        let mut session = EditSession::default();
        session.set_live_shared(Arc::clone(&live_schema));
        let mut edit = Self {
            key,
            table,
            gate: EditGate::of(&context),
            context,
            session,
            live,
            live_schema,
            properties,
            revision,
            rows: Arc::default(),
            filtered: Arc::default(),
            filter: String::new(),
            selected: None,
            field: Field::Name,
            notice: None,
            editor,
            editing: None,
            editing_name: None,
            suggesting,
            grid_focus,
            errors: Arc::default(),
            lists: Arc::default(),
            list_errors: Arc::default(),
            index_filtered: Arc::default(),
            foreign_key_filtered: Arc::default(),
            index_scroll: UniformListScrollHandle::new(),
            foreign_key_scroll: UniformListScrollHandle::new(),
            form: None,
        };
        edit.rebuild();
        edit
    }

    /// The table the draft is for.
    pub(in crate::app) fn origin(&self) -> TableOrigin {
        TableOrigin {
            connection: self.key.connection.clone(),
            database: self.key.database.clone(),
            schema: self.key.schema.clone(),
            object: self.key.object.clone(),
        }
    }

    /// Whether the draft is for this table.
    pub(in crate::app) fn is_for(&self, connection: &str, database: &str, schema: &str, object: &str) -> bool {
        self.key.covered_by(connection, Some(database), Some(schema), Some(object))
    }

    pub(in crate::app) fn revision(&self) -> u64 {
        self.revision
    }

    pub(in crate::app) fn pending_count(&self) -> usize {
        self.session.pending_count()
    }

    #[cfg(test)]
    pub(in crate::app) fn draft_changes(&self) -> &[Change] {
        &self.session.draft.changes
    }

    /// Puts `change` into the draft the way an editor does, for tests of what the plan says.
    #[cfg(test)]
    pub(in crate::app) fn push_for_test(&mut self, change: Change, cx: &mut Context<Self>) -> bool {
        self.edit_draft(cx, |draft| draft.push(change))
    }

    /// Puts all of `changes` into the draft as one edit (one rebuild), for benchmarks of a large draft.
    #[cfg(test)]
    pub(in crate::app) fn push_all_for_test(&mut self, changes: Vec<Change>, cx: &mut Context<Self>) -> bool {
        self.edit_draft(cx, |draft| changes.into_iter().for_each(|change| draft.push(change)))
    }

    /// Reruns everything derived from the draft and the described table, for benchmarks of it.
    #[cfg(test)]
    pub(in crate::app) fn rebuild_for_test(&mut self) {
        self.rebuild();
    }

    #[cfg(test)]
    pub(in crate::app) fn rows(&self) -> &[EditRow] {
        &self.rows
    }

    #[cfg(test)]
    pub(in crate::app) fn review_report(&self) -> Option<(String, bool)> {
        self.session.report().cloned()
    }

    #[cfg(test)]
    pub(in crate::app) fn review_applying(&self) -> bool {
        self.session.applying()
    }

    #[cfg(test)]
    pub(in crate::app) fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub(in crate::app) fn editable(&self) -> bool {
        self.context.disabled.is_none()
    }

    /// The position of column `name` in the filtered list.
    pub(in crate::app) fn position_of(&self, name: &str) -> Option<usize> {
        let row = self.row_index(name)?;
        self.filtered.iter().position(|&index| index == row)
    }

    /// What the Columns grid and the toolbar need to draw.
    pub(in crate::app) fn render_state(&self) -> EditRender {
        EditRender {
            rows: Arc::clone(&self.rows),
            filtered: Arc::clone(&self.filtered),
            selected: self.selected.clone(),
            field: self.field,
            gate: self.gate.clone(),
            pending: self.session.pending_count(),
            can_undo: self.session.history.can_undo(),
            can_redo: self.session.history.can_redo(),
            notice: self.notice.clone(),
            editor: self.editor.clone(),
            editing: self.editing,
            errors: Arc::clone(&self.errors),
            lists: Arc::clone(&self.lists),
            list_errors: Arc::clone(&self.list_errors),
            index_filtered: Arc::clone(&self.index_filtered),
            foreign_key_filtered: Arc::clone(&self.foreign_key_filtered),
            index_scroll: self.index_scroll.clone(),
            foreign_key_scroll: self.foreign_key_scroll.clone(),
            form: self.form_render(),
        }
    }

    // ---- what AppRoot feeds ----

    pub(in crate::app) fn set_context(&mut self, context: &EditContext, cx: &mut Context<Self>) {
        if self.context != *context {
            self.context = context.clone();
            self.gate = EditGate::of(context);
            self.rebuild();
            cx.notify();
        }
    }

    /// The described table changed (an apply refreshed it, the user refreshed): the draft is
    /// checked against it again, and a change an apply half-ran starts from the column now.
    pub(in crate::app) fn set_properties(&mut self, properties: Arc<TableProperties>, revision: u64, cx: &mut Context<Self>) {
        self.properties = properties;
        self.revision = revision;
        self.live = live::live_table(&self.properties);
        self.live_schema = Arc::new(live_schema(&self.table, &self.live));
        self.session.set_live_shared(Arc::clone(&self.live_schema));
        let mut draft = (*self.session.draft).clone();
        if self.session.reconcile_partial(&mut draft, &self.live_schema) {
            self.set_draft(Arc::new(draft), cx);
        } else {
            self.rebuild();
            cx.notify();
        }
    }

    pub(in crate::app) fn set_filter(&mut self, filter: &str, cx: &mut Context<Self>) {
        if self.filter != filter {
            self.filter = filter.to_owned();
            self.refilter();
            cx.notify();
        }
    }

    /// The tab left this table: nothing of the draft may be applied any more, and its dialog goes.
    pub(in crate::app) fn detach(&mut self, cx: &mut Context<Self>) {
        self.context.disabled = Some("The table of this tab changed: the pending structure changes were dropped.".into());
        self.gate = EditGate::of(&self.context);
        if let Some(window) = self.session.dialog_to_close(false) {
            structure_edit::close_dialog_later(window, cx);
        }
        cx.notify();
    }

    // ---- derived state ----

    /// Everything derived from the draft and the described table: the rows, the filtered list and
    /// which changes are stale.
    fn rebuild(&mut self) {
        #[cfg(test)]
        crate::perf_counters::bump(&crate::perf_counters::REBUILD);
        self.rows = Arc::new(build_rows(&self.properties, &self.live, &self.session.draft, &self.table));
        // A name the table already uses is the plan's to refuse (shown on its row); it is not "stale".
        // The plan takes the clashes found here and the review dialog reads the plan from the session.
        let clashes = name_clashes(&self.session.draft.changes, &self.live_schema, self.context.dialect().name_rules());
        let clashing: HashSet<usize> = clashes.iter().map(|(index, _)| *index).collect();
        self.session.stale = vec![None; self.session.draft.len()];
        for (index, reason) in stale(&self.session.draft.changes, &self.live_schema).into_iter().filter(|(index, _)| !clashing.contains(index)) {
            self.session.stale[index] = Some(reason);
        }
        let planned = self.session.planned(&self.context, Some(&clashes));
        self.errors = Arc::new(row_errors(&self.session.draft.changes, &planned));
        self.list_errors = Arc::new(lists::list_errors(&self.session.draft.changes, &planned));
        let indexes = lists::build_index_rows(&self.properties, &self.session.draft, &self.table);
        let foreign_keys = lists::build_foreign_key_rows(&self.properties, &self.session.draft, &self.table);
        let dialect = self.context.dialect();
        self.lists = Arc::new(EditLists {
            index_blocks: indexes.iter().map(|row| self.row_block_in(&*dialect, PropertiesTab::Indexes, row)).collect(),
            foreign_key_blocks: foreign_keys.iter().map(|row| self.row_block_in(&*dialect, PropertiesTab::ForeignKeys, row)).collect(),
            index_add_block: self.index_block(),
            foreign_key_add_block: self.foreign_key_block(),
            catalog_failed: self.context.catalog_error.is_some(),
            catalog_loading: self.context.catalog.is_none() && self.context.catalog_error.is_none(),
            key: lists::pending_key(&self.live, &self.session.draft, &self.table),
            indexes,
            foreign_keys,
        });
        // The open editor follows its column; the column is gone (removed, renamed): the edit goes.
        if let Some((_, field)) = self.editing {
            match self.editing_name.as_deref().and_then(|name| self.row_index(name)) {
                Some(row) => self.editing = Some((row, field)),
                None => {
                    self.editing = None;
                    self.editing_name = None;
                    self.suggesting.set(false);
                }
            }
        }
        self.refilter();
    }

    fn refilter(&mut self) {
        let needle = self.filter.to_lowercase();
        self.filtered = Arc::new(
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, row)| needle.is_empty() || row.spec.name.to_lowercase().contains(&needle))
                .map(|(index, _)| index)
                .collect(),
        );
        // The selection follows the column while it is listed.
        if self.selected.as_deref().is_some_and(|name| self.position_of(name).is_none()) {
            self.selected = None;
        }
        self.index_filtered = Arc::new(lists::filter_rows(&self.lists.indexes, &needle));
        self.foreign_key_filtered = Arc::new(lists::filter_rows(&self.lists.foreign_keys, &needle));
    }

    // ---- the draft ----

    /// Runs `change` on the draft (in place when nothing else holds it). When it changed anything
    /// the edit is recorded for undo.
    fn edit_draft(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Draft)) -> bool {
        if !self.editable() {
            return false;
        }
        match self.session.edit(change) {
            Some(after) => {
                self.set_draft(after, cx);
                true
            }
            None => false,
        }
    }

    fn set_draft(&mut self, draft: Arc<Draft>, cx: &mut Context<Self>) {
        self.session.set_draft(draft);
        self.rebuild();
        cx.notify();
    }

    pub(in crate::app) fn undo(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        if let Some(edit) = self.session.undo() {
            let draft = self.session.restore(&edit.before);
            self.set_draft(draft, cx);
        }
    }

    pub(in crate::app) fn redo(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        if let Some(edit) = self.session.redo() {
            let draft = self.session.restore(&edit.after);
            self.set_draft(draft, cx);
        }
    }

    pub(in crate::app) fn discard_all(&mut self, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        if let Some(after) = self.session.discard_all() {
            self.set_draft(after, cx);
        }
    }

    /// The index into `rows` of column `name`. A dropped column and one renamed onto its name share
    /// the name (the drop runs first): the column that stays is the one meant.
    fn row_index(&self, name: &str) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.spec.name == name && row.kind != RowKind::Dropped)
            .or_else(|| self.rows.iter().position(|row| row.spec.name == name))
    }

    fn row(&self, name: &str) -> Option<&EditRow> {
        self.row_index(name).map(|at| &self.rows[at])
    }

    pub(in crate::app) fn select(&mut self, name: Option<String>, cx: &mut Context<Self>) {
        // A column that is not listed (a click that removed it) is not a selection.
        if name.as_deref().is_some_and(|name| self.row_index(name).is_none()) {
            return;
        }
        if self.selected != name {
            self.selected = name;
            cx.notify();
        }
    }

    /// The column above (`-1`) or below (`1`) the selected one among the listed ones.
    pub(in crate::app) fn step_selection(&self, delta: isize) -> Option<String> {
        let last = self.filtered.len().checked_sub(1)?;
        let current = self.selected.as_deref().and_then(|name| self.position_of(name));
        let next = match current {
            None => 0,
            Some(position) => position.saturating_add_signed(delta).min(last),
        };
        Some(self.rows[self.filtered[next]].spec.name.clone())
    }

    // ---- column edits ----

    /// Marks column `name` dropped (a column the draft added is just forgotten).
    pub(in crate::app) fn drop_column(&mut self, name: &str, cx: &mut Context<Self>) {
        let table = self.table.clone();
        let column = name.to_owned();
        let (at, was_selected) = (self.position_of(name), self.selected.as_deref() == Some(name));
        self.edit_draft(cx, |draft| draft.push(Change::DropColumn { table, column }));
        // A column the draft added is gone, not dropped: the selection moves to its neighbour.
        if was_selected && self.row_index(name).is_none() {
            let next = at.and_then(|at| self.filtered.get(at).or(self.filtered.last()).copied());
            self.selected = next.map(|index| self.rows[index].spec.name.clone());
        }
    }

    /// Takes back the pending drop of column `name`.
    pub(in crate::app) fn restore_column(&mut self, name: &str, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        let Some(at) = self
            .session
            .draft
            .changes
            .iter()
            .position(|change| matches!(change, Change::DropColumn { table, column } if *table == self.table && column == name))
        else {
            return;
        };
        // A column renamed onto the dropped one's name holds it now: it cannot come back as well.
        if self.rows.iter().any(|row| row.kind != RowKind::Dropped && row.spec.name == name) {
            self.notice = Some((format!("A column named \"{name}\" exists now: rename it before restoring the dropped one."), true));
            cx.notify();
            return;
        }
        if let Some(after) = self.session.discard(at) {
            self.set_draft(after, cx);
        }
    }

    /// Delete on the selected column: drops it, or restores it when it is dropped.
    pub(in crate::app) fn toggle_drop_selected(&mut self, cx: &mut Context<Self>) {
        let Some(name) = self.selected.clone() else { return };
        match self.row(&name).map(|row| row.kind) {
            Some(RowKind::Dropped) => self.restore_column(&name, cx),
            Some(_) => self.drop_column(&name, cx),
            None => {}
        }
    }

    /// Sets whether column `name` allows NULL.
    pub(in crate::app) fn set_nullable(&mut self, name: &str, nullable: bool, cx: &mut Context<Self>) {
        let Some(row) = self.row(name).cloned() else { return };
        if self.gate.nullable_block(&row).is_some() || row.spec.nullable == nullable {
            return;
        }
        // The alteration starts from the column as it is now (the draft folds it into an earlier one).
        let (table, from) = (self.table.clone(), row.spec.clone());
        let to = ColumnSpec { nullable, ..row.spec };
        self.edit_draft(cx, |draft| draft.push(Change::AlterColumn { table, from, to }));
    }

    /// Adds column `name` to the primary key, or takes it out.
    pub(in crate::app) fn set_primary_key(&mut self, name: &str, on: bool, cx: &mut Context<Self>) {
        let Some(row) = self.row(name).cloned() else { return };
        if self.gate.key_block(&row).is_some() || row.primary_key == on {
            return;
        }
        // The key as it stands (the pending one, else the catalog's with the renames applied), in its own
        // order, with this one column added at the end or taken out.
        let mut columns = self.current_key();
        if on {
            columns.push(name.to_owned());
        } else {
            columns.retain(|column| column != name);
        }
        // Back to the catalog's key: nothing to replace, the pending change goes.
        if columns == self.catalog_key_renamed() {
            self.drop_pending_key(cx);
            return;
        }
        // The key being replaced under the names it has once the draft's renames ran (what the
        // replay of the change-set compares it with), with the catalog's constraint name.
        let previous = self
            .live
            .primary_key
            .as_ref()
            .filter(|key| !key.columns.is_empty())
            .map(|key| PrimaryKeyRef { name: key.name.clone(), columns: self.catalog_key_renamed() });
        let table = self.table.clone();
        self.edit_draft(cx, |draft| draft.push(Change::SetPrimaryKey { table, columns, previous }));
    }

    /// The catalog's primary key with the draft's column renames applied, in the key's order.
    fn catalog_key_renamed(&self) -> Vec<String> {
        let renames: HashMap<&str, &str> = self
            .session
            .draft
            .changes
            .iter()
            .filter_map(|change| match change {
                Change::RenameColumn { table, from, to } if *table == self.table => Some((from.as_str(), to.as_str())),
                _ => None,
            })
            .collect();
        let key = self.live.primary_key.iter().flat_map(|key| key.columns.iter());
        key.map(|column| renames.get(column.as_str()).map_or_else(|| column.clone(), |to| (*to).to_owned())).collect()
    }

    /// The primary key the draft would leave: the pending one, else the catalog's (renames applied).
    fn current_key(&self) -> Vec<String> {
        let pending = self.session.draft.changes.iter().find_map(|change| match change {
            Change::SetPrimaryKey { table, columns, .. } if *table == self.table => Some(columns.clone()),
            _ => None,
        });
        pending.unwrap_or_else(|| self.catalog_key_renamed())
    }

    /// Takes the pending primary key change out of the draft, if there is one.
    fn drop_pending_key(&mut self, cx: &mut Context<Self>) {
        let at = self.session.draft.changes.iter().position(|change| matches!(change, Change::SetPrimaryKey { table, .. } if *table == self.table));
        if let Some(after) = at.and_then(|at| self.session.discard(at)) {
            self.set_draft(after, cx);
        }
    }

    /// Adds a column with a placeholder name (`column1`, `column2`...) and the dialect's first
    /// common type; returns its name.
    pub(in crate::app) fn add_column(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let rules = self.context.scope.dialect().name_rules();
        let name = (1..)
            .map(|n| format!("column{n}"))
            .find(|name| !self.rows.iter().any(|row| rules.same(&row.spec.name, name)))?;
        let data_type = self.context.scope.dialect().common_types().first().copied().unwrap_or("integer");
        let column = ColumnSpec { name: name.clone(), data_type: data_type.into(), nullable: true, ..ColumnSpec::default() };
        let table = self.table.clone();
        self.edit_draft(cx, |draft| draft.push(Change::AddColumn { table, column })).then(|| {
            self.selected = Some(name.clone());
            self.refilter();
            name
        })
    }

    // ---- cell editing ----

    /// The index into `rows` of the selected column.
    fn selected_row(&self) -> Option<usize> {
        self.row_index(self.selected.as_deref()?)
    }

    pub(in crate::app) fn editing(&self) -> Option<(usize, Field)> {
        self.editing
    }

    #[cfg(test)]
    pub(in crate::app) fn editor(&self) -> &gpui::Entity<EditorState> {
        &self.editor
    }

    #[cfg(test)]
    pub(in crate::app) fn row_error(&self, name: &str) -> Option<&str> {
        self.errors.get(name).map(String::as_str)
    }

    /// Left (`-1`) and Right (`1`): the cell of the selected column the keys act on.
    pub(in crate::app) fn step_field(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = self.field.index().saturating_add_signed(delta).min(Field::ALL.len() - 1);
        if Field::ALL[next] != self.field {
            self.field = Field::ALL[next];
            cx.notify();
        }
    }

    /// Opens the editor on cell `field` of row `row` (an index into `rows`): the value is selected,
    /// and the Type cell lists the dialect's types. Whatever cell was open is committed first.
    /// `false`: the cell cannot be edited (the reason is the notice).
    pub(in crate::app) fn begin_edit(&mut self, row: usize, field: Field, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.editing == Some((row, field)) {
            return true;
        }
        self.commit_edit(Exit::Blur, window, cx);
        let Some(current) = self.rows.get(row).cloned() else { return false };
        self.selected = Some(current.spec.name.clone());
        self.field = field;
        if let Some(reason) = self.gate.field_block(&current, field) {
            self.notice = Some((reason.to_string(), true));
            cx.notify();
            return false;
        }
        self.notice = None;
        self.suggesting.set(field == Field::Type);
        let text = field.text(&current.spec);
        self.editor.update(cx, |editor, cx| {
            editor.set_value(text, window, cx);
            editor.focus(window, cx);
            editor.select_all(window, cx);
            if field == Field::Type {
                editor.show_completions(window, cx);
            }
        });
        self.editing = Some((row, field));
        self.editing_name = Some(current.spec.name.clone());
        cx.notify();
        true
    }

    /// Enter and F2 on the grid: opens the active cell of the selected column.
    pub(in crate::app) fn begin_edit_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row() {
            self.begin_edit(row, self.field, window, cx);
        }
    }

    /// Takes the text of the open editor into the draft. `true`: the editor is closed (the value
    /// was applied, was unchanged, or - for a blur - was refused and dropped); `false`: the value
    /// was refused and the editor stays open with the reason as the notice.
    pub(in crate::app) fn commit_edit(&mut self, exit: Exit, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((row, field)) = self.editing else { return true };
        let text = self.editor.read(cx).value().to_string();
        match self.apply_cell(row, field, &text, cx) {
            Ok(()) => {
                self.close_editor(cx);
                if exit == Exit::Enter {
                    self.grid_focus.focus(window, cx);
                }
                true
            }
            Err(reason) => {
                self.notice = Some((reason, true));
                if exit == Exit::Blur {
                    self.close_editor(cx);
                }
                cx.notify();
                exit == Exit::Blur
            }
        }
    }

    /// Escape: the editor closes and the value stays what the draft says.
    pub(in crate::app) fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_none() {
            return;
        }
        self.close_editor(cx);
        self.grid_focus.focus(window, cx);
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editing = None;
        self.editing_name = None;
        self.suggesting.set(false);
        self.editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
        cx.notify();
    }

    /// The editor lost the focus: what was typed is committed, unless the focus came back (a cell
    /// that opens another one moves it through here).
    fn editor_blurred(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() && !self.editor.read(cx).focus_handle(cx).is_focused(window) {
            self.commit_edit(Exit::Blur, window, cx);
        }
    }

    /// Runs before each frame: an editor that has lost the focus while its completion menu was open
    /// never announced it (the menu keeps a blur from being reported), so the cell is committed now
    /// that the menu is gone.
    pub(in crate::app) fn settle_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_none() {
            return;
        }
        let editor = self.editor.read(cx);
        if !editor.focus_handle(cx).is_focused(window) && !editor.completion_menu_state().open {
            self.commit_edit(Exit::Blur, window, cx);
        }
    }

    /// Tab (`1`) and Shift+Tab (`-1`): commits the open cell and opens the next (previous)
    /// editable one, down the rows. Returns the position in the list of the cell that opened.
    pub(in crate::app) fn tab_edit(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        let (row, field) = self.editing?;
        if !self.commit_edit(Exit::Tab, window, cx) {
            return None;
        }
        let start = self.filtered.iter().position(|&index| index == row)? * Field::ALL.len() + field.index();
        let total = self.filtered.len() * Field::ALL.len();
        let mut at = start;
        let target = loop {
            let Some(next) = at.checked_add_signed(delta).filter(|&next| next < total) else { break None };
            at = next;
            let (index, field) = (self.filtered[at / Field::ALL.len()], Field::ALL[at % Field::ALL.len()]);
            if self.gate.field_block(&self.rows[index], field).is_none() {
                break Some((index, field, at / Field::ALL.len()));
            }
        };
        match target {
            Some((index, field, position)) => self.begin_edit(index, field, window, cx).then_some(position),
            None => {
                self.grid_focus.focus(window, cx);
                None
            }
        }
    }

    /// Records the text of cell `field` of row `row` as a change of the draft (nothing when it is
    /// the value the row has). `Err`: the text is refused before it can enter the draft.
    fn apply_cell(&mut self, row: usize, field: Field, text: &str, cx: &mut Context<Self>) -> Result<(), String> {
        let Some(current) = self.rows.get(row).cloned() else { return Ok(()) };
        if let Some(reason) = self.gate.field_block(&current, field) {
            return Err(reason.to_string());
        }
        let table = self.table.clone();
        let mut spec = current.spec.clone();
        match field {
            Field::Name => {
                let to = text.trim().to_owned();
                if to == spec.name {
                    return Ok(());
                }
                // A dropped column's name is free: the drop runs before the rename.
                let rules = self.context.scope.dialect().name_rules();
                if self.rows.iter().enumerate().any(|(index, other)| index != row && other.kind != RowKind::Dropped && rules.same(&other.spec.name, &to)) {
                    return Err(format!("A column named \"{to}\" already exists."));
                }
                let from = spec.name.clone();
                let name = to.clone();
                if self.edit_draft(cx, |draft| draft.push(Change::RenameColumn { table, from, to })) {
                    self.selected = Some(name.clone());
                    self.note_row_error(&name);
                }
                return Ok(());
            }
            Field::Type => spec.data_type = text.trim().to_owned(),
            Field::Default => spec.default = Some(text.trim().to_owned()).filter(|default| !default.is_empty()),
            Field::Comment => spec.comment = Some(text.to_owned()).filter(|comment| !comment.trim().is_empty()),
        }
        if spec == current.spec {
            return Ok(());
        }
        let name = spec.name.clone();
        let change = match current.kind {
            RowKind::New => Change::AddColumn { table, column: spec },
            _ => Change::AlterColumn { table, from: current.spec, to: spec },
        };
        if self.edit_draft(cx, |draft| draft.push(change)) {
            self.note_row_error(&name);
        }
        Ok(())
    }

    /// When the dialect refuses what the draft now says of column `name`, the toolbar says so.
    fn note_row_error(&mut self, name: &str) {
        if let Some(reason) = self.errors.get(name) {
            self.notice = Some((format!("{name}: {reason}"), true));
        }
    }

    // ---- apply ----

    /// AppRoot ran the change-set. `done` are the indices of the changes that are applied for good;
    /// they leave the draft. The properties are refreshed by AppRoot, which feeds them to
    /// [`Self::set_properties`] when they arrive.
    pub(in crate::app) fn apply_finished(&mut self, done: &[usize], partial: &[usize], report: String, failed: bool, cx: &mut Context<Self>) {
        let completed = self.session.apply_finished(done, partial, &report, failed);
        if !completed.is_empty() {
            self.session.forget_applied(&completed);
            self.rebuild();
        }
        self.notice = Some((report.lines().next().unwrap_or_default().to_owned(), failed));
        cx.notify();
        if let Some(window) = self.session.dialog_to_close(failed) {
            structure_edit::close_dialog_later(window, cx);
        }
    }
}

impl ReviewHost for PropertiesEdit {
    type Edit = DraftEdit;
    const WORDING: Wording = Wording::PROPERTIES;

    fn session(&self) -> &EditSession<DraftEdit> {
        &self.session
    }

    fn session_mut(&mut self) -> &mut EditSession<DraftEdit> {
        &mut self.session
    }

    fn edit_context(&self) -> Option<&EditContext> {
        Some(&self.context)
    }

    fn before_review(&mut self, _: &mut Context<Self>) {}

    fn discard_change(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.editable() {
            return;
        }
        if let Some(after) = self.session.discard(index) {
            self.set_draft(after, cx);
        }
    }

    fn discard_all_changes(&mut self, cx: &mut Context<Self>) {
        if self.editable() {
            self.discard_all(cx);
        }
    }

    /// Apply: checks everything again, then hands the change-set (the very one that was reviewed)
    /// to AppRoot, which runs it on a dedicated session of the connection.
    fn request_apply(&mut self, reviewed: Vec<Change>, script: String, cx: &mut Context<Self>) {
        let request = match self.session.check_apply(reviewed, script, &self.context, &Wording::PROPERTIES) {
            Ok(request) => request,
            Err(NotSent::Ignored) => return,
            Err(NotSent::Refused(_)) => return cx.notify(),
        };
        self.session.apply_started(&request);
        cx.emit(PropertiesEditEvent::Apply(Box::new(request)));
        cx.notify();
    }

    fn cancel_apply(&mut self, cx: &mut Context<Self>) {
        if self.session.begin_cancel() {
            cx.emit(PropertiesEditEvent::CancelApply);
            cx.notify();
        }
    }

    fn show_notice(&mut self, text: String, failed: bool, cx: &mut Context<Self>) {
        self.notice = Some((text, failed));
        cx.notify();
    }
}

/// A text column of the Columns grid in edit mode: it starts from `basis` (the measured width of
/// the result's column, with a floor), grows into the room the viewport has and shrinks to `min`.
#[derive(Clone, Copy)]
struct Flex {
    basis: f32,
    min: f32,
}

/// Widths of the Columns grid in edit mode: the marker, the checkboxes and the action cell are
/// fixed, the four text columns flex, so the action cell (Drop / Restore / Remove) always fits.
#[derive(Clone, Copy)]
struct EditWidths {
    marker: f32,
    name: Flex,
    type_name: Flex,
    nullable: f32,
    key: f32,
    default: Flex,
    comment: Flex,
    action: f32,
}

impl EditWidths {
    fn new(measured: ColumnWidths) -> Self {
        Self {
            marker: 28.0,
            name: Flex { basis: measured.name.max(150.0), min: 110.0 },
            type_name: Flex { basis: measured.type_name.max(130.0), min: 100.0 },
            nullable: 72.0,
            key: 52.0,
            default: Flex { basis: measured.default.max(120.0), min: 90.0 },
            comment: Flex { basis: measured.comment.max(160.0), min: 110.0 },
            action: 84.0,
        }
    }

    /// The narrowest the grid gets (the viewport is narrower: the right edge is cut).
    fn min_total(&self) -> f32 {
        self.marker + self.name.min + self.type_name.min + self.nullable + self.key + self.default.min + self.comment.min + self.action
    }
}

/// The header, the virtualized rows and the title of the Columns grid in edit mode.
pub(super) struct EditColumnsBody {
    pub(super) title: String,
    pub(super) header: Div,
    pub(super) list: AnyElement,
    pub(super) empty: bool,
}

/// A text cell: [`Flex`] width.
fn flex_cell(width: Flex, header: bool) -> Div {
    grid_cell(0.0, header).w_auto().flex_shrink(1.0).flex_grow(1.0).flex_basis(px(width.basis)).min_w(px(width.min))
}

fn grid_cell(width: f32, header: bool) -> Div {
    div()
        .w(px(width))
        .h_full()
        .flex_shrink_0()
        .px(px(CELL_PADDING))
        .flex()
        .items_center()
        .overflow_hidden()
        .border_r_1()
        .border_color(theme::border())
        .when(header, |this| this.text_size(px(11.0)).text_color(theme::text_muted()))
}

/// The keys the open cell editor hands to the grid.
#[derive(Clone, Copy)]
enum CellKey {
    Enter,
    Escape,
    Tab(isize),
}

/// Whether a described DDL creates a view (`CREATE [OR REPLACE] [TEMP] [MATERIALIZED] [ALGORITHM=...] VIEW`):
/// the words before the object's kind decide, a table's `CREATE TABLE` comes first for a table.
pub(in crate::app) fn ddl_is_view(ddl: &str) -> bool {
    // Drivers may put `-- ...` lines or a `/* ... */` header before the statement.
    let mut ddl = ddl.trim_start();
    loop {
        if let Some(rest) = ddl.strip_prefix("--") {
            ddl = rest.split_once('\n').map_or("", |(_, rest)| rest).trim_start();
        } else if let Some(rest) = ddl.strip_prefix("/*") {
            ddl = rest.split_once("*/").map_or("", |(_, rest)| rest).trim_start();
        } else {
            break;
        }
    }
    let mut words = ddl.split_whitespace().map(|word| word.trim_matches(|c: char| c == ',' || c == ';').to_ascii_uppercase());
    if words.next().as_deref() != Some("CREATE") {
        return false;
    }
    for word in words.take(10) {
        match word.as_str() {
            "VIEW" => return true,
            "TABLE" | "INDEX" | "FUNCTION" | "PROCEDURE" | "TRIGGER" | "SEQUENCE" => return false,
            _ => {}
        }
    }
    false
}

impl AppRoot {
    /// Runs `edit` on the tab's edit mode, if it has one.
    pub(super) fn with_properties_edit(&mut self, tab_id: u64, cx: &mut Context<Self>, edit: impl FnOnce(&mut PropertiesEdit, &mut Context<PropertiesEdit>)) {
        if let Some(entity) = self.properties_edits.get(&tab_id).cloned() {
            entity.update(cx, edit);
        }
    }

    /// Whether the driver of `connection` describes tables (`metadata.table_properties`).
    pub(in crate::app) fn driver_describes_tables(&self, connection: &str) -> bool {
        let driver_id = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map(|saved| saved.kind.driver_id())
            .unwrap_or_default();
        match self.driver_capabilities(connection, driver_id) {
            Some(capabilities) => capabilities.has(engine_driver_api::Capability::MetadataTableProperties),
            // The UI fixture answers properties itself, without a driver.
            None => self.scenario_ui.service.is_some(),
        }
    }

    /// May the table of this tab's Properties be edited, and how. `None`: the tab has no table to
    /// edit (no toolbar). `Err`: why not (the toggle is disabled with this as its tooltip).
    pub(in crate::app) fn properties_edit_gate(&self, tab_id: u64) -> Option<Result<Rc<EditContext>, String>> {
        self.properties_edit_gate_with(tab_id, false)
    }

    /// [`Self::properties_edit_gate`]; `per_frame`: the edit context is the one computed once for this frame.
    fn properties_edit_gate_with(&self, tab_id: u64, per_frame: bool) -> Option<Result<Rc<EditContext>, String>> {
        let tab = self.workspace.tab(tab_id)?;
        let props = &self.workspace.runtime(tab_id)?.columns_panel.props;
        props.key.as_ref()?;
        let schema = tab.schema.clone().filter(|schema| !schema.is_empty());
        let context = if per_frame {
            self.structure_edit_context_this_frame(&tab.connection_id, &tab.database, schema.as_deref())
        } else {
            self.structure_edit_context(&tab.connection_id, &tab.database, schema.as_deref()).map(Rc::new)
        };
        let Some(context) = context else {
            return Some(Err("Structure editing is not available for this kind of connection.".into()));
        };
        if !self.driver_describes_tables(&tab.connection_id) {
            return Some(Err("This driver does not report table properties (metadata.table_properties): the table's structure cannot be edited.".into()));
        }
        let object = NavigatorNodeId::Object {
            connection: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: schema.clone().unwrap_or_default(),
            object: tab.collection.clone(),
        };
        // The navigator knows the kind once the Tables / Views group was loaded; the described DDL says
        // it as well (`CREATE [MATERIALIZED] VIEW`) when it was not.
        let is_view = match self.navigator.object_kind(&object) {
            Some(kind) => kind.is_view(),
            None => props.data.as_deref().and_then(|data| data.ddl.as_deref()).is_some_and(ddl_is_view),
        };
        if self.navigator.object_kind(&object) == Some(KindOfObject::Sequence) {
            return Some(Err("A sequence has no columns to edit: change it with ALTER SEQUENCE.".into()));
        }
        if is_view {
            return Some(Err(
                "Views and materialized views have no columns of their own to edit: change the query that defines them instead.".into(),
            ));
        }
        if let Some(reason) = &context.disabled {
            return Some(Err(reason.clone()));
        }
        Some(match (&props.status, props.data.as_deref()) {
            (_, Some(data)) if !data.columns.is_empty() => Ok(context),
            (crate::properties_model::LoadStatus::Failed(_), _) => Err("The table's properties could not be loaded.".into()),
            (crate::properties_model::LoadStatus::Loaded, Some(_)) => Err("The driver reported no columns for this table.".into()),
            _ => Err("The table's properties are still loading.".into()),
        })
    }

    /// The `Edit structure` toggle: starts the edit mode, or leaves it (only without pending changes).
    pub(in crate::app) fn toggle_properties_edit(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entity) = self.properties_edits.get(&tab_id) {
            if entity.read(cx).pending_count() == 0 {
                self.properties_edits.remove(&tab_id);
                self.notify_tab(tab_id, cx);
            } else {
                // Leaving would lose the draft: the toggle reads as on and says what to do.
                entity.update(cx, |edit, cx| edit.show_notice("Apply or discard the pending changes before leaving edit mode.".into(), true, cx));
            }
            return;
        }
        let Some(Ok(context)) = self.properties_edit_gate(tab_id) else { return };
        let Some(tab) = self.workspace.tab(tab_id) else { return };
        let Some(props) = self.workspace.runtime(tab_id).map(|runtime| &runtime.columns_panel.props) else { return };
        let (Some(key), Some(data)) = (props.key.clone(), props.data.clone()) else { return };
        let schema = tab.schema.clone().filter(|schema| !schema.is_empty());
        let table = context.scope.table_id(&tab.database, schema.as_deref(), &tab.collection);
        let revision = props.revision;
        let grid_focus = self.view_mut(tab_id).columns_focus(cx);
        let entity = cx.new(|cx| PropertiesEdit::new(key, table, Rc::unwrap_or_clone(context), data, revision, grid_focus, window, cx));
        cx.subscribe(&entity, move |this: &mut Self, entity, event: &PropertiesEditEvent, cx| {
            let target = ApplyTarget::Properties { tab_id, edit: entity.entity_id().as_u64() };
            match event {
                PropertiesEditEvent::Apply(request) => this.apply_structure_changes(target, (**request).clone(), cx),
                PropertiesEditEvent::CancelApply => this.cancel_structure_apply(target, cx),
                PropertiesEditEvent::Describe(table) => this.describe_referenced(tab_id, table.clone(), cx),
            }
        })
        .detach();
        cx.observe(&entity, move |this: &mut Self, _, cx| this.notify_tab(tab_id, cx)).detach();
        self.properties_edits.insert(tab_id, entity);
        self.notify_tab(tab_id, cx);
    }

    /// Keeps the tab's edit mode in step with the tab: it is dropped when the tab shows another
    /// table, and follows the context, the described table and the Columns filter.
    pub(in crate::app) fn sync_properties_edit(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entity) = self.properties_edits.get(&tab_id).cloned() else { return };
        let current = self.workspace.tab(tab_id).zip(self.workspace.runtime(tab_id)).map(|(tab, runtime)| {
            let panel = &runtime.columns_panel;
            let schema = tab.schema.clone().unwrap_or_default();
            (tab.connection_id.clone(), tab.database.clone(), schema, tab.collection.clone(), panel.props.data.clone(), panel.props.revision, panel.filter.clone())
        });
        let context = current.as_ref().and_then(|(connection, database, schema, ..)| {
            self.structure_edit_context_this_frame(connection, database, Some(schema.as_str()).filter(|schema| !schema.is_empty()))
        });
        let (Some((connection, database, schema, object, data, revision, filter)), Some(context)) = (current, context) else {
            return self.drop_properties_edit(tab_id, &entity, cx);
        };
        if !entity.read(cx).is_for(&connection, &database, &schema, &object) {
            return self.drop_properties_edit(tab_id, &entity, cx);
        }
        // The foreign key form lists the schema's tables: have the navigator load them if it has not.
        let namespace = Some(schema.as_str()).filter(|schema| !schema.is_empty());
        if context.catalog.is_none() && context.catalog_error.is_none() && self.structure_catalog_unrequested(&connection, &database, namespace) {
            self.load_structure_catalog(&connection, &database, namespace, false, cx);
        }
        entity.update(cx, |edit, cx| {
            edit.set_context(&context, cx);
            edit.set_filter(&filter, cx);
            edit.settle_editor(window, cx);
            edit.sync_form(window, cx);
            if let Some(data) = data.filter(|_| edit.revision() != revision) {
                edit.set_properties(data, revision, cx);
            }
        });
    }

    fn drop_properties_edit(&mut self, tab_id: u64, entity: &gpui::Entity<PropertiesEdit>, cx: &mut Context<Self>) {
        self.properties_edits.remove(&tab_id);
        entity.update(cx, |edit, cx| edit.detach(cx));
        self.notify_tab(tab_id, cx);
    }

    /// The tab's Properties edit mode, to draw it.
    pub(in crate::app) fn properties_edit_render(&self, tab_id: u64, cx: &gpui::App) -> Option<EditRender> {
        self.properties_edits.get(&tab_id).map(|entity| entity.read(cx).render_state())
    }

    /// The toolbar group of the Columns, Keys, Foreign keys and Indexes sub-tabs: the `Edit structure`
    /// toggle and, in edit mode, Changes (N), Undo, Redo, Discard all and the sub-tab's own Add
    /// button (column, foreign key, index). `None`: the tab has no table to edit.
    pub(in crate::app) fn render_properties_edit_toolbar(&self, tab_id: u64, tab: PropertiesTab, cx: &mut Context<Self>) -> Option<AnyElement> {
        let gate = self.properties_edit_gate_with(tab_id, true)?;
        let state = self.properties_edit_render(tab_id, cx);
        let editing = state.is_some();
        // Leaving the edit mode would lose the draft: it asks for Apply or Discard all first.
        let (toggle_reason, tooltip) = match (&state, gate) {
            (Some(state), _) if state.pending > 0 => (
                Some("Apply or discard the pending changes before leaving edit mode.".to_owned()),
                "Apply or discard the pending changes before leaving edit mode.".to_owned(),
            ),
            (Some(_), _) => (None, "Leave the structure edit mode".to_owned()),
            (None, Ok(_)) => (None, "Edit the columns, the primary key, the indexes and the foreign keys of this table".to_owned()),
            (None, Err(reason)) => (Some(reason.clone()), reason),
        };
        let toggle = Button::new(("properties-edit-toggle", tab_id))
            .icon(Icon::empty().path("icons/pencil-ruler.svg").size(px(14.0)))
            .label("Edit structure")
            .with_size(Size::Small)
            .compact()
            .ghost()
            .selected(editing)
            .disabled(toggle_reason.is_some() && !editing)
            .flex_shrink_0()
            .tooltip(tooltip)
            .debug_selector(|| "properties-edit-toggle".into())
            .on_click(cx.listener(move |this, _, window, cx| this.toggle_properties_edit(tab_id, window, cx)));
        let group = div().flex().items_center().gap(px(4.0)).flex_shrink_0().child(toggle);
        let Some(state) = state else { return Some(group.into_any_element()) };
        let editable = state.gate.blocked.is_none();
        let button = |id: &'static str, label: &'static str| {
            Button::new((id, tab_id)).label(label).with_size(Size::Small).compact().ghost().flex_shrink_0().debug_selector(move || id.into())
        };
        let icon_button = |id: &'static str, path: &'static str, tooltip: &'static str| {
            Button::new((id, tab_id))
                .icon(Icon::empty().path(path).size(px(14.0)))
                .with_size(Size::Small)
                .compact()
                .ghost()
                .flex_shrink_0()
                .tooltip(tooltip)
                .debug_selector(move || id.into())
        };
        let changes = if state.pending > 0 { format!("Changes ({})", state.pending) } else { "Changes".to_owned() };
        Some(
            group
                .child(
                    button("properties-edit-changes", "Changes")
                        .label(changes)
                        .selected(state.pending > 0)
                        .tooltip(if state.pending > 0 {
                            "Review and apply the pending structure changes"
                        } else {
                            "No pending structure changes"
                        })
                        .on_click(cx.listener(move |this, _, window, cx| this.open_properties_changes(tab_id, window, cx))),
                )
                .child(
                    icon_button("properties-edit-undo", "icons/undo-2.svg", "Undo")
                        .disabled(!state.can_undo || !editable)
                        .on_click(cx.listener(move |this, _, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.undo(cx)))),
                )
                .child(
                    icon_button("properties-edit-redo", "icons/redo-2.svg", "Redo")
                        .disabled(!state.can_redo || !editable)
                        .on_click(cx.listener(move |this, _, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.redo(cx)))),
                )
                .child(
                    icon_button("properties-edit-discard-all", "icons/eraser.svg", "Discard all pending changes")
                        .disabled(state.pending == 0 || !editable)
                        .on_click(cx.listener(move |this, _, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.discard_all(cx)))),
                )
                .children((tab == PropertiesTab::Columns).then(|| {
                    button("properties-edit-add-column", "Column")
                        .icon(Icon::empty().path("icons/plus.svg").size(px(14.0)))
                        .disabled(!editable)
                        .tooltip(state.gate.blocked.clone().unwrap_or_else(|| "Add a column to this table".into()))
                        .on_click(cx.listener(move |this, _, window, cx| this.add_properties_column(tab_id, window, cx)))
                }))
                .children((tab == PropertiesTab::Indexes).then(|| {
                    button("properties-edit-add-index", "Index")
                        .icon(Icon::empty().path("icons/plus.svg").size(px(14.0)))
                        .disabled(state.lists.index_add_block.is_some())
                        .tooltip(state.lists.index_add_block.clone().unwrap_or_else(|| "Add an index to this table".to_owned()))
                        .on_click(cx.listener(move |this, _, window, cx| this.open_properties_index_form(tab_id, window, cx)))
                }))
                .children((tab == PropertiesTab::ForeignKeys).then(|| {
                    button("properties-edit-add-foreign-key", "Foreign key")
                        .icon(Icon::empty().path("icons/plus.svg").size(px(14.0)))
                        .disabled(state.lists.foreign_key_add_block.is_some())
                        .tooltip(state.lists.foreign_key_add_block.clone().unwrap_or_else(|| "Add a foreign key to this table".to_owned()))
                        .on_click(cx.listener(move |this, _, window, cx| this.open_properties_foreign_key_form(tab_id, window, cx)))
                }))
                .children(state.notice.map(|(text, failed)| {
                    div()
                        .debug_selector(|| "properties-edit-notice".into())
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.0))
                        .text_color(if failed { theme::danger() } else { theme::text_muted() })
                        .child(text)
                }))
                .into_any_element(),
        )
    }

    /// Retry on the Foreign keys sub-tab after the schema's table names failed to load.
    pub(in crate::app) fn retry_structure_catalog(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else { return };
        let (connection, database) = (tab.connection_id.clone(), tab.database.clone());
        let schema = tab.schema.clone().filter(|schema| !schema.is_empty());
        self.load_structure_catalog(&connection, &database, schema.as_deref(), true, cx);
        self.notify_tab(tab_id, cx);
    }

    /// Takes the text of the tab's open cell editor into the draft (nothing is open: no-op).
    pub(in crate::app) fn commit_properties_edit_cell(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entity) = self.properties_edits.get(&tab_id).cloned() {
            entity.update(cx, |edit, cx| {
                edit.commit_edit(Exit::Blur, window, cx);
            });
        }
    }

    /// Add column: a placeholder row, its name open for typing.
    fn add_properties_column(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entity) = self.properties_edits.get(&tab_id).cloned() else { return };
        let position = entity.update(cx, |edit, cx| {
            edit.commit_edit(Exit::Blur, window, cx);
            let name = edit.add_column(cx)?;
            let row = edit.rows.iter().position(|row| row.spec.name == name)?;
            edit.begin_edit(row, Field::Name, window, cx);
            edit.position_of(&name)
        });
        if let Some(position) = position {
            self.view(tab_id).columns_scroll.scroll_to_item(position, gpui::ScrollStrategy::Nearest);
        }
    }

    /// Changes (N): opens the shared review dialog.
    pub(in crate::app) fn open_properties_changes(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entity) = self.properties_edits.get(&tab_id).cloned() {
            entity.update(cx, |edit, cx| {
                // What is being typed is part of what is reviewed.
                edit.commit_edit(Exit::Blur, window, cx);
                structure_edit::open_review(edit, window, cx)
            });
        }
    }

    /// Delete, the arrow keys, Enter and F2 of the Columns grid in edit mode. `true`: the key was
    /// used. While a cell is open the editor owns every key.
    pub(in crate::app) fn properties_edit_key(&mut self, tab_id: u64, key: &str, grid_focused: bool, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(entity) = self.properties_edits.get(&tab_id).cloned() else { return false };
        if entity.read(cx).editing().is_some() {
            return false;
        }
        match key {
            "down" | "up" => {
                let delta = if key == "down" { 1 } else { -1 };
                let target = entity.read(cx).step_selection(delta);
                let Some(name) = target else { return false };
                let position = entity.update(cx, |edit, cx| {
                    edit.select(Some(name.clone()), cx);
                    edit.position_of(&name)
                });
                if let Some(position) = position {
                    self.view(tab_id).columns_scroll.scroll_to_item(position, gpui::ScrollStrategy::Nearest);
                }
                self.refocus_grid(tab_id, window, cx);
                true
            }
            // Only with the grid itself focused: these keys belong to the filter box when it has the focus.
            "left" | "right" if grid_focused => {
                entity.update(cx, |edit, cx| edit.step_field(if key == "right" { 1 } else { -1 }, cx));
                true
            }
            "enter" | "f2" if grid_focused => {
                entity.update(cx, |edit, cx| edit.begin_edit_selected(window, cx));
                true
            }
            "delete" | "backspace" if grid_focused => {
                entity.update(cx, |edit, cx| edit.toggle_drop_selected(cx));
                true
            }
            _ => false,
        }
    }

    /// Enter, Tab and Escape of the open cell editor.
    fn properties_cell_key(&mut self, tab_id: u64, key: CellKey, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entity) = self.properties_edits.get(&tab_id).cloned() else { return };
        let position = entity.update(cx, |edit, cx| match key {
            CellKey::Enter => {
                edit.commit_edit(Exit::Enter, window, cx);
                None
            }
            CellKey::Escape => {
                edit.cancel_edit(window, cx);
                None
            }
            CellKey::Tab(delta) => edit.tab_edit(delta, window, cx),
        });
        if let Some(position) = position {
            self.view(tab_id).columns_scroll.scroll_to_item(position, gpui::ScrollStrategy::Nearest);
        }
    }

    /// `table-properties-edit`: the orders tab shows its Properties > Columns; the edit mode and
    /// its sample edits follow once the table is described (see [`Self::run_properties_edit_seed`]).
    pub(in crate::app) fn seed_scenario_properties_edit(&mut self, tab_id: u64) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.columns_panel.presentation = crate::columns_panel::Presentation::Columns;
        }
        self.scenario_ui.properties_edit_seed = Some(tab_id);
    }

    /// Runs the pending `table-properties-edit` seed when the table of `tab_id` can be edited.
    pub(in crate::app) fn run_properties_edit_seed(&mut self, tab_id: u64, cx: &mut Context<Self>, window: &mut Window) {
        if self.scenario_ui.properties_edit_seed != Some(tab_id) || self.properties_edit_gate(tab_id).is_none_or(|gate| gate.is_err()) {
            return;
        }
        self.scenario_ui.properties_edit_seed = None;
        cx.defer_in(window, move |this, window, cx| this.seed_properties_edit(tab_id, window, cx));
    }

    /// The edit mode with a rename, a type change, a dropped and an added column, an index and a
    /// foreign key pending (`TABLEX_SEED_PROPERTIES=review`: the Changes dialog is open; `=type`:
    /// the Type cell of `payload` is open on its picker; `=indexes` / `=foreign-keys`: that
    /// sub-tab is shown).
    fn seed_properties_edit(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_properties_edit(tab_id, window, cx);
        let Some(entity) = self.properties_edits.get(&tab_id).cloned() else { return };
        let seed = std::env::var("TABLEX_SEED_PROPERTIES").unwrap_or_default();
        let wants = |token: &str| seed.split(',').any(|wanted| wanted == token);
        fn stage(edit: &mut PropertiesEdit, name: &str, field: Field, text: &str, cx: &mut Context<PropertiesEdit>) {
            if let Some(row) = edit.rows.iter().position(|row| row.spec.name == name) {
                let _ = edit.apply_cell(row, field, text, cx);
            }
        }
        entity.update(cx, |edit, cx| {
            stage(edit, "status", Field::Name, "order_status", cx);
            stage(edit, "score", Field::Type, "numeric(12,2)", cx);
            edit.drop_column("big_number", cx);
            if let Some(name) = edit.add_column(cx) {
                stage(edit, &name, Field::Name, "region", cx);
                stage(edit, "region", Field::Type, "varchar(40)", cx);
                stage(edit, "region", Field::Default, "'EU'", cx);
                stage(edit, "region", Field::Comment, "Sales region", cx);
            }
            // An index on the renamed column and a foreign key from the added one.
            let table = edit.table.clone();
            let ref_table = edit.context.scope.table_id(&edit.context.scope.database, edit.context.scope.schema.as_deref(), "regions");
            let index = crate::sql_ddl::IndexSpec { name: "idx_orders_order_status".into(), columns: vec!["order_status".into()], unique: false };
            edit.edit_draft(cx, |draft| draft.push(Change::CreateIndex { table: table.clone(), index }));
            let foreign_key = crate::sql_ddl::ForeignKeySpec {
                name: "fk_orders_region".into(),
                columns: vec!["region".into()],
                ref_table,
                ref_columns: vec!["code".into()],
                on_delete: Some(crate::sql_ddl::FkAction::SetNull),
                on_update: None,
            };
            edit.edit_draft(cx, |draft| draft.push(Change::AddForeignKey { table, foreign_key }));
            if wants("type") {
                if let Some(row) = edit.rows.iter().position(|row| row.spec.name == "payload") {
                    edit.begin_edit(row, Field::Type, window, cx);
                }
            } else {
                edit.selected = Some("order_status".into());
            }
        });
        let sub_tab = if wants("indexes") {
            Some(PropertiesTab::Indexes)
        } else {
            wants("foreign-keys").then_some(PropertiesTab::ForeignKeys)
        };
        if let Some(runtime) = sub_tab.and_then(|tab| self.workspace.runtime_mut(tab_id).map(|runtime| (runtime, tab))) {
            runtime.0.columns_panel.properties_tab = runtime.1;
        }
        if wants("review") {
            self.open_properties_changes(tab_id, window, cx);
        }
    }

    /// The tabs that show the table whose structure changed reload its columns and their result.
    pub(in crate::app) fn refresh_table_tabs_after_apply(&mut self, origin: &TableOrigin, cx: &mut Context<Self>) {
        let TableOrigin { connection, database, schema, object } = origin.clone();
        let ids: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.query_mode.is_table_data()
                    && tab.connection_id == connection
                    && tab.database == database
                    && tab.schema.as_deref().unwrap_or_default() == schema
                    && tab.collection == object
            })
            .map(|tab| tab.id)
            .collect();
        for id in ids {
            self.load_table_structure(id, &connection, database.clone(), schema.clone(), object.clone(), cx);
            // A grid with staged edits keeps them: its result is refreshed by the user.
            if !self.workspace.tab_has_staged_changes(id) {
                self.rerun_shown_result(id, false, cx);
            }
        }
    }

    // ---- row actions ----

    fn select_properties_row(&mut self, tab_id: u64, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.view_mut(tab_id).columns_focus(cx).focus(window, cx);
        self.with_properties_edit(tab_id, cx, |edit, cx| {
            edit.commit_edit(Exit::Blur, window, cx);
            edit.select(Some(name.to_owned()), cx);
        });
    }

    /// A click on a text cell of row `row` (an index into the rows): selects the column and opens
    /// the cell.
    fn edit_properties_cell(&mut self, tab_id: u64, row: usize, name: &str, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| {
            // The row may have moved since it was drawn: the name says which one was meant.
            let row = Some(row).filter(|&row| edit.rows.get(row).is_some_and(|r| r.spec.name == name)).or_else(|| edit.rows.iter().position(|r| r.spec.name == name));
            if let Some(row) = row {
                edit.begin_edit(row, field, window, cx);
            }
        });
    }

    /// The Nullable box of column `name` (the real one of the selected row, or the drawn one of any
    /// other row).
    fn set_row_nullable(&mut self, tab_id: u64, name: &str, nullable: bool, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| edit.set_nullable(name, nullable, cx));
    }

    fn set_row_key(&mut self, tab_id: u64, name: &str, key: bool, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| edit.set_primary_key(name, key, cx));
    }

    /// The action of the row `name`: Restore on a dropped column, Drop (Remove on a new one) else.
    fn row_action(&mut self, tab_id: u64, name: &str, dropped: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| match dropped {
            true => edit.restore_column(name, cx),
            false => edit.drop_column(name, cx),
        });
        self.refocus_grid(tab_id, window, cx);
    }

    /// The real controls live on the selected row only: when the selection moves, or the row goes
    /// (Remove), a control that had the focus goes with it and the window would be left without
    /// one. The focus returns to the grid then, so the keys keep working. The filter box and the
    /// open editor keep theirs.
    fn refocus_grid(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let grid = self.view_mut(tab_id).columns_focus(cx);
        let filter = self.columns_filter_inputs.get(&tab_id).is_some_and(|input| input.read(cx).focus_handle(cx).contains_focused(window, cx));
        let editing = self.properties_edits.get(&tab_id).is_some_and(|edit| edit.read(cx).editing().is_some());
        if grid.contains_focused(window, cx) && !grid.is_focused(window) && !filter && !editing {
            grid.focus(window, cx);
        }
    }

    /// The header, the rows and the title of the Columns grid in edit mode.
    pub(super) fn edit_columns_body(&self, tab_id: u64, state: EditRender, measured: ColumnWidths, cx: &mut Context<Self>) -> EditColumnsBody {
        let widths = EditWidths::new(measured);
        let table_width = widths.min_total();
        let title = if state.filtered.len() == state.rows.len() {
            format!("Columns ({})", state.rows.len())
        } else {
            format!("Columns ({}/{})", state.filtered.len(), state.rows.len())
        };
        let header = div()
            .debug_selector(|| "columns-table-header".into())
            .h(px(HEADER_ROW_HEIGHT))
            .w_full()
            .min_w(px(table_width))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .child(grid_cell(widths.marker, true))
            .child(flex_cell(widths.name, true).child("Name"))
            .child(flex_cell(widths.type_name, true).child("Type"))
            .child(grid_cell(widths.nullable, true).child("Nullable"))
            .child(grid_cell(widths.key, true).child("Key"))
            .child(flex_cell(widths.default, true).child("Default"))
            .child(flex_cell(widths.comment, true).child("Comment"))
            .child(grid_cell(widths.action, true));
        let empty = state.filtered.is_empty();
        let list = uniform_list(
            ("columns-view-list", tab_id),
            state.filtered.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                let look = StaticControls::new(cx);
                range.map(|position| edit_row(tab_id, position, &state, &widths, table_width, &look, cx)).collect()
            }),
        )
        .track_scroll(&self.view(tab_id).columns_scroll)
        .flex_1()
        .min_h_0()
        .into_any_element();
        EditColumnsBody { title, header, list, empty }
    }
}

/// How the boxes and the action label of a row without real controls are drawn.
///
/// The official `Checkbox` and `Button` make a keyed focus handle and a dozen elements each, on
/// every row of every frame. Only the selected row gets them (focus, Tab, Space and tooltips work
/// there as ever); the other rows draw the same look from the same theme tokens and take the click
/// themselves, which also selects the row as before. Read once per frame, not per row.
struct StaticControls {
    box_radius: gpui::Pixels,
    box_fill: gpui::Hsla,
    box_border: gpui::Hsla,
    box_border_off: gpui::Hsla,
    checked_fill: gpui::Background,
    checked_border: gpui::Hsla,
    checked_off: gpui::Hsla,
    mark: gpui::Hsla,
    mark_off: gpui::Hsla,
    action_radius: gpui::Pixels,
    action_text: gpui::Hsla,
    action_text_off: gpui::Hsla,
    action_hover_fill: gpui::Background,
    action_hover_text: gpui::Hsla,
    action_active_fill: gpui::Background,
}

impl StaticControls {
    fn new(cx: &gpui::App) -> Self {
        use gpui_component::ActiveTheme as _;
        let theme = cx.theme();
        let hover_fill: gpui::Background = theme.tokens.accent.into();
        Self {
            box_radius: theme.radius.min(px(4.0)),
            box_fill: theme.input_background(),
            box_border: theme.input,
            box_border_off: theme.input.opacity(0.5),
            checked_fill: theme.tokens.primary.into(),
            checked_border: theme.primary,
            checked_off: theme.primary.opacity(0.5),
            mark: theme.primary_foreground,
            mark_off: theme.primary_foreground.opacity(0.5),
            action_radius: theme.radius,
            action_text: theme.secondary_foreground,
            action_text_off: theme.muted_foreground.opacity(0.5),
            action_hover_fill: if theme.mode.is_dark() { hover_fill.opacity(0.5) } else { hover_fill },
            action_hover_text: theme.accent_foreground,
            action_active_fill: theme.tokens.button_active.into(),
        }
    }

    /// A check box as the official `Checkbox` draws its indicator (`off`: disabled).
    fn check_box(&self, id: (&'static str, usize), checked: bool, off: bool) -> gpui::Stateful<Div> {
        let (fill, border) = match (checked, off) {
            (false, false) => (self.box_fill.into(), self.box_border),
            (false, true) => (self.box_fill.into(), self.box_border_off),
            (true, false) => (self.checked_fill, self.checked_border),
            (true, true) => (self.checked_off.into(), self.checked_off),
        };
        div()
            .id(id)
            .relative()
            .flex_shrink_0()
            .size(gpui::rems(1.0))
            .border_1()
            .rounded(self.box_radius)
            .border_color(border)
            .bg(fill)
            .when(checked, |this| {
                this.child(gpui::svg().absolute().top_px().left_px().size_3().text_color(if off { self.mark_off } else { self.mark }).path("icons/check.svg"))
            })
    }

    /// The label of a ghost, extra small, compact `Button` (`off`: disabled).
    fn action(&self, id: (&'static str, usize), label: &'static str, off: bool) -> gpui::Stateful<Div> {
        div()
            .id(id)
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .h_5()
            .min_w_5()
            .px_1()
            .rounded(self.action_radius)
            .text_xs()
            .whitespace_nowrap()
            .line_height(gpui::relative(1.25))
            .cursor_default()
            // As the official disabled Button: the press is not a click on the row either.
            .when(off, |this| this.text_color(self.action_text_off).on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation()))
            .when(!off, |this| {
                this.text_color(self.action_text)
                    .hover(|style| style.bg(self.action_hover_fill).text_color(self.action_hover_text))
                    .active(|style| style.bg(self.action_active_fill))
            })
            .child(label)
    }
}

/// The tooltip that says why a control is off. The official `Checkbox` shows its reason through the
/// component's managed tooltip, which is private to gpui-component (it adds a slide and fade-in);
/// this one has the same delay (`TooltipDefaults`) and the same content and frame, and carries a
/// debug selector so that tests can see it.
fn blocked_tooltip(reason: SharedString) -> impl Fn(&mut Window, &mut gpui::App) -> gpui::AnyView + 'static {
    move |window, cx| {
        let reason = reason.clone();
        gpui_component::tooltip::Tooltip::element(move |_, _| div().debug_selector(|| "properties-edit-tooltip".into()).child(reason.clone())).build(window, cx)
    }
}

/// One visible row of the Columns grid in edit mode.
fn edit_row(tab_id: u64, position: usize, state: &EditRender, widths: &EditWidths, table_width: f32, look: &StaticControls, cx: &mut Context<AppRoot>) -> AnyElement {
    let index = state.filtered[position];
    let row = &state.rows[index];
    let selected = state.selected.as_deref() == Some(row.spec.name.as_str());
    let dropped = row.kind == RowKind::Dropped;
    let error = state.errors.get(&row.spec.name).cloned();
    let marker_color = match row.kind {
        _ if error.is_some() => theme::danger(),
        RowKind::New => theme::success(),
        RowKind::Changed => theme::warning(),
        RowKind::Dropped => theme::danger(),
        RowKind::Existing => theme::text_muted(),
    };
    // One copy of the name for every listener of the row.
    let name: Rc<str> = Rc::from(row.spec.name.as_str());
    // A Nullable or Key cell. The selected row has the real `Checkbox`; the others draw the box and
    // toggle on the click (the cell's own listener then selects the row, as it does for the real one).
    let boolean = |id: &'static str, drawn_id: &'static str, checked: bool, block: Option<SharedString>, set: fn(&mut AppRoot, u64, &str, bool, &mut Context<AppRoot>), cx: &mut Context<AppRoot>| {
        let name = name.clone();
        let selector = move || format!("{id}-{position}");
        if selected {
            let checkbox = Checkbox::new((id, position))
                .checked(checked)
                .disabled(block.is_some())
                .when_some(block, |checkbox, reason| checkbox.tooltip(reason))
                .on_click(cx.listener(move |this, checked: &bool, _, cx| set(this, tab_id, &name, *checked, cx)));
            return div().debug_selector(selector).child(checkbox).into_any_element();
        }
        let drawn = look.check_box((drawn_id, position), checked, block.is_some()).debug_selector(selector);
        match block {
            Some(reason) => drawn.tooltip(blocked_tooltip(reason)).tooltip_show_delay(gpui_component::tooltip::TooltipDefaults::global(cx).show_delay()),
            None => drawn.on_click(cx.listener(move |this, _, _, cx| set(this, tab_id, &name, !checked, cx))),
        }
        .into_any_element()
    };
    let nullable = boolean("properties-edit-nullable", "properties-edit-nullable-box", row.nullable(), state.nullable_block(row), AppRoot::set_row_nullable, cx);
    let key = boolean("properties-edit-key", "properties-edit-key-box", row.primary_key, state.key_block(row), AppRoot::set_row_key, cx);
    let (action_label, action_selector) = match row.kind {
        RowKind::Dropped => ("Restore", "properties-edit-restore"),
        RowKind::New => ("Remove", "properties-edit-remove"),
        _ => ("Drop", "properties-edit-drop"),
    };
    let editable = state.gate.blocked.is_none();
    let action = {
        let name = name.clone();
        let on_click = cx.listener(move |this, _, window, cx| this.row_action(tab_id, &name, dropped, window, cx));
        let selector = move || format!("{action_selector}-{position}");
        if selected {
            let button = Button::new(("properties-edit-action", position))
                .label(action_label)
                .with_size(Size::XSmall)
                .compact()
                .ghost()
                .disabled(!editable)
                .on_click(on_click);
            div().debug_selector(selector).child(button).into_any_element()
        } else {
            look.action(("properties-edit-action-label", position), action_label, !editable).debug_selector(selector).when(editable, |this| this.on_click(on_click)).into_any_element()
        }
    };
    let text_cell = |field: Field, width: Flex, color: gpui::Rgba, cx: &mut Context<AppRoot>| {
        let editing_here = state.editing == Some((index, field));
        let block = state.field_block(row, field);
        let cell = flex_cell(width, false)
            .id(("properties-edit-cell", position * Field::ALL.len() + field.index()))
            .debug_selector(move || format!("properties-edit-{}-{position}", field.slug()));
        if editing_here {
            // A click in the open editor, or a drag that starts in it and ends on another cell, is
            // not a click on the row: the row would commit the editor from under the selection.
            return cell
                .border_1()
                .border_color(theme::accent())
                .bg(theme::background())
                .p_0()
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(editor_cell(tab_id, state, cx));
        }
        let active = selected && state.field == field && state.editing.is_none();
        let open_name = name.clone();
        let value = field.text_str(&row.spec);
        let value = if value.is_empty() { SharedString::new_static("") } else { SharedString::from(value) };
        cell.when(active, |this| this.border_1().border_color(theme::accent()))
            .when_some(block.clone(), |this, reason| this.tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(reason.clone()).build(window, cx)))
            // The click opens the cell; it does not also select the row a second time.
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.edit_properties_cell(tab_id, index, &open_name, field, window, cx)
            }))
            .when(!value.is_empty(), |this| {
                this.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(color)
                        .when(block.is_some(), |this| this.opacity(0.7))
                        .when(dropped && field == Field::Name, |this| this.line_through())
                        .child(value),
                )
            })
    };
    let marker = grid_cell(widths.marker, false)
        .justify_center()
        .debug_selector(move || format!("properties-edit-marker-{position}"))
        .text_color(marker_color)
        .font_weight(gpui::FontWeight::BOLD)
        .child(if error.is_some() { "!" } else { row.kind.marker() });
    // Only a cell with a tooltip needs an id.
    let marker = match error {
        Some(message) => marker.id(("properties-edit-marker-cell", position)).tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(message.clone()).build(window, cx)).into_any_element(),
        None => marker.into_any_element(),
    };
    let name_cell = text_cell(Field::Name, widths.name, theme::text(), cx);
    let type_cell = text_cell(Field::Type, widths.type_name, grid_column_type_color(&row.spec.data_type), cx);
    let default_cell = text_cell(Field::Default, widths.default, theme::text_muted(), cx);
    let comment_cell = text_cell(Field::Comment, widths.comment, theme::text_muted(), cx);
    let nullable_cell = grid_cell(widths.nullable, false).child(nullable);
    let key_cell = grid_cell(widths.key, false).child(key);
    let action_cell = grid_cell(widths.action, false).child(action);
    // A click anywhere on the row selects it, except on a text cell (which opens).
    let select_name = name.clone();
    div()
        .id(("properties-edit-row", position))
        .debug_selector(move || format!("properties-edit-row-{position}"))
        .h(px(ROW_HEIGHT))
        .w_full()
        .min_w(px(table_width))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .cursor_pointer()
        .when(dropped, |this| this.opacity(0.55))
        .when(selected, |this| this.bg(theme::accent_surface()))
        .when(!selected, |this| this.hover(|style| style.bg(theme::hover())))
        .border_b_1()
        .border_color(theme::divider())
        .on_click(cx.listener(move |this, _, window, cx| this.select_properties_row(tab_id, &select_name, window, cx)))
        .child(marker)
        .child(name_cell)
        .child(type_cell)
        .child(nullable_cell)
        .child(key_cell)
        .child(default_cell)
        .child(comment_cell)
        .child(action_cell)
        .into_any_element()
}

/// The open cell: the tab's one editor. Enter, Escape and Tab are taken before the editor sees
/// them, except while its completion menu is open (Enter picks the highlighted type, Escape closes
/// the menu).
fn editor_cell(tab_id: u64, state: &EditRender, cx: &mut Context<AppRoot>) -> AnyElement {
    let (for_enter, for_escape) = (state.editor.clone(), state.editor.clone());
    div()
        .debug_selector(|| "properties-edit-editor".into())
        .size_full()
        .min_w_0()
        .flex()
        .items_center()
        .capture_action(cx.listener(move |this, _: &InputEnter, window, cx| {
            if !for_enter.read(cx).completion_menu_state().open {
                cx.stop_propagation();
                this.properties_cell_key(tab_id, CellKey::Enter, window, cx);
            }
        }))
        .capture_action(cx.listener(move |this, _: &InputEscape, window, cx| {
            if !for_escape.read(cx).completion_menu_state().open {
                cx.stop_propagation();
                this.properties_cell_key(tab_id, CellKey::Escape, window, cx);
            }
        }))
        .capture_action(cx.listener(move |this, _: &IndentInline, window, cx| {
            cx.stop_propagation();
            this.properties_cell_key(tab_id, CellKey::Tab(1), window, cx);
        }))
        .capture_action(cx.listener(move |this, _: &OutdentInline, window, cx| {
            cx.stop_propagation();
            this.properties_cell_key(tab_id, CellKey::Tab(-1), window, cx);
        }))
        // The editor lays out a line plus its own 8px padding top and bottom (and two rows when no height is
        // given): 34px, centered in the 26px cell, puts the line where the plain cells' text is.
        .child(Editor::new(&state.editor).appearance(false).bordered(false).text_size(px(12.0)).h(px(34.0)))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogColumn as TableColumn, CatalogConstraint, CatalogConstraintKind};

    fn column(name: &str, nullable: bool, primary_key: bool) -> TableColumn {
        TableColumn { name: name.into(), data_type: "text".into(), nullable, primary_key, ..TableColumn::default() }
    }

    fn described() -> TableProperties {
        TableProperties {
            columns: vec![column("id", false, true), column("name", false, false), column("note", true, false)],
            keys: Some(vec![CatalogConstraint {
                name: "t_pkey".into(),
                kind: CatalogConstraintKind::PrimaryKey,
                columns: vec!["id".into()],
                definition: None,
            }]),
            ..TableProperties::default()
        }
    }

    fn table() -> TableId {
        TableId { schema: Some("public".into()), name: "t".into() }
    }

    fn rows(draft: Vec<Change>) -> Vec<(String, RowKind, bool, bool)> {
        let properties = described();
        let live = live::live_table(&properties);
        build_rows(&properties, &live, &Draft::from_changes(draft), &table())
            .into_iter()
            .map(|row| (row.spec.name.clone(), row.kind, row.nullable(), row.primary_key))
            .collect()
    }

    #[test]
    fn rows_without_a_draft_are_the_described_columns() {
        assert_eq!(
            rows(vec![]),
            [
                ("id".into(), RowKind::Existing, false, true),
                ("name".into(), RowKind::Existing, false, false),
                ("note".into(), RowKind::Existing, true, false),
            ]
        );
    }

    #[test]
    fn the_draft_marks_new_changed_and_dropped_rows() {
        let spec = |name: &str, nullable: bool| ColumnSpec { name: name.into(), data_type: "text".into(), nullable, ..ColumnSpec::default() };
        let got = rows(vec![
            Change::AddColumn { table: table(), column: spec("extra", true) },
            Change::AlterColumn { table: table(), from: spec("note", true), to: spec("note", false) },
            Change::DropColumn { table: table(), column: "name".into() },
            Change::SetPrimaryKey {
                table: table(),
                columns: vec!["id".into(), "extra".into()],
                previous: Some(PrimaryKeyRef { name: Some("t_pkey".into()), columns: vec!["id".into()] }),
            },
        ]);
        assert_eq!(
            got,
            [
                ("id".into(), RowKind::Existing, false, true),
                ("name".into(), RowKind::Dropped, false, false),
                ("note".into(), RowKind::Changed, false, false),
                // A key column holds no NULL.
                ("extra".into(), RowKind::New, false, true),
            ]
        );
    }

    #[test]
    fn a_change_of_another_table_is_not_shown() {
        let other = TableId { schema: Some("public".into()), name: "other".into() };
        assert_eq!(rows(vec![Change::DropColumn { table: other, column: "note".into() }]), rows(vec![]));
    }

    #[test]
    fn the_properties_wording_never_speaks_of_the_diagram() {
        for text in [Wording::PROPERTIES.refresh, Wording::PROPERTIES.empty] {
            assert!(!text.to_lowercase().contains("diagram") && !text.contains("Inspector"), "{text}");
        }
        assert_eq!(Wording::DIAGRAM.refresh, "refresh the diagram");
        assert_eq!(
            Wording::DIAGRAM.empty,
            "No pending changes. Edit a table in the Inspector, or right click the canvas > New table."
        );
    }
}
