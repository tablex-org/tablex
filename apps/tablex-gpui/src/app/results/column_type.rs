//! One column-type resolver for every surface that names a result column's
//! type: the grid header, the cell-detail panel and the Properties Columns
//! tab. Priority, highest first:
//!
//! 1. the catalog type (`format_type`-style, with typmods such as
//!    `timestamp(3) with time zone`) when the result is table-backed and the
//!    catalog knows the column;
//! 2. the engine-provided native type (Arrow field metadata
//!    `tablex.native_type`; base type only, as RowDescription carries no
//!    typmods);
//! 3. the Arrow logical type (`int64`, `string`, ...) as the last fallback.
//!
//! Mongo has no catalog schema and its source already reports BSON types, so
//! it skips (1). Labels for the header are cached per result: the cache is
//! rebuilt only when the result, catalog or query generation changes.

use super::super::*;
use super::TabView;
use gpui::SharedString;

/// Display family of a column type, shared by header, detail and Properties
/// tinting. Understands Arrow names (`int64`), BSON names (`objectId`) and
/// native SQL names (`bigint`, `timestamp(3) with time zone`, `text[]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ColumnTypeCategory {
    Number,
    Boolean,
    Temporal,
    Object,
    Binary,
    Uuid,
    Text,
}

pub(in crate::app) fn column_type_category(type_name: &str) -> ColumnTypeCategory {
    use ColumnTypeCategory::*;
    let lower = type_name.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if lower.ends_with("[]") || has(&["array", "object", "json", "map"]) {
        return Object;
    }
    if has(&["bool"]) {
        return Boolean;
    }
    // Before numbers: `interval` contains "int".
    if has(&["date", "time", "interval"]) {
        return Temporal;
    }
    if has(&["uuid"]) {
        return Uuid;
    }
    if has(&["bytea", "binary", "blob"]) {
        return Binary;
    }
    if (has(&["int"]) && !has(&["point"]))
        || has(&[
            "float", "double", "decimal", "numeric", "real", "money", "serial", "number",
        ])
    {
        return Number;
    }
    Text
}

/// What the editor does with text typed into a column, decided on the column's
/// BASE type (see [`base_type_head`]), never by looking for a word inside the
/// type text: `serial_number` (a domain over varchar), `interval`, `int4range`,
/// `enum('internal')` and `integer[]` are text for the server to coerce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum ValueKind {
    /// Whole numbers of any width (`Int128`, numeric keys).
    Integer,
    /// `numeric` / `decimal`: exact digits, plus `NaN` / `Infinity`.
    Decimal,
    /// `real` / `float` / `double` (and the Arrow `number` label): exact digits,
    /// exponents, `NaN`, `Infinity`, `-0`.
    Float,
    Boolean,
    /// Dates and times: an empty editor is NULL, the text goes to the server.
    Temporal,
    /// Byte strings of a SQL result, written `0x` + hex.
    Binary,
    Json,
    /// Everything else, unknown and user-defined types included.
    Text,
}

/// The lower-case base type of `type_name`: wrappers (`Nullable(...)`,
/// `LowCardinality(...)`), parameters (`decimal(10,2)`, `enum('a','b')`,
/// `DateTime('America/Montreal')`) and the `unsigned` / `signed` / `zerofill`
/// modifiers are gone; an array keeps its `[]` / `array(` marker, so it can
/// never read as its element type.
pub(in crate::app) fn base_type_head(type_name: &str) -> String {
    let lower = type_name.trim().to_ascii_lowercase();
    let mut name = lower.as_str();
    while let Some(open) = name.find('(') {
        let inner = matches!(name[..open].trim(), "nullable" | "lowcardinality")
            .then(|| name[open + 1..].trim_end().strip_suffix(')'))
            .flatten();
        match inner {
            Some(inner) => name = inner.trim(),
            None => break,
        }
    }
    let array = name.ends_with(']');
    let head = name.split('(').next().unwrap_or_default();
    let mut words = head
        .split_whitespace()
        .filter(|word| !matches!(*word, "unsigned" | "signed" | "zerofill"));
    let first = words.next().unwrap_or_default();
    let mut base = match (first, words.next()) {
        ("big", Some("int")) => "bigint".to_string(),
        ("double", _) => "double".to_string(),
        (first, _) => first.trim_end_matches("[]").to_string(),
    };
    if array || name.starts_with("array(") {
        base.push_str("[]");
    }
    base
}

pub(in crate::app) fn value_kind(type_name: &str) -> ValueKind {
    match base_type_head(type_name).as_str() {
        "int" | "integer" | "tinyint" | "smallint" | "mediumint" | "bigint" | "int1" | "int2"
        | "int3" | "int4" | "int8" | "int16" | "int32" | "int64" | "int128" | "int256"
        | "uint8" | "uint16" | "uint32" | "uint64" | "uint128" | "uint256" | "serial"
        | "smallserial" | "bigserial" | "serial2" | "serial4" | "serial8" => ValueKind::Integer,
        "numeric" | "decimal" | "dec" | "fixed" | "decimal32" | "decimal64" | "decimal128"
        | "decimal256" => ValueKind::Decimal,
        "real" | "float" | "float4" | "float8" | "float32" | "float64" | "double" | "number" => {
            ValueKind::Float
        }
        "bool" | "boolean" => ValueKind::Boolean,
        "date" | "date32" | "time" | "timetz" | "timestamp" | "timestamptz" | "datetime"
        | "datetime2" | "datetime64" | "smalldatetime" => ValueKind::Temporal,
        "blob" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" => {
            ValueKind::Binary
        }
        "json" | "jsonb" | "object" | "array" => ValueKind::Json,
        _ => ValueKind::Text,
    }
}

pub(in crate::app) fn grid_column_type_color(data_type: &str) -> gpui::Rgba {
    match column_type_category(data_type) {
        ColumnTypeCategory::Number => theme::type_number(),
        ColumnTypeCategory::Boolean => theme::accent(),
        ColumnTypeCategory::Temporal => theme::warning(),
        ColumnTypeCategory::Object => theme::type_object(),
        ColumnTypeCategory::Binary | ColumnTypeCategory::Uuid | ColumnTypeCategory::Text => {
            theme::type_text()
        }
    }
}

/// Pure resolver: catalog (exact name, then ASCII case-insensitive), then the
/// engine's native type, then the Arrow logical type. `catalog` is empty for
/// ad-hoc queries and Mongo.
pub(in crate::app) fn resolve_column_type<'a>(
    catalog: &'a [crate::catalog::CatalogColumn],
    name: &str,
    native: Option<&'a str>,
    logical: Option<&'a str>,
) -> Option<&'a str> {
    let column = catalog
        .iter()
        .find(|column| column.name == name)
        .or_else(|| {
            catalog
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(name))
        });
    column
        .map(|column| column.data_type.as_str())
        .filter(|data_type| !data_type.is_empty())
        .or(native.filter(|native| !native.is_empty()))
        .or(logical.filter(|logical| !logical.is_empty()))
}

/// Resolved header labels for one result, valid while the key holds. The
/// retained `grid` clone keeps the source allocation alive, so the pointer
/// comparison cannot be fooled by address reuse.
pub(in crate::app) struct ColumnTypeLabels {
    generation: u64,
    page_id: Option<u64>,
    mongo: bool,
    catalog_ptr: usize,
    catalog_len: usize,
    catalog_generation: u64,
    grid: GridState<GridContentSource>,
    labels: Vec<Option<SharedString>>,
    /// Per source column: the label's category is [`ColumnTypeCategory::Number`].
    /// Classified once per result so the per-cell alignment check is a lookup.
    numeric: Vec<bool>,
}

impl ColumnTypeLabels {
    fn matches(
        &self,
        runtime: &crate::workspace::TabRuntime,
        mongo: bool,
        grid: &GridState<GridContentSource>,
    ) -> bool {
        self.generation == runtime.query.generation()
            && self.page_id == runtime.driver_page_id
            && self.mongo == mongo
            && self.catalog_ptr == runtime.structure_columns.as_ptr() as usize
            && self.catalog_len == runtime.structure_columns.len()
            && self.catalog_generation == runtime.structure_generation
            && self.grid.same_source(grid)
    }
}

impl TabView {
    /// Whether a source column holds numbers (right-aligned in the grid), from
    /// the same cached labels as the header. `false` when no type is known.
    pub(in crate::app) fn column_is_numeric(&self, source_column: usize) -> bool {
        self.grid_ui
            .column_types
            .as_ref()
            .and_then(|cache| cache.numeric.get(source_column))
            .copied()
            .unwrap_or(false)
    }

    /// Cached header label for a source column; `text` when no source knows.
    pub(in crate::app) fn header_column_type(&self, source_column: usize) -> SharedString {
        self.grid_ui
            .column_types
            .as_ref()
            .and_then(|cache| cache.labels.get(source_column))
            .and_then(|label| label.clone())
            .unwrap_or_else(|| SharedString::new_static("text"))
    }
}

impl AppRoot {
    fn tab_is_mongo(&self, tab_id: u64) -> bool {
        self.workspace
            .tab(tab_id)
            .is_some_and(|tab| tab.query_mode.is_mongo_find())
    }

    fn resolve_column_type_uncached(
        &self,
        tab_id: u64,
        source_column: usize,
        mongo: bool,
    ) -> Option<String> {
        let grid = self.grid(tab_id);
        let name = grid.column_name(source_column)?;
        let catalog: &[crate::catalog::CatalogColumn] = if mongo {
            &[]
        } else {
            self.workspace
                .runtime(tab_id)
                .map_or(&[], |runtime| runtime.structure_columns.as_slice())
        };
        resolve_column_type(
            catalog,
            name,
            grid.native_type(source_column),
            grid.column_type(source_column),
        )
        .map(str::to_string)
    }

    /// The type label for `source_column` of the tab's result. Reads the
    /// per-result cache when it is current, otherwise resolves directly; both
    /// go through [`resolve_column_type`], so header, detail and Properties
    /// always agree.
    pub(in crate::app) fn resolved_column_type(
        &self,
        tab_id: u64,
        source_column: usize,
    ) -> Option<String> {
        let mongo = self.tab_is_mongo(tab_id);
        if let (Some(cache), Some(runtime)) = (
            self.view(tab_id).grid_ui.column_types.as_ref(),
            self.workspace.runtime(tab_id),
        ) {
            if cache.matches(runtime, mongo, self.grid(tab_id)) {
                return cache
                    .labels
                    .get(source_column)
                    .and_then(|label| label.as_ref())
                    .map(|label| label.to_string());
            }
        }
        self.resolve_column_type_uncached(tab_id, source_column, mongo)
    }

    /// Rebuilds the header label cache when the result, catalog or query
    /// generation changed; a key comparison otherwise (never per-frame work
    /// proportional to the column count).
    pub(in crate::app) fn sync_column_type_labels(&mut self, tab_id: u64) {
        let mongo = self.tab_is_mongo(tab_id);
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        let grid = &runtime.grid;
        if self
            .view(tab_id)
            .grid_ui
            .column_types
            .as_ref()
            .is_some_and(|cache| cache.matches(runtime, mongo, grid))
        {
            return;
        }
        let labels = (0..grid.column_count())
            .map(|column| {
                self.resolve_column_type_uncached(tab_id, column, mongo)
                    .map(SharedString::from)
            })
            .collect::<Vec<Option<SharedString>>>();
        let numeric = labels
            .iter()
            .map(|label| {
                label
                    .as_ref()
                    .is_some_and(|label| column_type_category(label) == ColumnTypeCategory::Number)
            })
            .collect();
        let cache = ColumnTypeLabels {
            generation: runtime.query.generation(),
            page_id: runtime.driver_page_id,
            mongo,
            catalog_ptr: runtime.structure_columns.as_ptr() as usize,
            catalog_len: runtime.structure_columns.len(),
            catalog_generation: runtime.structure_generation,
            grid: grid.clone(),
            labels,
            numeric,
        };
        self.view_mut(tab_id).grid_ui.column_types = Some(cache);
    }
}
