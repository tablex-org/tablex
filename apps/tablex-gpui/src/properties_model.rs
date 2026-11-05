//! Pure model of the Properties sub-tabs backed by `TableProperties`
//! (Keys, Foreign Keys, Indexes, References, Triggers list tables and the DDL
//! text): fetch state, per-tab list models with content-sized column widths,
//! and query filtering. Built once per fetched result and re-filtered only when
//! the filter text changes, never per frame.

use crate::columns_panel::{PropertiesTab, CELL_PADDING, NAME_MAX};
use crate::catalog::{
    CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex, CatalogTrigger,
    TableProperties,
};
use gpui::{SharedString, UniformListScrollHandle};
use gpui_component::input::EditorState;
use std::collections::HashMap;
use std::sync::Arc;

pub const DEFINITION_MAX: f32 = 420.0;
pub const OTHER_MAX: f32 = 260.0;

/// How a cell is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellStyle {
    /// Primary text.
    Name,
    /// Regular secondary text.
    Plain,
    /// Ellipsized definition text; the full text is the row's tooltip.
    Definition,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PropertyColumn {
    pub header: &'static str,
    pub style: CellStyle,
    pub max: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PropertyRow {
    pub cells: Vec<SharedString>,
    /// Full (unabridged) definition, shown as the Definition cell's tooltip.
    pub tooltip: Option<SharedString>,
    /// Lower-cased searchable text of every cell.
    haystack: String,
}

/// One list sub-tab: rows, the visible columns (all-empty columns dropped),
/// content-sized widths, the filter result and the selected row.
#[derive(Debug, Clone)]
pub struct PropertyTable {
    pub tab: PropertiesTab,
    pub columns: Arc<Vec<PropertyColumn>>,
    pub rows: Arc<Vec<PropertyRow>>,
    pub widths: Arc<Vec<f32>>,
    /// Indices into `rows` matching the filter, in order.
    pub filtered: Arc<Vec<usize>>,
    filtered_for: String,
    /// Selected row (index into `rows`).
    pub selected: Option<usize>,
    pub scroll: UniformListScrollHandle,
}

fn column(header: &'static str, style: CellStyle) -> PropertyColumn {
    let max = match (header, style) {
        (_, CellStyle::Definition) => DEFINITION_MAX,
        ("Name", _) => NAME_MAX,
        _ => OTHER_MAX,
    };
    PropertyColumn { header, style, max }
}

fn join(list: &[String]) -> String {
    list.join(", ")
}

fn yes_no(value: bool) -> String {
    if value { "Yes" } else { "No" }.into()
}

fn opt_bool(value: Option<bool>) -> String {
    value.map(yes_no).unwrap_or_default()
}

fn opt(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

/// One-line form of a (possibly multi-line) definition for a table cell.
fn collapse(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

fn constraint_type(kind: CatalogConstraintKind) -> &'static str {
    match kind {
        CatalogConstraintKind::PrimaryKey => "PK",
        CatalogConstraintKind::Unique => "Unique",
        CatalogConstraintKind::Check => "Check",
        CatalogConstraintKind::Exclusion => "Exclusion",
        CatalogConstraintKind::Other => "Other",
    }
}

fn referenced(fk: &CatalogForeignKey) -> String {
    if fk.referenced_table.is_empty() {
        return String::new();
    }
    format!("{}({})", fk.referenced_table, join(&fk.referenced_columns))
}

/// Columns plus raw rows (cell texts + the full definition, if any).
type RawTable = (Vec<PropertyColumn>, Vec<(Vec<String>, Option<String>)>);

/// Columns and raw rows (cells + full definition) of a list tab; `None` for
/// Columns / DDL or when the driver did not report the list.
fn raw_rows(
    tab: PropertiesTab,
    props: &TableProperties,
) -> Option<RawTable> {
    use CellStyle::{Definition, Name, Plain};
    Some(match tab {
        PropertiesTab::Keys => (
            vec![
                column("Name", Name),
                column("Type", Plain),
                column("Columns", Plain),
                column("Definition", Definition),
            ],
            props
                .keys
                .as_ref()?
                .iter()
                .map(|key: &CatalogConstraint| {
                    (
                        vec![
                            key.name.clone(),
                            constraint_type(key.kind).into(),
                            join(&key.columns),
                            key.definition.as_deref().map(collapse).unwrap_or_default(),
                        ],
                        key.definition.clone(),
                    )
                })
                .collect(),
        ),
        PropertiesTab::ForeignKeys => (
            vec![
                column("Name", Name),
                column("Columns", Plain),
                column("References", Plain),
                column("On Update", Plain),
                column("On Delete", Plain),
            ],
            props
                .foreign_keys
                .as_ref()?
                .iter()
                .map(|fk| {
                    (
                        vec![
                            fk.name.clone(),
                            join(&fk.columns),
                            referenced(fk),
                            opt(&fk.on_update),
                            opt(&fk.on_delete),
                        ],
                        None,
                    )
                })
                .collect(),
        ),
        PropertiesTab::Indexes => (
            vec![
                column("Name", Name),
                column("Columns", Plain),
                column("Unique", Plain),
                column("Primary", Plain),
                column("Method", Plain),
                column("Definition", Definition),
            ],
            props
                .indexes
                .as_ref()?
                .iter()
                .map(|index: &CatalogIndex| {
                    (
                        vec![
                            index.name.clone(),
                            join(&index.columns),
                            yes_no(index.unique),
                            yes_no(index.primary),
                            opt(&index.method),
                            index.definition.as_deref().map(collapse).unwrap_or_default(),
                        ],
                        index.definition.clone(),
                    )
                })
                .collect(),
        ),
        PropertiesTab::References => (
            vec![
                column("Name", Name),
                column("From Table", Plain),
                column("Columns", Plain),
                column("To Columns", Plain),
                column("On Delete", Plain),
            ],
            props
                .references
                .as_ref()?
                .iter()
                .map(|fk| {
                    (
                        vec![
                            fk.name.clone(),
                            fk.table.clone(),
                            join(&fk.columns),
                            join(&fk.referenced_columns),
                            opt(&fk.on_delete),
                        ],
                        None,
                    )
                })
                .collect(),
        ),
        PropertiesTab::Triggers => (
            vec![
                column("Name", Name),
                column("Timing", Plain),
                column("Events", Plain),
                column("Enabled", Plain),
                column("Definition", Definition),
            ],
            props
                .triggers
                .as_ref()?
                .iter()
                .map(|trigger: &CatalogTrigger| {
                    (
                        vec![
                            trigger.name.clone(),
                            opt(&trigger.timing),
                            join(&trigger.events),
                            opt_bool(trigger.enabled),
                            trigger.definition.as_deref().map(collapse).unwrap_or_default(),
                        ],
                        trigger.definition.clone(),
                    )
                })
                .collect(),
        ),
        PropertiesTab::Columns | PropertiesTab::Ddl => return None,
    })
}

impl PropertyTable {
    /// Builds the list model of `tab`; columns whose values are all empty are
    /// hidden (the Name column always stays).
    pub fn build(tab: PropertiesTab, props: &TableProperties) -> Option<Self> {
        let (columns, raw) = raw_rows(tab, props)?;
        let keep: Vec<usize> = (0..columns.len())
            .filter(|&index| {
                index == 0 || raw.is_empty() || raw.iter().any(|(cells, _)| !cells[index].is_empty())
            })
            .collect();
        let rows: Vec<PropertyRow> = raw
            .into_iter()
            .map(|(cells, tooltip)| {
                let cells: Vec<SharedString> =
                    keep.iter().map(|&index| SharedString::from(cells[index].clone())).collect();
                let mut haystack = String::new();
                for cell in &cells {
                    haystack.push_str(&cell.to_lowercase());
                    haystack.push('\n');
                }
                let tooltip = tooltip
                    .filter(|full| !full.trim().is_empty())
                    .map(SharedString::from);
                PropertyRow { cells, tooltip, haystack }
            })
            .collect();
        let columns: Vec<PropertyColumn> = keep.iter().map(|&index| columns[index].clone()).collect();
        let filtered: Vec<usize> = (0..rows.len()).collect();
        Some(Self {
            tab,
            columns: Arc::new(columns),
            rows: Arc::new(rows),
            widths: Arc::default(),
            filtered: Arc::new(filtered),
            filtered_for: String::new(),
            selected: None,
            scroll: UniformListScrollHandle::new(),
        })
    }

    /// Sizes every column to `max(header, widest cell) + padding`, capped.
    /// Runs once per built table, never per frame.
    pub fn measure_widths(&mut self, measure: &dyn Fn(&str, bool) -> f32) {
        let widths = self
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let widest = self
                    .rows
                    .iter()
                    .map(|row| measure(&row.cells[index], false))
                    .fold(measure(column.header, true), f32::max);
                (widest + 2.0 * CELL_PADDING).min(column.max)
            })
            .collect();
        self.widths = Arc::new(widths);
    }

    pub fn total_width(&self) -> f32 {
        self.widths.iter().sum()
    }

    pub fn filter_is_stale(&self, query: &str) -> bool {
        self.filtered_for != query
    }

    /// Recomputes `filtered` (case-insensitive substring over every cell).
    pub fn refilter(&mut self, query: &str) {
        let needle = query.trim().to_lowercase();
        let filtered = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| needle.is_empty() || row.haystack.contains(&needle))
            .map(|(index, _)| index)
            .collect();
        self.filtered = Arc::new(filtered);
        self.filtered_for = query.to_string();
    }

    pub fn title(&self) -> String {
        if self.filtered.len() == self.rows.len() {
            format!("{} ({})", self.tab.label(), self.rows.len())
        } else {
            format!("{} ({}/{})", self.tab.label(), self.filtered.len(), self.rows.len())
        }
    }

    /// Position of `row` among the filtered rows.
    pub fn position_of(&self, row: usize) -> Option<usize> {
        self.filtered.iter().position(|&index| index == row)
    }

    /// The row `delta` positions away from the selection within the filtered
    /// rows (clamped); the first row when nothing is selected.
    pub fn step_selection(&self, delta: isize) -> Option<usize> {
        if self.filtered.is_empty() {
            return None;
        }
        let target = match self.selected.and_then(|row| self.position_of(row)) {
            Some(position) => {
                (position as isize + delta).clamp(0, self.filtered.len() as isize - 1) as usize
            }
            None => 0,
        };
        Some(self.filtered[target])
    }
}

/// Everything a fetched `TableProperties` depends on. Table metadata does not
/// change when the result is paged, sorted, filtered or auto-refreshed, so the
/// result generation is deliberately NOT part of the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertiesKey {
    pub connection: String,
    pub database: String,
    pub schema: String,
    pub object: String,
    /// Engine connection id (0 = not connected): a reconnect refetches.
    pub session: u64,
}

impl PropertiesKey {
    pub fn matches(
        &self,
        connection: &str,
        database: &str,
        schema: &str,
        object: &str,
        session: u64,
    ) -> bool {
        self.session == session
            && self.connection == connection
            && self.database == database
            && self.schema == schema
            && self.object == object
    }

    /// Same table, ignoring the session (a reconnect keeps the cached data
    /// visible while it refetches).
    fn same_table(&self, other: &Self) -> bool {
        self.connection == other.connection
            && self.database == other.database
            && self.schema == other.schema
            && self.object == other.object
    }

    /// Whether this table lies under the sidebar node given by the non-`None`
    /// path parts (a connection, database, schema or the table itself).
    pub fn covered_by(&self, connection: &str, database: Option<&str>, schema: Option<&str>, object: Option<&str>) -> bool {
        self.connection == connection
            && database.is_none_or(|value| value == self.database)
            && schema.is_none_or(|value| value == self.schema)
            && object.is_none_or(|value| value == self.object)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LoadStatus {
    /// Not table-backed, or not requested yet.
    #[default]
    Idle,
    Loading,
    Loaded,
    Failed(String),
}

/// Read-only SQL editor of the DDL tab, rebuilt when the fetched text changes.
#[derive(Debug, Clone)]
pub struct DdlEditor {
    pub revision: u64,
    pub state: gpui::Entity<EditorState>,
}

/// Per result tab: the fetched properties (cached by `key`), their status and
/// the lazily built per-sub-tab models.
#[derive(Debug, Clone, Default)]
pub struct PropertiesState {
    pub key: Option<PropertiesKey>,
    pub status: LoadStatus,
    /// Last successfully fetched properties; kept while a refetch for the
    /// same table is in flight so the sub-tabs do not flicker.
    pub data: Option<Arc<TableProperties>>,
    /// Bumped whenever `data` is replaced.
    pub revision: u64,
    pub tables: HashMap<PropertiesTab, PropertyTable>,
    pub ddl: Option<DdlEditor>,
    /// Number of fetches started (diagnostics / tests).
    pub fetches: u64,
    /// Set by an explicit refresh; the next sync refetches the same key while
    /// the cached data stays visible.
    pub stale: bool,
}

impl PropertiesState {
    /// Marks `key` as being fetched. Data of a different table is dropped.
    pub fn begin(&mut self, key: PropertiesKey) {
        if self.key.as_ref().is_none_or(|old| !old.same_table(&key)) {
            self.data = None;
            self.tables.clear();
            self.revision += 1;
        }
        self.key = Some(key);
        self.status = LoadStatus::Loading;
        self.stale = false;
        self.fetches += 1;
    }

    /// Requests a refetch (manual refresh, sidebar refresh); cached data stays
    /// on screen until the new result arrives. Ignored while one is in flight.
    pub fn invalidate(&mut self) {
        if self.key.is_some() && self.status != LoadStatus::Loading {
            self.stale = true;
        }
    }

    /// Applies a fetch result; results for a superseded key are ignored.
    pub fn finish(&mut self, key: &PropertiesKey, result: Result<TableProperties, String>) -> bool {
        if self.key.as_ref() != Some(key) {
            return false;
        }
        match result {
            Ok(properties) => {
                self.data = Some(Arc::new(properties));
                self.revision += 1;
                self.tables.clear();
                self.status = LoadStatus::Loaded;
            }
            Err(error) => self.status = LoadStatus::Failed(error),
        }
        true
    }

    /// Sub-tabs to show, in order: Columns plus every reported list.
    pub fn visible_tabs(&self) -> impl Iterator<Item = PropertiesTab> + '_ {
        PropertiesTab::ALL
            .into_iter()
            .filter(|tab| tab.available(self.data.as_deref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fk(name: &str, table: &str, cols: &[&str], target: &str, target_cols: &[&str]) -> CatalogForeignKey {
        CatalogForeignKey {
            name: name.into(),
            table: table.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
            referenced_table: target.into(),
            referenced_columns: target_cols.iter().map(|c| c.to_string()).collect(),
            on_update: None,
            on_delete: Some("CASCADE".into()),
            definition: None,
        }
    }

    fn props() -> TableProperties {
        TableProperties {
            keys: Some(vec![
                CatalogConstraint {
                    name: "orders_pkey".into(),
                    kind: CatalogConstraintKind::PrimaryKey,
                    columns: vec!["id".into()],
                    definition: None,
                },
                CatalogConstraint {
                    name: "qty_positive".into(),
                    kind: CatalogConstraintKind::Check,
                    columns: vec![],
                    definition: Some("CHECK (qty > 0)".into()),
                },
            ]),
            foreign_keys: Some(vec![fk("orders_customer_fk", "orders", &["customer_id"], "customers", &["id"])]),
            indexes: Some(vec![CatalogIndex {
                name: "orders_pkey".into(),
                columns: vec!["id".into()],
                unique: true,
                primary: true,
                method: Some("btree".into()),
                definition: None,
            }]),
            references: Some(vec![fk("items_order_fk", "public.items", &["order_id"], "orders", &["id"])]),
            triggers: Some(vec![CatalogTrigger {
                name: "audit".into(),
                timing: Some("AFTER".into()),
                events: vec!["INSERT".into(), "UPDATE".into()],
                enabled: None,
                definition: Some("CREATE TRIGGER audit\n    AFTER INSERT ON orders".into()),
            }]),
            ddl: Some("CREATE TABLE orders ()".into()),
            ..Default::default()
        }
    }

    fn headers(table: &PropertyTable) -> Vec<&'static str> {
        table.columns.iter().map(|c| c.header).collect()
    }

    fn cells(table: &PropertyTable, row: usize) -> Vec<&str> {
        table.rows[row].cells.iter().map(|c| c.as_ref()).collect()
    }

    #[test]
    fn every_list_tab_has_its_columns_and_rows() {
        let props = props();
        let keys = PropertyTable::build(PropertiesTab::Keys, &props).unwrap();
        assert_eq!(headers(&keys), ["Name", "Type", "Columns", "Definition"]);
        assert_eq!(cells(&keys, 0), ["orders_pkey", "PK", "id", ""]);
        assert_eq!(cells(&keys, 1), ["qty_positive", "Check", "", "CHECK (qty > 0)"]);

        let fks = PropertyTable::build(PropertiesTab::ForeignKeys, &props).unwrap();
        assert_eq!(headers(&fks), ["Name", "Columns", "References", "On Delete"], "On Update is all empty");
        assert_eq!(cells(&fks, 0), ["orders_customer_fk", "customer_id", "customers(id)", "CASCADE"]);

        let indexes = PropertyTable::build(PropertiesTab::Indexes, &props).unwrap();
        assert_eq!(headers(&indexes), ["Name", "Columns", "Unique", "Primary", "Method"]);
        assert_eq!(cells(&indexes, 0), ["orders_pkey", "id", "Yes", "Yes", "btree"]);

        let refs = PropertyTable::build(PropertiesTab::References, &props).unwrap();
        assert_eq!(headers(&refs), ["Name", "From Table", "Columns", "To Columns", "On Delete"]);
        assert_eq!(cells(&refs, 0), ["items_order_fk", "public.items", "order_id", "id", "CASCADE"]);

        let triggers = PropertyTable::build(PropertiesTab::Triggers, &props).unwrap();
        assert_eq!(headers(&triggers), ["Name", "Timing", "Events", "Definition"], "Enabled unknown");
        assert_eq!(
            cells(&triggers, 0),
            ["audit", "AFTER", "INSERT, UPDATE", "CREATE TRIGGER audit AFTER INSERT ON orders"]
        );
        assert_eq!(
            triggers.rows[0].tooltip.as_deref(),
            Some("CREATE TRIGGER audit\n    AFTER INSERT ON orders"),
            "the tooltip keeps the full definition"
        );
    }

    #[test]
    fn unreported_lists_and_non_list_tabs_have_no_table() {
        let none = TableProperties::default();
        for tab in PropertiesTab::ALL {
            assert!(PropertyTable::build(tab, &none).is_none(), "{tab:?}");
        }
        assert!(PropertyTable::build(PropertiesTab::Ddl, &props()).is_none());
        let empty = TableProperties { keys: Some(Vec::new()), ..Default::default() };
        let table = PropertyTable::build(PropertiesTab::Keys, &empty).unwrap();
        assert_eq!(table.rows.len(), 0);
        assert_eq!(headers(&table).len(), 4, "an empty list keeps its headers");
    }

    #[test]
    fn filter_matches_any_cell_case_insensitively_and_titles_track_it() {
        let mut table = PropertyTable::build(PropertiesTab::Keys, &props()).unwrap();
        assert_eq!(table.title(), "Keys (2)");
        assert!(table.filter_is_stale("check"));
        table.refilter("CHECK");
        assert_eq!(*table.filtered, vec![1]);
        assert_eq!(table.title(), "Keys (1/2)");
        assert!(!table.filter_is_stale("CHECK"));
        table.refilter("  ");
        assert_eq!(*table.filtered, vec![0, 1]);
        table.refilter("zzz");
        assert!(table.filtered.is_empty());
        assert_eq!(table.step_selection(1), None);
    }

    #[test]
    fn widths_fit_content_capped_and_selection_steps_within_the_filter() {
        let mut table = PropertyTable::build(PropertiesTab::Keys, &props()).unwrap();
        let measure = |text: &str, header: bool| text.chars().count() as f32 * if header { 6.0 } else { 7.0 };
        table.measure_widths(&measure);
        assert_eq!(table.widths[0], 12.0 * 7.0 + 2.0 * CELL_PADDING, "widest name: qty_positive");
        assert_eq!(table.widths[3], 15.0 * 7.0 + 2.0 * CELL_PADDING, "CHECK (qty > 0)");
        assert_eq!(table.total_width(), table.widths.iter().sum::<f32>());
        let long = |_: &str, _: bool| 10_000.0;
        table.measure_widths(&long);
        assert_eq!(table.widths[0], NAME_MAX);
        assert_eq!(table.widths[3], DEFINITION_MAX);

        table.refilter("");
        assert_eq!(table.step_selection(1), Some(0), "nothing selected: first row");
        table.selected = Some(0);
        assert_eq!(table.step_selection(1), Some(1));
        assert_eq!(table.step_selection(-1), Some(0));
        table.selected = Some(1);
        assert_eq!(table.step_selection(1), Some(1), "clamped");
    }

    fn key(session: u64) -> PropertiesKey {
        PropertiesKey {
            connection: "c".into(),
            database: "d".into(),
            schema: "s".into(),
            object: "orders".into(),
            session,
        }
    }

    #[test]
    fn fetch_state_caches_by_table_and_session_not_by_result_generation() {
        let mut state = PropertiesState::default();
        assert_eq!(state.visible_tabs().collect::<Vec<_>>(), [PropertiesTab::Columns]);
        state.begin(key(1));
        assert_eq!(state.status, LoadStatus::Loading);
        // A result for a superseded key is ignored.
        assert!(!state.finish(&key(0), Ok(props())));
        assert_eq!(state.status, LoadStatus::Loading);
        assert!(state.finish(&key(1), Ok(props())));
        assert_eq!(state.status, LoadStatus::Loaded);
        assert_eq!(state.visible_tabs().count(), 7, "Columns + 5 lists + DDL");
        // The key is (connection, database, schema, object, session): paging,
        // sorting or refreshing the result never changes it.
        let cached = state.key.as_ref().unwrap();
        assert!(cached.matches("c", "d", "s", "orders", 1));
        assert!(!cached.matches("c", "d", "s", "orders", 2), "reconnect (new session) refetches");
        assert!(!cached.matches("c", "d", "s", "customers", 1));

        // Explicit invalidation keeps the cached data and marks a refetch.
        assert!(!state.stale);
        state.invalidate();
        assert!(state.stale);
        assert!(state.data.is_some());
        state.begin(key(1));
        assert!(!state.stale, "beginning the refetch clears the flag");
        assert!(state.data.is_some(), "cached data stays while refreshing");
        assert_eq!(state.fetches, 2);
        state.invalidate();
        assert!(!state.stale, "ignored while a fetch is in flight");
        // ...and a failure keeps the data too, with the error recorded.
        assert!(state.finish(&key(1), Err("boom".into())));
        assert_eq!(state.status, LoadStatus::Failed("boom".into()));
        assert!(state.data.is_some());

        // A reconnect of the same table keeps the data visible.
        state.begin(key(2));
        assert!(state.data.is_some());
        // A different table drops the data.
        let mut other = key(2);
        other.object = "customers".into();
        state.begin(other);
        assert!(state.data.is_none());
        assert_eq!(state.visible_tabs().collect::<Vec<_>>(), [PropertiesTab::Columns]);
    }

    #[test]
    fn sidebar_nodes_cover_the_tables_below_them() {
        let key = key(1);
        assert!(key.covered_by("c", None, None, None), "connection node");
        assert!(key.covered_by("c", Some("d"), None, None), "database node");
        assert!(key.covered_by("c", Some("d"), Some("s"), Some("orders")), "the table itself");
        assert!(!key.covered_by("other", None, None, None));
        assert!(!key.covered_by("c", Some("x"), None, None));
        assert!(!key.covered_by("c", Some("d"), Some("s"), Some("customers")));
    }
}
