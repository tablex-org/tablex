//! Pure model of the "Columns" presentation of a result (left
//! rail tab, exclusive with the grid): one row per displayed grid column,
//! filtering and type presentation. Built once per result/schema change and
//! filtered only when the query text changes.

use std::sync::Arc;

/// Which presentation of the result fills the grid area.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Presentation {
    #[default]
    Grid,
    Columns,
    /// ER diagram of the table and its foreign-key neighbours.
    Diagram,
    /// Explain plan of the last Explain run (see `plan_view`).
    Plan,
    /// The result as a bar / line / area / scatter / pie chart (see `app::results::chart`).
    Chart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnItem {
    /// Index among the displayed grid columns (what selection/scroll use).
    pub display_index: usize,
    pub name: String,
    /// Column comment when the engine provides one (none do today).
    pub label: String,
    /// Default expression from the catalog, when known.
    pub default: Option<String>,
    pub type_name: String,
    pub primary_key: bool,
    /// `None` when no schema metadata is available (e.g. Mongo).
    pub nullable: Option<bool>,
    /// Hidden in the grid by the Columns picker: still listed (Properties
    /// describes the whole table) but dimmed and not selectable, since it
    /// has no grid column. Its `display_index` is a unique placeholder.
    pub hidden: bool,
}

/// Sub-tabs of the Properties presentation (horizontal underline strip).
/// Columns always exists; the list tabs and DDL are shown only when the
/// driver reported them (`TableProperties` field is `Some`). Mongo Validation /
/// Stats are added the same way (variant + `label` + `selector` + `available`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PropertiesTab {
    #[default]
    Columns,
    Keys,
    ForeignKeys,
    Indexes,
    References,
    Triggers,
    Ddl,
}

impl PropertiesTab {
    /// Display order of the sub-tab strip.
    pub const ALL: [PropertiesTab; 7] = [
        PropertiesTab::Columns,
        PropertiesTab::Keys,
        PropertiesTab::ForeignKeys,
        PropertiesTab::Indexes,
        PropertiesTab::References,
        PropertiesTab::Triggers,
        PropertiesTab::Ddl,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Columns => "Columns",
            Self::Keys => "Keys",
            Self::ForeignKeys => "Foreign Keys",
            Self::Indexes => "Indexes",
            Self::References => "References",
            Self::Triggers => "Triggers",
            Self::Ddl => "DDL",
        }
    }

    /// Debug selector of the sub-tab strip entry.
    pub fn selector(self) -> &'static str {
        match self {
            Self::Columns => "properties-tab-columns",
            Self::Keys => "properties-tab-keys",
            Self::ForeignKeys => "properties-tab-foreign-keys",
            Self::Indexes => "properties-tab-indexes",
            Self::References => "properties-tab-references",
            Self::Triggers => "properties-tab-triggers",
            Self::Ddl => "properties-tab-ddl",
        }
    }

    /// Whether the sub-tab is shown for the driver-reported `properties`
    /// (`None` = nothing fetched yet or fetch failed: only Columns).
    pub fn available(self, properties: Option<&crate::catalog::TableProperties>) -> bool {
        match self {
            Self::Columns => true,
            Self::Keys => properties.is_some_and(|p| p.keys.is_some()),
            Self::ForeignKeys => properties.is_some_and(|p| p.foreign_keys.is_some()),
            Self::Indexes => properties.is_some_and(|p| p.indexes.is_some()),
            Self::References => properties.is_some_and(|p| p.references.is_some()),
            Self::Triggers => properties.is_some_and(|p| p.triggers.is_some()),
            Self::Ddl => properties.is_some_and(|p| p.ddl.is_some()),
        }
    }
}

/// Pixel widths of the Columns table's columns (0 = column not shown).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColumnWidths {
    pub name: f32,
    pub type_name: f32,
    pub nullable: f32,
    pub key: f32,
    pub default: f32,
    pub comment: f32,
}

impl ColumnWidths {
    pub fn total(&self) -> f32 {
        self.name + self.type_name + self.nullable + self.key + self.default + self.comment
    }
}

pub const CELL_PADDING: f32 = 10.0;
pub const NAME_MAX: f32 = 320.0;
pub const TYPE_MAX: f32 = 220.0;
pub const DEFAULT_MAX: f32 = 260.0;
pub const COMMENT_MAX: f32 = 360.0;
/// Space the type icon (and gap) takes inside the Name cell.
pub const NAME_ICON: f32 = 18.0;
/// Space a key marker (icon + "PK") takes in the Key cell.
pub const KEY_CELL: f32 = 30.0;

/// Sizes every visible column to `max(header, widest cell) + padding`, capped.
/// `measure(text, header)` returns a text width in px (`header` selects the
/// smaller header font). Runs once per item-list change, never per frame.
pub fn compute_widths(
    items: &[ColumnItem],
    flags: (bool, bool, bool, bool),
    measure: &dyn Fn(&str, bool) -> f32,
) -> ColumnWidths {
    let (has_nullable, has_key, has_default, has_comment) = flags;
    let widest = |header: &str, cell: &dyn Fn(&ColumnItem) -> String| {
        items
            .iter()
            .map(|item| measure(&cell(item), false))
            .fold(measure(header, true), f32::max)
    };
    let padded = |width: f32, extra: f32, max: f32| (width + extra + 2.0 * CELL_PADDING).min(max);
    ColumnWidths {
        name: padded(widest("Name", &|item| item.name.clone()), NAME_ICON, NAME_MAX),
        type_name: padded(widest("Type", &|item| item.type_name.clone()), 0.0, TYPE_MAX),
        nullable: if has_nullable {
            padded(widest("Nullable", &|item| nullable_text(item).into()), 0.0, 120.0)
        } else {
            0.0
        },
        key: if has_key {
            padded(measure("Key", true).max(KEY_CELL), 0.0, 120.0)
        } else {
            0.0
        },
        default: if has_default {
            padded(
                widest("Default", &|item| item.default.clone().unwrap_or_default()),
                0.0,
                DEFAULT_MAX,
            )
        } else {
            0.0
        },
        comment: if has_comment {
            padded(widest("Comment", &|item| item.label.clone()), 0.0, COMMENT_MAX)
        } else {
            0.0
        },
    }
}

pub fn nullable_text(item: &ColumnItem) -> &'static str {
    match item.nullable {
        Some(true) => "Yes",
        Some(false) => "No",
        None => "",
    }
}

/// Per-result-tab panel state.
#[derive(Debug, Clone)]
pub struct ColumnsPanelState {
    pub presentation: Presentation,
    /// Selected Properties sub-tab (per result tab).
    pub properties_tab: PropertiesTab,
    /// Selected row, as a display column index.
    pub selected: Option<usize>,
    pub filter: String,
    /// Rebuild key of `items` (see `AppRoot::sync_columns_panel`).
    pub key: Option<ColumnsKey>,
    pub items: Arc<Vec<ColumnItem>>,
    /// Indices into `items` matching `filter`, in order.
    pub filtered: Arc<Vec<usize>>,
    /// The filter `filtered` was computed for.
    pub filtered_for: String,
    /// Optional table columns render only when some row has a known value.
    pub has_nullable: bool,
    pub has_default: bool,
    pub has_comment: bool,
    pub has_key: bool,
    /// Content-sized column widths for the current item list.
    pub widths: ColumnWidths,
    /// Fetched table properties (keys, indexes, ...) and their list models.
    pub props: crate::properties_model::PropertiesState,
}

impl Default for ColumnsPanelState {
    fn default() -> Self {
        Self {
            presentation: Presentation::Grid,
            properties_tab: PropertiesTab::default(),
            selected: None,
            filter: String::new(),
            key: None,
            items: Arc::default(),
            filtered: Arc::default(),
            filtered_for: String::new(),
            has_nullable: false,
            has_default: false,
            has_comment: false,
            has_key: false,
            widths: ColumnWidths::default(),
            props: Default::default(),
        }
    }
}

/// Everything the item list depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnsKey {
    pub generation: u64,
    pub page_id: Option<u64>,
    pub columns: usize,
    pub schema_columns: usize,
    pub visible: usize,
}

impl ColumnsPanelState {
    pub fn set_items(&mut self, key: ColumnsKey, items: Vec<ColumnItem>) {
        self.key = Some(key);
        self.has_nullable = items.iter().any(|item| item.nullable.is_some());
        self.has_default = items.iter().any(|item| item.default.is_some());
        self.has_comment = items.iter().any(|item| !item.label.is_empty());
        self.has_key = items.iter().any(|item| item.primary_key);
        self.items = Arc::new(items);
        self.refilter();
    }

    /// Recomputes the content-sized column widths for the current items.
    pub fn measure_widths(&mut self, measure: &dyn Fn(&str, bool) -> f32) {
        self.widths = compute_widths(
            &self.items,
            (self.has_nullable, self.has_key, self.has_default, self.has_comment),
            measure,
        );
    }

    /// Recomputes `filtered` when the query changed (or items were replaced).
    pub fn refilter(&mut self) {
        self.filtered = Arc::new(filter_items(&self.items, &self.filter));
        self.filtered_for = self.filter.clone();
    }

    /// Position of `display_index` among the filtered rows.
    pub fn position_of(&self, display_index: usize) -> Option<usize> {
        self.filtered
            .iter()
            .position(|&item| self.items[item].display_index == display_index)
    }

    /// The display column `delta` rows away from the selection within the
    /// filtered list (clamped); the first row when nothing is selected.
    /// Hidden columns have no grid column and are skipped.
    pub fn step_selection(&self, delta: isize) -> Option<usize> {
        let selectable: Vec<usize> = self
            .filtered
            .iter()
            .copied()
            .filter(|&item| !self.items[item].hidden)
            .collect();
        if selectable.is_empty() {
            return None;
        }
        let current = self.selected.and_then(|selected| {
            selectable
                .iter()
                .position(|&item| self.items[item].display_index == selected)
        });
        let target = match current {
            Some(position) => {
                (position as isize + delta).clamp(0, selectable.len() as isize - 1) as usize
            }
            None => 0,
        };
        Some(self.items[selectable[target]].display_index)
    }

    pub fn filter_is_stale(&self) -> bool {
        self.filtered_for != self.filter
    }
}

/// Case-insensitive match against name, label and type; blank queries match all.
pub fn filter_items(items: &[ColumnItem], query: &str) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            query.is_empty()
                || [&item.name, &item.label, &item.type_name]
                    .iter()
                    .any(|field| field.to_lowercase().contains(&query))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Icon (asset path) for a column type: number, calendar, checkbox, object, text.
pub fn type_icon(type_name: &str) -> &'static str {
    let lower = type_name.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if has(&["object_id", "objectid"]) {
        "icons/type.svg"
    } else if has(&["int", "number", "decimal", "double", "float", "numeric", "real", "money"]) {
        "icons/hash.svg"
    } else if has(&["date", "time"]) {
        "icons/calendar-clock.svg"
    } else if has(&["bool"]) {
        "icons/check.svg"
    } else if has(&["json", "array", "object", "document", "map"]) {
        "icons/square-dashed.svg"
    } else {
        "icons/type.svg"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(index: usize, name: &str, label: &str, type_name: &str) -> ColumnItem {
        ColumnItem {
            display_index: index,
            name: name.into(),
            label: label.into(),
            default: None,
            type_name: type_name.into(),
            primary_key: false,
            nullable: None,
            hidden: false,
        }
    }

    fn sample() -> Vec<ColumnItem> {
        vec![
            item(0, "id", "", "int64"),
            item(1, "customer_name", "Customer", "varchar"),
            item(2, "created_at", "", "timestamp"),
            item(3, "payload", "", "jsonb"),
        ]
    }

    #[test]
    fn blank_queries_match_everything() {
        assert_eq!(filter_items(&sample(), ""), vec![0, 1, 2, 3]);
        assert_eq!(filter_items(&sample(), "   "), vec![0, 1, 2, 3]);
    }

    #[test]
    fn filter_matches_name_label_and_type_case_insensitively() {
        let items = sample();
        assert_eq!(filter_items(&items, "NAME"), vec![1]);
        assert_eq!(filter_items(&items, "customer"), vec![1]);
        assert_eq!(filter_items(&items, "Int"), vec![0]);
        assert_eq!(filter_items(&items, "  json "), vec![3]);
        assert_eq!(filter_items(&items, "_"), vec![1, 2]);
        assert!(filter_items(&items, "zzz").is_empty());
    }

    #[test]
    fn refilter_only_when_the_query_changed() {
        let mut state = ColumnsPanelState::default();
        state.set_items(
            ColumnsKey { generation: 1, page_id: None, columns: 4, schema_columns: 0, visible: 4 },
            sample(),
        );
        assert!(!state.filter_is_stale());
        assert_eq!(state.filtered.len(), 4);
        assert!(state.has_comment && !state.has_default && !state.has_nullable);
        state.filter = "id".into();
        assert!(state.filter_is_stale());
        state.refilter();
        assert!(!state.filter_is_stale());
        assert_eq!(*state.filtered, vec![0]);
    }

    #[test]
    fn selection_steps_within_the_filtered_rows() {
        let mut state = ColumnsPanelState::default();
        state.set_items(
            ColumnsKey { generation: 1, page_id: None, columns: 4, schema_columns: 0, visible: 4 },
            sample(),
        );
        assert_eq!(state.step_selection(1), Some(0), "nothing selected: first row");
        state.selected = Some(0);
        assert_eq!(state.step_selection(1), Some(1));
        assert_eq!(state.step_selection(-1), Some(0), "clamped at the top");
        state.selected = Some(3);
        assert_eq!(state.step_selection(1), Some(3), "clamped at the bottom");
        state.filter = "_".into();
        state.refilter();
        state.selected = Some(1);
        assert_eq!(state.step_selection(1), Some(2), "skips filtered-out rows");
        assert_eq!(state.position_of(3), None);
    }

    #[test]
    fn selection_steps_skip_columns_hidden_in_the_grid() {
        let mut items = sample();
        // `customer_name` is hidden: listed, but it has no grid column.
        items[1].hidden = true;
        items[1].display_index = 100;
        let mut state = ColumnsPanelState::default();
        state.set_items(
            ColumnsKey { generation: 1, page_id: None, columns: 4, schema_columns: 0, visible: 3 },
            items,
        );
        assert_eq!(state.filtered.len(), 4, "hidden columns stay in the list");
        state.selected = Some(0);
        assert_eq!(state.step_selection(1), Some(2), "down skips the hidden row");
        state.selected = Some(2);
        assert_eq!(state.step_selection(-1), Some(0), "up skips it too");
        assert_eq!(state.step_selection(1), Some(3));
        state.selected = None;
        assert_eq!(state.step_selection(1), Some(0));
        state.filter = "customer".into();
        state.refilter();
        assert_eq!(state.step_selection(1), None, "only a hidden row matches");
    }

    fn fake_measure(text: &str, header: bool) -> f32 {
        text.chars().count() as f32 * if header { 6.0 } else { 7.0 }
    }

    #[test]
    fn column_widths_fit_content_and_hide_unknown_columns() {
        let mut items = sample();
        items[0].primary_key = true;
        items[1].nullable = Some(true);
        items[3].default = Some("'{}'::jsonb".into());
        items[2].nullable = Some(false);
        items[1].label = String::new();
        let mut state = ColumnsPanelState::default();
        state.set_items(
            ColumnsKey { generation: 1, page_id: None, columns: 4, schema_columns: 4, visible: 4 },
            items,
        );
        state.measure_widths(&fake_measure);
        let widths = state.widths;
        // Name: widest cell ("customer_name" = 13 chars) + icon + padding.
        assert_eq!(widths.name, 13.0 * 7.0 + NAME_ICON + 2.0 * CELL_PADDING);
        assert_eq!(widths.type_name, 9.0 * 7.0 + 2.0 * CELL_PADDING, "'timestamp' is the widest type");
        assert!(widths.nullable > 0.0 && widths.key > 0.0 && widths.default > 0.0);
        assert_eq!(widths.comment, 0.0, "labels are empty -> no Comment column");
        assert_eq!(
            widths.total(),
            widths.name + widths.type_name + widths.nullable + widths.key + widths.default
        );
    }

    #[test]
    fn column_widths_are_capped_and_never_below_the_header() {
        let mut items = sample();
        items[0].name = "n".repeat(500);
        items[1].label = "c".repeat(500);
        items[2].type_name = "t".into();
        let mut state = ColumnsPanelState::default();
        state.set_items(
            ColumnsKey { generation: 1, page_id: None, columns: 4, schema_columns: 0, visible: 4 },
            items,
        );
        state.measure_widths(&fake_measure);
        assert_eq!(state.widths.name, NAME_MAX);
        assert_eq!(state.widths.comment, COMMENT_MAX);
        // "Type" header (4 chars) vs longest type ("timestamp"): the cell wins, never below header.
        assert!(state.widths.type_name >= 4.0 * 6.0 + 2.0 * CELL_PADDING);
    }

    #[test]
    fn properties_tabs_have_labels_icons_and_unique_selectors() {
        assert_eq!(PropertiesTab::default(), PropertiesTab::Columns);
        assert_eq!(PropertiesTab::ALL.len(), 7);
        for tab in PropertiesTab::ALL {
            assert!(!tab.label().is_empty());
        }
        assert_eq!(PropertiesTab::ForeignKeys.label(), "Foreign Keys");
        assert_eq!(PropertiesTab::Ddl.label(), "DDL");
        let selectors: std::collections::HashSet<_> =
            PropertiesTab::ALL.iter().map(|tab| tab.selector()).collect();
        assert_eq!(selectors.len(), PropertiesTab::ALL.len());
    }

    #[test]
    fn tabs_are_available_only_when_the_driver_reports_them() {
        use crate::catalog::TableProperties;
        assert!(PropertiesTab::Columns.available(None));
        for tab in &PropertiesTab::ALL[1..] {
            assert!(!tab.available(None), "{tab:?}");
            assert!(!tab.available(Some(&TableProperties::default())), "{tab:?}");
        }
        let props = TableProperties {
            keys: Some(Vec::new()),
            indexes: Some(Vec::new()),
            ddl: Some("CREATE TABLE t ()".into()),
            ..Default::default()
        };
        let shown: Vec<_> = PropertiesTab::ALL
            .into_iter()
            .filter(|tab| tab.available(Some(&props)))
            .collect();
        assert_eq!(
            shown,
            [PropertiesTab::Columns, PropertiesTab::Keys, PropertiesTab::Indexes, PropertiesTab::Ddl]
        );
    }

    #[test]
    fn type_icons_follow_the_type_family() {
        assert_eq!(type_icon("BIGINT"), "icons/hash.svg");
        assert_eq!(type_icon("decimal128"), "icons/hash.svg");
        assert_eq!(type_icon("date_time"), "icons/calendar-clock.svg");
        assert_eq!(type_icon("boolean"), "icons/check.svg");
        assert_eq!(type_icon("document"), "icons/square-dashed.svg");
        assert_eq!(type_icon("varchar(20)"), "icons/type.svg");
        assert_eq!(type_icon("object_id"), "icons/type.svg");
    }
}
