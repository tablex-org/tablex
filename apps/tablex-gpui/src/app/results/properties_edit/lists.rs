//! Indexes, Foreign keys and Keys of the Properties edit mode (R3-3).
//!
//! The rows of the two list sub-tabs are the described ones with what the draft does to them
//! (dropped, struck through and restorable; new, with `+` and a Remove), computed once per change
//! of the draft ([`EditLists`]); the grids stay one virtualized `uniform_list` each. A new index or
//! foreign key is made in a small form above the grid: its Name `Input`, a multi-select
//! `Combobox` for the columns (the official control that keeps the order the columns were
//! picked in), a `Select` for the referenced table and the ON DELETE / ON UPDATE actions. The
//! form's entities exist while it is open and are dropped on close; nothing is created per row.
//! Submitting the form pushes the change into the draft (a new row), as everything else.
//!
//! The Keys sub-tab keeps its read-only list and tells what the draft does to the primary key
//! ([`PendingKey`]).

use gpui::{SharedString, Subscription};
use gpui_component::combobox::{Combobox, ComboboxEvent, ComboboxState};
use gpui_component::searchable_list::SearchableVec;
use gpui_component::select::{Select, SelectEvent, SelectState};

use super::*;
use crate::app::structure_edit::specs;
use crate::columns_panel::PropertiesTab;
use crate::sql_ddl::{DdlError, FkAction, ForeignKeySpec, IndexSpec};

/// More entries than this and the pickers get a search box.
const SEARCHABLE_FROM: usize = 10;

pub(super) type Names = SearchableVec<SharedString>;
pub(in crate::app) type ColumnCombobox = gpui::Entity<ComboboxState<Names>>;
type ActionSelect = gpui::Entity<SelectState<Vec<SharedString>>>;
type TableSelect = gpui::Entity<SelectState<Names>>;

// ---- rows ----------------------------------------------------------------------------------------

/// One row of the Indexes or Foreign keys grid in edit mode.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct ListRow {
    pub(in crate::app) kind: RowKind,
    /// The name of the index / foreign key (cells[0]).
    pub(in crate::app) name: String,
    pub(in crate::app) cells: Vec<SharedString>,
    /// The index behind the primary key: it goes with the key, not on its own.
    pub(in crate::app) primary: bool,
}

fn join(columns: &[String]) -> String {
    columns.join(", ")
}

fn yes_no(on: bool) -> &'static str {
    if on {
        "Yes"
    } else {
        "No"
    }
}

fn action_text(action: Option<FkAction>) -> String {
    action.map(|action| action.sql().to_owned()).unwrap_or_default()
}

/// The Indexes grid: the described indexes (dropped ones marked), then the ones the draft creates.
pub(in crate::app) fn build_index_rows(properties: &TableProperties, draft: &Draft, table: &TableId) -> Vec<ListRow> {
    let dropped: HashSet<&str> = draft
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::DropIndex { table: t, name } if t == table => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let described = properties.indexes.iter().flatten().map(|index| ListRow {
        kind: if dropped.contains(index.name.as_str()) { RowKind::Dropped } else { RowKind::Existing },
        name: index.name.clone(),
        cells: vec![
            index.name.clone().into(),
            join(&index.columns).into(),
            yes_no(index.unique).into(),
            index.method.clone().unwrap_or_default().into(),
        ],
        primary: index.primary,
    });
    let created = draft.changes.iter().filter_map(|change| match change {
        Change::CreateIndex { table: t, index } if t == table => Some(ListRow {
            kind: RowKind::New,
            name: index.name.clone(),
            cells: vec![index.name.clone().into(), join(&index.columns).into(), yes_no(index.unique).into(), SharedString::default()],
            primary: false,
        }),
        _ => None,
    });
    described.chain(created).collect()
}

/// The Foreign keys grid: the described keys (dropped ones marked), then the ones the draft adds.
pub(in crate::app) fn build_foreign_key_rows(properties: &TableProperties, draft: &Draft, table: &TableId) -> Vec<ListRow> {
    let dropped: HashSet<&str> = draft
        .changes
        .iter()
        .filter_map(|change| match change {
            Change::DropForeignKey { table: t, name } if t == table => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let described = properties.foreign_keys.iter().flatten().map(|fk| ListRow {
        kind: if dropped.contains(fk.name.as_str()) { RowKind::Dropped } else { RowKind::Existing },
        name: fk.name.clone(),
        cells: vec![
            fk.name.clone().into(),
            join(&fk.columns).into(),
            format!("{}({})", fk.referenced_table, join(&fk.referenced_columns)).into(),
            fk.on_update.clone().unwrap_or_default().into(),
            fk.on_delete.clone().unwrap_or_default().into(),
        ],
        primary: false,
    });
    let added = draft.changes.iter().filter_map(|change| match change {
        Change::AddForeignKey { table: t, foreign_key } if t == table => Some(ListRow {
            kind: RowKind::New,
            name: foreign_key.name.clone(),
            cells: vec![
                foreign_key.name.clone().into(),
                join(&foreign_key.columns).into(),
                format!("{}({})", foreign_key.ref_table, join(&foreign_key.ref_columns)).into(),
                action_text(foreign_key.on_update).into(),
                action_text(foreign_key.on_delete).into(),
            ],
            primary: false,
        }),
        _ => None,
    });
    described.chain(added).collect()
}

/// What the draft does to the primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct PendingKey {
    pub(in crate::app) before: Vec<String>,
    pub(in crate::app) after: Vec<String>,
}

impl PendingKey {
    pub(in crate::app) fn added(&self) -> impl Iterator<Item = &String> {
        self.after.iter().filter(|column| !self.before.contains(column))
    }

    pub(in crate::app) fn removed(&self) -> impl Iterator<Item = &String> {
        self.before.iter().filter(|column| !self.after.contains(column))
    }

    /// One line for the Keys sub-tab ("Pending: primary key (id, code); adds code").
    pub(in crate::app) fn summary(&self) -> String {
        let list = |columns: &[String]| format!("({})", join(columns));
        let mut text = match (self.before.is_empty(), self.after.is_empty()) {
            (_, true) => format!("Pending: the primary key {} is dropped", list(&self.before)),
            (true, false) => format!("Pending: new primary key {}", list(&self.after)),
            (false, false) => format!("Pending: the primary key {} becomes {}", list(&self.before), list(&self.after)),
        };
        let (added, removed): (Vec<String>, Vec<String>) = (self.added().cloned().collect(), self.removed().cloned().collect());
        if !added.is_empty() && !self.before.is_empty() {
            text.push_str(&format!("; adds {}", join(&added)));
        }
        if !removed.is_empty() && !self.after.is_empty() {
            text.push_str(&format!("; removes {}", join(&removed)));
        }
        text
    }
}

/// The primary key the draft sets, against the one the catalog reports.
pub(super) fn pending_key(live: &LiveTable, draft: &Draft, table: &TableId) -> Option<PendingKey> {
    draft.changes.iter().find_map(|change| match change {
        Change::SetPrimaryKey { table: t, columns, .. } if t == table => Some(PendingKey {
            before: live.primary_key.as_ref().map(|key| key.columns.clone()).unwrap_or_default(),
            after: columns.clone(),
        }),
        _ => None,
    })
}

/// Everything the list sub-tabs derive from the draft.
#[derive(Debug, Default)]
pub(in crate::app) struct EditLists {
    pub(in crate::app) indexes: Vec<ListRow>,
    pub(in crate::app) foreign_keys: Vec<ListRow>,
    /// Why each row cannot be dropped (parallel to the rows).
    pub(in crate::app) index_blocks: Vec<Option<String>>,
    pub(in crate::app) foreign_key_blocks: Vec<Option<String>>,
    /// Why a new index / foreign key cannot be added now.
    pub(in crate::app) index_add_block: Option<String>,
    pub(in crate::app) foreign_key_add_block: Option<String>,
    /// The table names could not be loaded (the FK tab offers Retry) / are still being loaded.
    pub(in crate::app) catalog_failed: bool,
    pub(in crate::app) catalog_loading: bool,
    pub(in crate::app) key: Option<PendingKey>,
}

impl EditLists {
    pub(in crate::app) fn rows(&self, tab: PropertiesTab) -> &[ListRow] {
        match tab {
            PropertiesTab::Indexes => &self.indexes,
            _ => &self.foreign_keys,
        }
    }

    pub(in crate::app) fn blocks(&self, tab: PropertiesTab) -> &[Option<String>] {
        match tab {
            PropertiesTab::Indexes => &self.index_blocks,
            _ => &self.foreign_key_blocks,
        }
    }
}

/// The message of the first statement that did not generate, per new index / foreign key it is about.
pub(super) fn list_errors(changes: &[Change], planned: &Planned) -> HashMap<(PropertiesTab, String), String> {
    let mut errors = HashMap::new();
    for entry in &planned.entries {
        let Err(error) = &entry.statements else { continue };
        let key = match &changes[entry.change] {
            Change::CreateIndex { index, .. } => (PropertiesTab::Indexes, index.name.clone()),
            Change::AddForeignKey { foreign_key, .. } => (PropertiesTab::ForeignKeys, foreign_key.name.clone()),
            _ => continue,
        };
        errors.entry(key).or_insert_with(|| error.to_string());
    }
    errors
}

/// The rows of `rows` whose cells contain `needle` (lower case; empty: all).
pub(super) fn filter_rows(rows: &[ListRow], needle: &str) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, row)| needle.is_empty() || row.cells.iter().any(|cell| cell.to_lowercase().contains(needle)))
        .map(|(index, _)| index)
        .collect()
}

// ---- the forms -----------------------------------------------------------------------------------

/// The referenced columns of the foreign key form.
pub(super) enum Referenced {
    /// No table chosen yet.
    Nothing,
    /// The table is being described.
    Loading(String),
    Failed { table: String, error: String },
    /// Described; the picker is made when the window is at hand (`sync_form`).
    Loaded { table: String, names: Vec<String>, picker: Option<ColumnCombobox> },
}

pub(super) struct IndexForm {
    name: gpui::Entity<InputState>,
    columns: ColumnCombobox,
    unique: bool,
    /// The generated name the Name input shows until the user types another.
    auto_name: String,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

pub(super) struct FkForm {
    name: gpui::Entity<InputState>,
    columns: ColumnCombobox,
    table: TableSelect,
    referenced: Referenced,
    on_delete: ActionSelect,
    on_update: ActionSelect,
    auto_name: String,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

pub(super) enum Form {
    Index(IndexForm),
    ForeignKey(FkForm),
}

/// What the grid's panel needs to draw a form (cheap to clone).
#[derive(Clone)]
pub(in crate::app) enum FormRender {
    Index { name: gpui::Entity<InputState>, columns: ColumnCombobox, unique: bool, error: Option<String> },
    ForeignKey {
        name: gpui::Entity<InputState>,
        columns: ColumnCombobox,
        table: TableSelect,
        referenced: ReferencedRender,
        on_delete: ActionSelect,
        on_update: ActionSelect,
        error: Option<String>,
    },
}

#[derive(Clone)]
pub(in crate::app) enum ReferencedRender {
    Nothing,
    Loading(String),
    Failed(String),
    Picker(ColumnCombobox),
}

impl Form {
    fn render(&self) -> FormRender {
        match self {
            Self::Index(form) => FormRender::Index {
                name: form.name.clone(),
                columns: form.columns.clone(),
                unique: form.unique,
                error: form.error.clone(),
            },
            Self::ForeignKey(form) => FormRender::ForeignKey {
                name: form.name.clone(),
                columns: form.columns.clone(),
                table: form.table.clone(),
                referenced: match &form.referenced {
                    Referenced::Nothing => ReferencedRender::Nothing,
                    Referenced::Loading(table) | Referenced::Loaded { table, picker: None, .. } => ReferencedRender::Loading(table.clone()),
                    Referenced::Failed { table, error } => ReferencedRender::Failed(format!("Could not describe {table}: {error}")),
                    Referenced::Loaded { picker: Some(picker), .. } => ReferencedRender::Picker(picker.clone()),
                },
                on_delete: form.on_delete.clone(),
                on_update: form.on_update.clone(),
                error: form.error.clone(),
            },
        }
    }
}

fn names(items: impl IntoIterator<Item = String>) -> Names {
    SearchableVec::new(items.into_iter().map(SharedString::from).collect::<Vec<_>>())
}

fn multi_picker(items: Vec<String>, window: &mut Window, cx: &mut Context<PropertiesEdit>) -> ColumnCombobox {
    let searchable = items.len() > SEARCHABLE_FROM;
    cx.new(|cx| ComboboxState::new(names(items), Vec::new(), window, cx).multiple(true).searchable(searchable))
}

fn action_select(window: &mut Window, cx: &mut Context<PropertiesEdit>) -> ActionSelect {
    cx.new(|cx| SelectState::new(specs::fk_action_labels(), Some(gpui_component::IndexPath::default().row(0)), window, cx))
}

fn selected(select: &ActionSelect, cx: &gpui::App) -> Option<FkAction> {
    specs::fk_action_at(select.read(cx).selected_index(cx).map_or(0, |index| index.row))
}

fn picked(columns: &ColumnCombobox, cx: &gpui::App) -> Vec<String> {
    columns.read(cx).selected_values().iter().map(ToString::to_string).collect()
}

impl PropertiesEdit {
    // ---- what the grids and the toolbar read ----

    /// A pending change the dialect refuses, as its words say it (`Unsupported` only).
    fn refusal(&self, change: Change) -> Option<String> {
        Self::refusal_in(&*self.context.dialect(), change)
    }

    fn refusal_in(dialect: &dyn crate::sql_ddl::DdlDialect, change: Change) -> Option<String> {
        match dialect.statements(&change, &[]) {
            Err(DdlError::Unsupported(reason)) => Some(reason),
            _ => None,
        }
    }

    /// Why a new index cannot be added now, if it cannot.
    pub(super) fn index_block(&self) -> Option<String> {
        if let Some(reason) = &self.context.disabled {
            return Some(reason.clone());
        }
        if !self.context.scope.dialect().capabilities().indexes {
            let probe = Change::CreateIndex {
                table: self.table.clone(),
                index: IndexSpec { name: "new_index".into(), columns: vec!["column".into()], unique: false },
            };
            return Some(self.refusal(probe).unwrap_or_else(|| format!("{} cannot add indexes.", self.context.scope.dialect().label())));
        }
        (!self.has_column_choice()).then(|| "The table has no column to index.".to_owned())
    }

    /// Why a new foreign key cannot be added now, if it cannot.
    pub(super) fn foreign_key_block(&self) -> Option<String> {
        if let Some(reason) = &self.context.disabled {
            return Some(reason.clone());
        }
        if !self.context.scope.dialect().capabilities().foreign_keys {
            let probe = Change::AddForeignKey {
                table: self.table.clone(),
                foreign_key: ForeignKeySpec {
                    name: "new_fk".into(),
                    columns: vec!["column".into()],
                    ref_table: self.table.clone(),
                    ref_columns: vec!["column".into()],
                    ..ForeignKeySpec::default()
                },
            };
            return Some(self.refusal(probe).unwrap_or_else(|| format!("{} cannot add foreign keys.", self.context.scope.dialect().label())));
        }
        match &self.context.catalog {
            None => Some(match &self.context.catalog_error {
                Some(error) => format!("Could not load the tables of this schema: {error}"),
                None => "Loading the tables of this schema\u{2026}".into(),
            }),
            Some(tables) if tables.is_empty() => Some("The schema lists no table to reference.".into()),
            Some(_) => (!self.has_column_choice()).then(|| "The table has no column to reference from.".to_owned()),
        }
    }

    /// Why row `row` of `tab` cannot be dropped, if it cannot.
    pub(in crate::app) fn row_block(&self, tab: PropertiesTab, row: &ListRow) -> Option<String> {
        self.row_block_in(&*self.context.dialect(), tab, row)
    }

    /// [`Self::row_block`] with the dialect of the context, for a caller that asks about many rows.
    pub(in crate::app) fn row_block_in(&self, dialect: &dyn crate::sql_ddl::DdlDialect, tab: PropertiesTab, row: &ListRow) -> Option<String> {
        if let Some(reason) = &self.context.disabled {
            return Some(reason.clone());
        }
        if row.kind == RowKind::New {
            return None;
        }
        match tab {
            PropertiesTab::Indexes if row.primary => Some("This index is the primary key: change the key on the Columns tab.".into()),
            PropertiesTab::Indexes => Self::refusal_in(dialect, Change::DropIndex { table: self.table.clone(), name: row.name.clone() }),
            _ => Self::refusal_in(dialect, Change::DropForeignKey { table: self.table.clone(), name: row.name.clone() }),
        }
    }

    /// Whether the draft leaves the table any column.
    fn has_column_choice(&self) -> bool {
        self.rows.iter().any(|row| row.kind != RowKind::Dropped)
    }

    /// The columns of the table as the draft leaves them.
    fn column_choices(&self) -> Vec<String> {
        self.rows.iter().filter(|row| row.kind != RowKind::Dropped).map(|row| row.spec.name.clone()).collect()
    }

    pub(super) fn form_render(&self) -> Option<FormRender> {
        self.form.as_ref().map(Form::render)
    }

    #[cfg(test)]
    pub(in crate::app) fn lists(&self) -> &EditLists {
        &self.lists
    }

    #[cfg(test)]
    pub(in crate::app) fn list_error(&self, tab: PropertiesTab, name: &str) -> Option<&str> {
        self.list_errors.get(&(tab, name.to_owned())).map(String::as_str)
    }

    #[cfg(test)]
    pub(in crate::app) fn form_is_open(&self) -> bool {
        self.form.is_some()
    }

    #[cfg(test)]
    pub(in crate::app) fn form_error(&self) -> Option<&str> {
        match self.form.as_ref()? {
            Form::Index(form) => form.error.as_deref(),
            Form::ForeignKey(form) => form.error.as_deref(),
        }
    }

    /// The columns picked in the open form, in the order they were picked.
    #[cfg(test)]
    pub(in crate::app) fn form_columns(&self, cx: &gpui::App) -> Vec<String> {
        match self.form.as_ref() {
            Some(Form::Index(form)) => picked(&form.columns, cx),
            Some(Form::ForeignKey(form)) => picked(&form.columns, cx),
            None => Vec::new(),
        }
    }

    /// The referenced table and the referenced columns picked in the open foreign key form.
    #[cfg(test)]
    pub(in crate::app) fn form_referenced(&self, cx: &gpui::App) -> (Option<String>, Vec<String>) {
        match self.form.as_ref() {
            Some(Form::ForeignKey(form)) => (
                form.table.read(cx).selected_value().map(ToString::to_string),
                match &form.referenced {
                    Referenced::Loaded { picker: Some(picker), .. } => picked(picker, cx),
                    _ => Vec::new(),
                },
            ),
            _ => (None, Vec::new()),
        }
    }

    /// ON DELETE and ON UPDATE chosen in the open foreign key form.
    #[cfg(test)]
    pub(in crate::app) fn form_actions(&self, cx: &gpui::App) -> (Option<FkAction>, Option<FkAction>) {
        match self.form.as_ref() {
            Some(Form::ForeignKey(form)) => (selected(&form.on_delete, cx), selected(&form.on_update, cx)),
            _ => (None, None),
        }
    }

    #[cfg(test)]
    pub(in crate::app) fn form_name(&self, cx: &gpui::App) -> String {
        match self.form.as_ref() {
            Some(Form::Index(form)) => form.name.read(cx).value().to_string(),
            Some(Form::ForeignKey(form)) => form.name.read(cx).value().to_string(),
            None => String::new(),
        }
    }

    /// The state of the referenced columns of the open foreign key form, for the tests.
    #[cfg(test)]
    pub(in crate::app) fn referenced_state(&self) -> &'static str {
        match self.form.as_ref() {
            Some(Form::ForeignKey(form)) => match &form.referenced {
                Referenced::Nothing => "nothing",
                Referenced::Loading(_) => "loading",
                Referenced::Failed { .. } => "failed",
                Referenced::Loaded { picker: None, .. } => "preparing",
                Referenced::Loaded { picker: Some(_), .. } => "loaded",
            },
            _ => "none",
        }
    }

    // ---- opening and closing a form ----

    fn taken_index_names(&self) -> impl Fn(&str) -> bool + '_ {
        let rules = self.context.scope.dialect().name_rules();
        move |name| self.lists.indexes.iter().any(|row| row.kind != RowKind::Dropped && rules.same(&row.name, name))
    }

    fn taken_foreign_key_names(&self) -> impl Fn(&str) -> bool + '_ {
        let rules = self.context.scope.dialect().name_rules();
        move |name| self.lists.foreign_keys.iter().any(|row| row.kind != RowKind::Dropped && rules.same(&row.name, name))
    }

    /// `Add index`: the form opens with a generated name.
    pub(in crate::app) fn open_index_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lists.index_add_block.is_some() {
            return;
        }
        self.commit_edit(Exit::Blur, window, cx);
        let auto_name = specs::default_index_name(&self.table.name, &[], self.taken_index_names());
        let name = cx.new(|cx| InputState::new(window, cx).default_value(auto_name.clone()));
        let columns = multi_picker(self.column_choices(), window, cx);
        let subscription = cx.subscribe_in(&columns, window, |this: &mut Self, _, _: &ComboboxEvent<Names>, window, cx| {
            this.columns_picked(window, cx);
        });
        name.update(cx, |input, cx| input.focus(window, cx));
        self.form = Some(Form::Index(IndexForm { name, columns, unique: false, auto_name, error: None, _subscriptions: vec![subscription] }));
        self.notice = None;
        cx.notify();
    }

    /// `Add foreign key`: the form opens with a generated name; the referenced columns follow the table.
    pub(in crate::app) fn open_foreign_key_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lists.foreign_key_add_block.is_some() {
            return;
        }
        self.commit_edit(Exit::Blur, window, cx);
        let auto_name = specs::default_foreign_key_name(&self.table.name, &[], self.taken_foreign_key_names());
        let name = cx.new(|cx| InputState::new(window, cx).default_value(auto_name.clone()));
        let columns = multi_picker(self.column_choices(), window, cx);
        let tables: Vec<String> = self.context.catalog.iter().flat_map(|tables| tables.iter().cloned()).collect();
        let searchable = tables.len() > SEARCHABLE_FROM;
        let table = cx.new(|cx| SelectState::new(names(tables), None, window, cx).searchable(searchable));
        let (on_delete, on_update) = (action_select(window, cx), action_select(window, cx));
        let subscriptions = vec![
            cx.subscribe_in(&columns, window, |this: &mut Self, _, _: &ComboboxEvent<Names>, window, cx| this.columns_picked(window, cx)),
            cx.subscribe(&table, |this: &mut Self, _, event: &SelectEvent<Names>, cx| {
                if let SelectEvent::Confirm(Some(table)) = event {
                    this.choose_referenced_table(table.to_string(), cx);
                }
            }),
        ];
        name.update(cx, |input, cx| input.focus(window, cx));
        self.form = Some(Form::ForeignKey(FkForm {
            name,
            columns,
            table,
            referenced: Referenced::Nothing,
            on_delete,
            on_update,
            auto_name,
            error: None,
            _subscriptions: subscriptions,
        }));
        self.notice = None;
        cx.notify();
    }

    pub(in crate::app) fn close_form(&mut self, cx: &mut Context<Self>) {
        if self.form.take().is_some() {
            cx.notify();
        }
    }

    /// The columns of the form changed: a generated name follows them, a typed one stays.
    fn columns_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.table.name.clone();
        let (name, picked, auto_name, foreign) = match self.form.as_ref() {
            Some(Form::Index(form)) => (form.name.clone(), picked(&form.columns, cx), form.auto_name.clone(), false),
            Some(Form::ForeignKey(form)) => (form.name.clone(), picked(&form.columns, cx), form.auto_name.clone(), true),
            None => return,
        };
        if name.read(cx).value().as_ref() != auto_name {
            return;
        }
        let next = if foreign {
            specs::default_foreign_key_name(&table, &picked, self.taken_foreign_key_names())
        } else {
            specs::default_index_name(&table, &picked, self.taken_index_names())
        };
        name.update(cx, |input, cx| input.set_value(next.clone(), window, cx));
        match self.form.as_mut() {
            Some(Form::Index(form)) => form.auto_name = next,
            Some(Form::ForeignKey(form)) => form.auto_name = next,
            None => {}
        }
    }

    pub(in crate::app) fn set_index_unique(&mut self, unique: bool, cx: &mut Context<Self>) {
        if let Some(Form::Index(form)) = self.form.as_mut() {
            form.unique = unique;
            cx.notify();
        }
    }

    // ---- the referenced table ----

    /// A table was chosen in the foreign key form: AppRoot describes it for the referenced columns.
    fn choose_referenced_table(&mut self, table: String, cx: &mut Context<Self>) {
        if let Some(Form::ForeignKey(form)) = self.form.as_mut() {
            form.referenced = Referenced::Loading(table.clone());
            form.error = None;
            cx.emit(PropertiesEditEvent::Describe(table));
            cx.notify();
        }
    }

    /// Chooses `table` as the form's referenced table without the Select (the answer is awaited as usual).
    #[cfg(test)]
    pub(in crate::app) fn start_referenced_for_test(&mut self, table: &str, cx: &mut Context<Self>) {
        self.choose_referenced_table(table.to_owned(), cx);
    }

    /// Retry on a failed description.
    pub(in crate::app) fn retry_referenced(&mut self, cx: &mut Context<Self>) {
        let Some(Form::ForeignKey(form)) = self.form.as_mut() else { return };
        if let Referenced::Failed { table, .. } = &form.referenced {
            let table = table.clone();
            self.choose_referenced_table(table, cx);
        }
    }

    /// AppRoot described `table`: its column names, or why not. An answer for a table that is not
    /// the chosen one any more (another was chosen, the form closed) is dropped.
    pub(in crate::app) fn referenced_described(&mut self, table: &str, result: Result<Vec<String>, String>, cx: &mut Context<Self>) {
        let Some(Form::ForeignKey(form)) = self.form.as_mut() else { return };
        if !matches!(&form.referenced, Referenced::Loading(chosen) if chosen == table) {
            return;
        }
        form.referenced = match result {
            Ok(columns) if columns.is_empty() => Referenced::Failed { table: table.to_owned(), error: "the table has no column".into() },
            Ok(names) => Referenced::Loaded { table: table.to_owned(), names, picker: None },
            Err(error) => Referenced::Failed { table: table.to_owned(), error },
        };
        cx.notify();
    }

    /// Runs before each frame: makes the referenced columns' picker once they are described (it
    /// needs the window).
    pub(in crate::app) fn sync_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Form::ForeignKey(form)) = self.form.as_mut() else { return };
        let Referenced::Loaded { names, picker: picker @ None, .. } = &mut form.referenced else { return };
        let items = std::mem::take(names);
        *picker = Some(multi_picker(items, window, cx));
        cx.notify();
    }

    // ---- submit ----

    /// The form's Add button: the change enters the draft, the form closes. A refused value keeps
    /// the form open with the reason.
    pub(in crate::app) fn submit_form(&mut self, cx: &mut Context<Self>) {
        let outcome = match self.form.as_ref() {
            Some(Form::Index(form)) => self.index_change(form, cx),
            Some(Form::ForeignKey(form)) => self.foreign_key_change(form, cx),
            None => return,
        };
        match outcome {
            Ok(change) => {
                if self.edit_draft(cx, |draft| draft.push(change)) {
                    self.form = None;
                    cx.notify();
                }
            }
            Err(reason) => {
                match self.form.as_mut() {
                    Some(Form::Index(form)) => form.error = Some(reason),
                    Some(Form::ForeignKey(form)) => form.error = Some(reason),
                    None => {}
                }
                cx.notify();
            }
        }
    }

    fn index_change(&self, form: &IndexForm, cx: &gpui::App) -> Result<Change, String> {
        let spec = specs::index_spec(form.name.read(cx).value().as_ref(), picked(&form.columns, cx), form.unique, self.taken_index_names())?;
        Ok(Change::CreateIndex { table: self.table.clone(), index: spec })
    }

    fn foreign_key_change(&self, form: &FkForm, cx: &gpui::App) -> Result<Change, String> {
        let Some(chosen) = form.table.read(cx).selected_value().map(ToString::to_string) else {
            return Err("Choose the referenced table.".into());
        };
        let ref_columns = match &form.referenced {
            Referenced::Loaded { picker: Some(picker), .. } => picked(picker, cx),
            Referenced::Loading(_) | Referenced::Loaded { .. } => return Err("The referenced columns are still loading.".into()),
            Referenced::Failed { error, .. } => return Err(format!("The referenced columns could not be loaded: {error}")),
            Referenced::Nothing => return Err("Choose the referenced table.".into()),
        };
        let scope = &self.context.scope;
        let spec = ForeignKeySpec {
            name: form.name.read(cx).value().trim().to_owned(),
            columns: picked(&form.columns, cx),
            ref_table: scope.table_id(&scope.database, scope.schema.as_deref(), &chosen),
            ref_columns,
            on_delete: selected(&form.on_delete, cx),
            on_update: selected(&form.on_update, cx),
        };
        specs::check_foreign_key(&spec)?;
        if self.taken_foreign_key_names()(&spec.name) {
            return Err(format!("There is a foreign key named {} already.", spec.name));
        }
        Ok(Change::AddForeignKey { table: self.table.clone(), foreign_key: spec })
    }

    // ---- row actions ----

    /// Drop on an existing row, Remove on a new one.
    pub(in crate::app) fn drop_list_row(&mut self, tab: PropertiesTab, name: &str, cx: &mut Context<Self>) {
        let Some(row) = self.lists.rows(tab).iter().find(|row| row.name == name).cloned() else { return };
        if self.row_block(tab, &row).is_some() {
            return;
        }
        let (table, name) = (self.table.clone(), name.to_owned());
        let change = match tab {
            PropertiesTab::Indexes => Change::DropIndex { table, name },
            _ => Change::DropForeignKey { table, name },
        };
        self.edit_draft(cx, |draft| draft.push(change));
    }

    /// Restore on a dropped row: the pending drop leaves the draft.
    pub(in crate::app) fn restore_list_row(&mut self, tab: PropertiesTab, name: &str, cx: &mut Context<Self>) {
        let at = self.session.draft.changes.iter().position(|change| match (tab, change) {
            (PropertiesTab::Indexes, Change::DropIndex { table, name: n }) => *table == self.table && n == name,
            (PropertiesTab::ForeignKeys, Change::DropForeignKey { table, name: n }) => *table == self.table && n == name,
            _ => false,
        });
        if !self.editable() {
            return;
        }
        if let Some(after) = at.and_then(|at| self.session.discard(at)) {
            self.set_draft(after, cx);
        }
    }
}

// ---- drawing -------------------------------------------------------------------------------------

/// A column of a list grid in edit mode: its header and width.
struct ListColumn {
    header: &'static str,
    width: f32,
}

const MARKER_WIDTH: f32 = 28.0;
const ACTION_WIDTH: f32 = 92.0;

fn list_columns(tab: PropertiesTab) -> &'static [ListColumn] {
    const INDEXES: [ListColumn; 4] = [
        ListColumn { header: "Name", width: 200.0 },
        ListColumn { header: "Columns", width: 220.0 },
        ListColumn { header: "Unique", width: 70.0 },
        ListColumn { header: "Method", width: 90.0 },
    ];
    const FOREIGN_KEYS: [ListColumn; 5] = [
        ListColumn { header: "Name", width: 180.0 },
        ListColumn { header: "Columns", width: 150.0 },
        ListColumn { header: "References", width: 200.0 },
        ListColumn { header: "On Update", width: 100.0 },
        ListColumn { header: "On Delete", width: 100.0 },
    ];
    match tab {
        PropertiesTab::Indexes => &INDEXES,
        _ => &FOREIGN_KEYS,
    }
}

fn cell(width: f32) -> Div {
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
}

fn field(label: &'static str, input: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(div().text_size(px(10.0)).text_color(theme::text_faint()).child(label))
        .child(input)
}

impl AppRoot {
    /// `Add index`: opens the form.
    pub(in crate::app) fn open_properties_index_form(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entity) = self.properties_edits.get(&tab_id).cloned() {
            entity.update(cx, |edit, cx| edit.open_index_form(window, cx));
        }
    }

    /// `Add foreign key`: opens the form.
    pub(in crate::app) fn open_properties_foreign_key_form(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entity) = self.properties_edits.get(&tab_id).cloned() {
            entity.update(cx, |edit, cx| edit.open_foreign_key_form(window, cx));
        }
    }

    /// The Indexes or Foreign keys sub-tab in edit mode: toolbar, the open form, and the grid of
    /// the rows with the draft's markers.
    pub(in crate::app) fn render_edit_list_content(&mut self, tab_id: u64, tab: PropertiesTab, state: EditRender, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let columns_focus = self.view_mut(tab_id).columns_focus(cx);
        let columns = list_columns(tab);
        let table_width = MARKER_WIDTH + columns.iter().map(|column| column.width).sum::<f32>() + ACTION_WIDTH;
        let (filtered, scroll, add_block) = match tab {
            PropertiesTab::Indexes => (Arc::clone(&state.index_filtered), state.index_scroll.clone(), state.lists.index_add_block.clone()),
            _ => (Arc::clone(&state.foreign_key_filtered), state.foreign_key_scroll.clone(), state.lists.foreign_key_add_block.clone()),
        };
        let total = state.lists.rows(tab).len();
        let noun = if tab == PropertiesTab::Indexes { "Indexes" } else { "Foreign keys" };
        let title = if filtered.len() == total { format!("{noun} ({total})") } else { format!("{noun} ({}/{total})", filtered.len()) };
        let header = div()
            .debug_selector(|| "properties-table-header".into())
            .h(px(HEADER_ROW_HEIGHT))
            .w(px(table_width))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::grid_header())
            .border_b_1()
            .border_color(theme::border())
            .text_size(px(11.0))
            .text_color(theme::text_muted())
            .child(cell(MARKER_WIDTH))
            .children(columns.iter().map(|column| cell(column.width).child(column.header)))
            .child(cell(ACTION_WIDTH));
        let empty = filtered.is_empty();
        let empty_text = format!("No {}{}", if total == 0 { "" } else { "matching " }, noun.to_lowercase());
        let list_state = state.clone();
        let list = uniform_list(
            (tab.selector(), tab_id),
            filtered.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range.map(|position| list_row(tab_id, tab, position, &list_state, table_width, cx)).collect()
            }),
        )
        .track_scroll(&scroll)
        .flex_1()
        .min_h_0();
        let filter = self.columns_filter_inputs.get(&tab_id).cloned().map(|input| {
            div().debug_selector(|| "columns-view-filter".into()).w(px(220.0)).min_w(px(96.0)).flex_shrink(1.0).child(Input::new(&input).small().cleanable(true))
        });
        let toolbar = self.render_properties_edit_toolbar(tab_id, tab, cx);
        let right = div().flex().items_center().gap(px(12.0)).children(filter).children(toolbar);
        let form = state.form.clone().map(|form| form_panel(tab_id, tab, form, cx));
        div()
            .id(("properties-content", tab_id))
            .track_focus(&columns_focus)
            .key_context("properties-edit")
            .on_action(cx.listener(move |this, _: &crate::app::actions::PropertiesUndo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.undo(cx))))
            .on_action(cx.listener(move |this, _: &crate::app::actions::PropertiesRedo, _, cx| this.with_properties_edit(tab_id, cx, |edit, cx| edit.redo(cx))))
            .debug_selector(|| "properties-content".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(Self::properties_toolbar(title, right))
            .children(form)
            .children(add_block.filter(|_| state.form.is_none()).map(|reason| {
                let loading = tab == PropertiesTab::ForeignKeys && state.lists.catalog_loading;
                let failed = tab == PropertiesTab::ForeignKeys && state.lists.catalog_failed;
                div()
                    .debug_selector(|| "properties-edit-list-note".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(11.0))
                    .text_color(if failed { theme::danger() } else { theme::text_muted() })
                    .border_b_1()
                    .border_color(theme::border())
                    .children(loading.then(|| Icon::empty().path("icons/loader-circle.svg").size(px(12.0))))
                    .child(div().min_w_0().truncate().child(reason))
                    .children(failed.then(|| {
                        Button::new(("properties-edit-catalog-retry", tab_id))
                            .label("Retry")
                            .with_size(Size::XSmall)
                            .compact()
                            .ghost()
                            .debug_selector(|| "properties-edit-catalog-retry".into())
                            .on_click(cx.listener(move |this, _, _, cx| this.retry_structure_catalog(tab_id, cx)))
                    }))
            }))
            .child(header)
            .child(if empty {
                div()
                    .debug_selector(|| "properties-empty".into())
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(empty_text)
                    .into_any_element()
            } else {
                list.into_any_element()
            })
    }

    /// Drop (an existing row) or Remove (a new one) on the row `name`.
    fn drop_properties_list_row(&mut self, tab_id: u64, tab: PropertiesTab, name: String, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| edit.drop_list_row(tab, &name, cx));
    }

    fn restore_properties_list_row(&mut self, tab_id: u64, tab: PropertiesTab, name: String, cx: &mut Context<Self>) {
        self.with_properties_edit(tab_id, cx, |edit, cx| edit.restore_list_row(tab, &name, cx));
    }
}

/// One visible row of an Indexes / Foreign keys grid in edit mode.
fn list_row(tab_id: u64, tab: PropertiesTab, position: usize, state: &EditRender, table_width: f32, cx: &mut Context<AppRoot>) -> AnyElement {
    let filtered = if tab == PropertiesTab::Indexes { &state.index_filtered } else { &state.foreign_key_filtered };
    let index = filtered[position];
    let row = &state.lists.rows(tab)[index];
    let block = state.lists.blocks(tab)[index].clone();
    let error = state.list_errors.get(&(tab, row.name.clone())).cloned();
    let dropped = row.kind == RowKind::Dropped;
    let color = match row.kind {
        _ if error.is_some() => theme::danger(),
        RowKind::New => theme::success(),
        RowKind::Dropped => theme::danger(),
        _ => theme::text_muted(),
    };
    let (label, selector) = match row.kind {
        RowKind::Dropped => ("Restore", "properties-edit-restore"),
        RowKind::New => ("Remove", "properties-edit-remove"),
        _ => ("Drop", "properties-edit-drop"),
    };
    let name = row.name.clone();
    let action = Button::new(("properties-edit-action", position))
        .label(label)
        .with_size(Size::XSmall)
        .compact()
        .ghost()
        .disabled(block.is_some())
        .when_some(block, |button, reason| button.tooltip(reason))
        .on_click(cx.listener(move |this, _, _, cx| {
            let name = name.clone();
            match dropped {
                true => this.restore_properties_list_row(tab_id, tab, name, cx),
                false => this.drop_properties_list_row(tab_id, tab, name, cx),
            }
        }));
    let marker = cell(MARKER_WIDTH)
        .debug_selector(move || format!("properties-edit-marker-{position}"))
        .id(("properties-edit-marker-cell", position))
        .justify_center()
        .text_color(color)
        .font_weight(gpui::FontWeight::BOLD)
        .child(if error.is_some() { "!" } else { row.kind.marker() })
        .when_some(error, |this, message| this.tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(message.clone()).build(window, cx)));
    div()
        .id(("properties-edit-row", position))
        .debug_selector(move || format!("properties-edit-row-{position}"))
        .h(px(ROW_HEIGHT))
        .w(px(table_width))
        .flex()
        .items_center()
        .text_size(px(12.0))
        .when(dropped, |this| this.opacity(0.55))
        .hover(|style| style.bg(theme::hover()))
        .border_b_1()
        .border_color(theme::divider())
        .child(marker)
        .children(list_columns(tab).iter().enumerate().map(|(at, column)| {
            cell(column.width).child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(if at == 0 { theme::text() } else { theme::text_muted() })
                    .when(dropped && at == 0, |this| this.line_through())
                    .child(row.cells[at].clone()),
            )
        }))
        .child(cell(ACTION_WIDTH).child(div().debug_selector(move || format!("{selector}-{position}")).child(action)))
        .into_any_element()
}

/// The panel of the open "add index" / "add foreign key" form.
fn form_panel(tab_id: u64, tab: PropertiesTab, form: FormRender, cx: &mut Context<AppRoot>) -> AnyElement {
    let button = |id: &'static str, label: &'static str| Button::new((id, tab_id)).label(label).with_size(Size::Small).compact().debug_selector(move || id.into());
    let submit_label = if tab == PropertiesTab::Indexes { "Add index" } else { "Add foreign key" };
    let buttons = div()
        .flex()
        .gap(px(6.0))
        .items_center()
        .child(button("properties-edit-form-submit", submit_label).primary().on_click(cx.listener(move |this, _, _, cx| {
            this.with_properties_edit(tab_id, cx, |edit, cx| edit.submit_form(cx))
        })))
        .child(button("properties-edit-form-cancel", "Cancel").ghost().on_click(cx.listener(move |this, _, _, cx| {
            this.with_properties_edit(tab_id, cx, |edit, cx| edit.close_form(cx))
        })));
    let sized = |selector: &'static str, width: f32, control: AnyElement| div().w(px(width)).debug_selector(move || selector.into()).child(control);
    let panel = div()
        .id(("properties-edit-form", tab_id))
        .debug_selector(|| "properties-edit-form".into())
        .flex_shrink_0()
        .px(px(12.0))
        .py(px(8.0))
        .flex()
        .flex_wrap()
        .items_start()
        .gap(px(12.0))
        .bg(theme::grid_header())
        .border_b_1()
        .border_color(theme::border())
        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
            // Escape closes a menu first (the control takes it); the form closes on the next one.
            if event.keystroke.key == "escape" && !gpui_component::GlobalState::is_in_deferred_context(cx) {
                this.with_properties_edit(tab_id, cx, |edit, cx| edit.close_form(cx));
                cx.stop_propagation();
            }
        }));
    let error = |message: Option<String>| {
        message.map(|message| div().debug_selector(|| "properties-edit-form-error".into()).w_full().text_size(px(11.0)).text_color(theme::danger()).child(message))
    };
    match form {
        FormRender::Index { name, columns, unique, error: message } => panel
            .child(field("Index name", sized("properties-edit-form-name", 240.0, Input::new(&name).with_size(Size::Small).into_any_element())))
            .child(field(
                "Columns (in the order picked)",
                sized(
                    "properties-edit-form-columns",
                    280.0,
                    Combobox::new(&columns).with_size(Size::Small).placeholder("Choose columns").into_any_element(),
                ),
            ))
            .child(field(
                "Options",
                div().debug_selector(|| "properties-edit-form-unique".into()).h(px(28.0)).flex().items_center().child(
                    Checkbox::new(("properties-edit-form-unique-box", tab_id)).label("Unique").checked(unique).on_click(cx.listener(
                        move |this, checked: &bool, _, cx| {
                            let checked = *checked;
                            this.with_properties_edit(tab_id, cx, |edit, cx| edit.set_index_unique(checked, cx));
                        },
                    )),
                ),
            ))
            .child(field("", buttons))
            .children(error(message)),
        FormRender::ForeignKey { name, columns, table, referenced, on_delete, on_update, error: message } => {
            let referenced_columns: AnyElement = match referenced {
                ReferencedRender::Nothing => div().h(px(28.0)).flex().items_center().text_size(px(11.0)).text_color(theme::text_faint()).child("Choose the table first").into_any_element(),
                ReferencedRender::Loading(table) => div()
                    .debug_selector(|| "properties-edit-form-loading".into())
                    .h(px(28.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(Icon::empty().path("icons/loader-circle.svg").size(px(12.0)))
                    .child(format!("Loading the columns of {table}\u{2026}"))
                    .into_any_element(),
                ReferencedRender::Failed(text) => div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().debug_selector(|| "properties-edit-form-ref-error".into()).min_w_0().truncate().text_size(px(11.0)).text_color(theme::danger()).child(text))
                    .child(button("properties-edit-form-retry", "Retry").ghost().on_click(cx.listener(move |this, _, _, cx| {
                        this.with_properties_edit(tab_id, cx, |edit, cx| edit.retry_referenced(cx))
                    })))
                    .into_any_element(),
                ReferencedRender::Picker(picker) => Combobox::new(&picker).with_size(Size::Small).placeholder("Choose columns").into_any_element(),
            };
            panel
                .child(field("Constraint name", sized("properties-edit-form-name", 240.0, Input::new(&name).with_size(Size::Small).into_any_element())))
                .child(field(
                    "Columns of this table (in order)",
                    sized("properties-edit-form-columns", 260.0, Combobox::new(&columns).with_size(Size::Small).placeholder("Choose columns").into_any_element()),
                ))
                .child(field(
                    "References",
                    sized("properties-edit-form-table", 220.0, Select::new(&table).with_size(Size::Small).placeholder("Table").into_any_element()),
                ))
                .child(field("Referenced columns (in order)", sized("properties-edit-form-ref-columns", 280.0, referenced_columns)))
                .child(field("On delete", sized("properties-edit-form-on-delete", 170.0, Select::new(&on_delete).with_size(Size::Small).into_any_element())))
                .child(field("On update", sized("properties-edit-form-on-update", 170.0, Select::new(&on_update).with_size(Size::Small).into_any_element())))
                .child(field("", buttons))
                .children(error(message))
        }
    }
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{CatalogForeignKey, CatalogIndex};

    fn table() -> TableId {
        TableId { schema: Some("public".into()), name: "t".into() }
    }

    fn described() -> TableProperties {
        TableProperties {
            indexes: Some(vec![
                CatalogIndex { name: "t_pkey".into(), columns: vec!["id".into()], unique: true, primary: true, method: Some("btree".into()), definition: None },
                CatalogIndex { name: "t_a_idx".into(), columns: vec!["a".into(), "b".into()], unique: false, primary: false, method: None, definition: None },
            ]),
            foreign_keys: Some(vec![CatalogForeignKey {
                name: "t_fk".into(),
                table: "public.t".into(),
                columns: vec!["a".into()],
                referenced_table: "public.o".into(),
                referenced_columns: vec!["id".into()],
                on_update: None,
                on_delete: Some("CASCADE".into()),
                definition: None,
            }]),
            ..TableProperties::default()
        }
    }

    fn index(name: &str) -> IndexSpec {
        IndexSpec { name: name.into(), columns: vec!["b".into()], unique: true }
    }

    #[test]
    fn index_rows_mark_what_the_draft_drops_and_creates() {
        let draft = Draft::from_changes(vec![
            Change::DropIndex { table: table(), name: "t_a_idx".into() },
            Change::CreateIndex { table: table(), index: index("new_idx") },
            Change::CreateIndex { table: TableId { schema: None, name: "other".into() }, index: index("elsewhere") },
        ]);
        let rows = build_index_rows(&described(), &draft, &table());
        let summary: Vec<(&str, RowKind, bool)> = rows.iter().map(|row| (row.name.as_str(), row.kind, row.primary)).collect();
        assert_eq!(summary, [("t_pkey", RowKind::Existing, true), ("t_a_idx", RowKind::Dropped, false), ("new_idx", RowKind::New, false)]);
        let cells: Vec<&str> = rows[1].cells.iter().map(|cell| cell.as_ref()).collect();
        assert_eq!(cells, ["t_a_idx", "a, b", "No", ""]);
        assert_eq!(rows[2].cells[2].as_ref(), "Yes");
    }

    #[test]
    fn foreign_key_rows_mark_what_the_draft_drops_and_adds() {
        let added = ForeignKeySpec {
            name: "t_new_fk".into(),
            columns: vec!["b".into()],
            ref_table: TableId { schema: Some("public".into()), name: "p".into() },
            ref_columns: vec!["id".into()],
            on_delete: Some(FkAction::SetNull),
            on_update: Some(FkAction::Restrict),
        };
        let draft = Draft::from_changes(vec![
            Change::DropForeignKey { table: table(), name: "t_fk".into() },
            Change::AddForeignKey { table: table(), foreign_key: added },
        ]);
        let rows = build_foreign_key_rows(&described(), &draft, &table());
        assert_eq!(rows.iter().map(|row| (row.name.as_str(), row.kind)).collect::<Vec<_>>(), [("t_fk", RowKind::Dropped), ("t_new_fk", RowKind::New)]);
        let cells: Vec<&str> = rows[0].cells.iter().map(|cell| cell.as_ref()).collect();
        assert_eq!(cells, ["t_fk", "a", "public.o(id)", "", "CASCADE"]);
        let cells: Vec<&str> = rows[1].cells.iter().map(|cell| cell.as_ref()).collect();
        assert_eq!(cells, ["t_new_fk", "b", "public.p(id)", "RESTRICT", "SET NULL"]);
    }

    #[test]
    fn the_filter_matches_any_cell() {
        let rows = build_index_rows(&described(), &Draft::default(), &table());
        assert_eq!(filter_rows(&rows, ""), [0, 1]);
        assert_eq!(filter_rows(&rows, "a, b"), [1]);
        assert_eq!(filter_rows(&rows, "btree"), [0]);
        assert!(filter_rows(&rows, "nothing").is_empty());
    }

    #[test]
    fn a_pending_key_says_what_it_adds_removes_or_drops() {
        let key = |before: &[&str], after: &[&str]| PendingKey {
            before: before.iter().map(|c| c.to_string()).collect(),
            after: after.iter().map(|c| c.to_string()).collect(),
        };
        assert_eq!(key(&["id"], &["id", "code"]).summary(), "Pending: the primary key (id) becomes (id, code); adds code");
        assert_eq!(key(&["id", "code"], &["id"]).summary(), "Pending: the primary key (id, code) becomes (id); removes code");
        assert_eq!(key(&[], &["id"]).summary(), "Pending: new primary key (id)");
        assert_eq!(key(&["id"], &[]).summary(), "Pending: the primary key (id) is dropped");
        assert_eq!(key(&["a"], &["b"]).summary(), "Pending: the primary key (a) becomes (b); adds b; removes a");
        let live = LiveTable { primary_key: Some(PrimaryKeyRef { name: None, columns: vec!["id".into()] }), ..LiveTable::default() };
        let draft = Draft::from_changes(vec![Change::SetPrimaryKey { table: table(), columns: vec!["id".into(), "x".into()], previous: None }]);
        assert_eq!(pending_key(&live, &draft, &table()), Some(key(&["id"], &["id", "x"])));
        assert_eq!(pending_key(&live, &Draft::default(), &table()), None);
    }
}
