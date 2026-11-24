//! Row counts, sizes and comments of the tables in the tree: the dimmed estimate at the right of a table row
//! (Settings > Sidebar > "Show table sizes") and the tooltip of a table, view or materialized view row.
//!
//! The driver's catalog listing carries none of it, so it is read from the server's own catalog, off the UI thread, on a session
//! of its own (the one a structure apply uses, never a SQL tab's), **in one statement per namespace** (never per row, never a
//! `COUNT(*)`):
//!
//! * PostgreSQL: `pg_class.reltuples`, `pg_total_relation_size`, `obj_description`;
//! * MySQL and MariaDB: `information_schema.TABLES` (`TABLE_ROWS`, `DATA_LENGTH + INDEX_LENGTH`, `TABLE_COMMENT`);
//! * ClickHouse: `system.tables` (`total_rows`, `total_bytes`, `comment`);
//! * SQLite has no estimate and MongoDB's collections are not read this way: no row asks for either.
//!
//! The estimates of a namespace are cached; they are read when a table row of it is first drawn with the setting on, or first
//! hovered, never for a folder nobody expands. A reload of one of the namespace's folders drops them (the entry remembers the
//! load generation of the folders it was read for), and the next draw or hover reads them again. An unknown estimate (PostgreSQL's
//! `reltuples = -1`, a NULL) shows nothing.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, Pixels, SharedString};

use super::*;
use crate::sql_account::catalog::{CatalogSession, ReadError, TextRows};
use crate::sql_ddl::{dialect_for, literal};

/// What the catalog says about one table, view or materialized view; every part may be unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableStats {
    pub(super) rows: Option<u64>,
    pub(super) bytes: Option<u64>,
    pub(super) comment: Option<String>,
    /// `1.2M rows · 45 MB`, what the row shows (`None`: nothing is known of either).
    pub(super) label: Option<SharedString>,
    /// `1.2M rows`: the label without the size, what a row too narrow for the whole label shows (`None`: the label has no size
    /// to drop).
    pub(super) short_label: Option<SharedString>,
}

impl TableStats {
    fn new(rows: Option<u64>, bytes: Option<u64>, comment: Option<String>) -> Self {
        let comment = comment.map(|comment| comment.trim().chars().take(COMMENT_LIMIT).collect::<String>()).filter(|comment| !comment.is_empty());
        let label = match (rows, bytes) {
            (None, None) => None,
            (rows, bytes) => Some(
                rows.map(format_rows).into_iter().chain(bytes.map(format_bytes)).collect::<Vec<_>>().join(" \u{00B7} ").into(),
            ),
        };
        let short_label = rows.zip(bytes).map(|(rows, _)| SharedString::from(format_rows(rows)));
        Self { rows, bytes, comment, label, short_label }
    }

    /// Nothing worth a tooltip.
    pub(super) fn is_empty(&self) -> bool {
        self.rows.is_none() && self.bytes.is_none() && self.comment.is_none()
    }

    /// The lines of the tooltip: the comment, then the estimates.
    pub(super) fn tooltip_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.extend(self.comment.clone());
        if let Some(rows) = self.rows {
            lines.push(t!("sidebar.stats_rows", rows = format_rows(rows)).into_owned());
        }
        if let Some(bytes) = self.bytes {
            lines.push(t!("sidebar.stats_size", size = format_bytes(bytes)).into_owned());
        }
        lines
    }
}

/// Room kept on each side of the estimate at the right of a row.
const LABEL_PAD: f32 = 8.0;

/// The dimmed estimate at the right edge of a table row, which never takes room from the name. It asks for the room of its whole
/// label but gives it up first (the shrink factor of `DETAIL_SHRINK`, far above the name's) and draws what fits of what it was
/// given: the whole label, else the rows alone, else nothing. One shaping of the two strings per visible row (the text system
/// caches them), no state of its own.
pub(super) struct StatsLabel {
    label: SharedString,
    short: Option<SharedString>,
}

impl StatsLabel {
    pub(super) fn new(stats: &TableStats) -> Option<Self> {
        Some(Self { label: stats.label.clone()?, short: stats.short_label.clone() })
    }
}

/// The first of `widths` (widest first) that fits `available` with the padding on both sides.
fn fitting(widths: impl IntoIterator<Item = Pixels>, available: Pixels) -> Option<usize> {
    widths.into_iter().position(|width| width + px(2.0 * LABEL_PAD) <= available)
}

pub(super) struct StatsShaping {
    lines: Vec<gpui::ShapedLine>,
    line_height: Pixels,
}

impl gpui::Element for StatsLabel {
    type RequestLayoutState = StatsShaping;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (gpui::LayoutId, StatsShaping) {
        let mut text_style = window.text_style();
        text_style.font_size = px(10.0).into();
        let font_size = px(10.0);
        let line_height = text_style.line_height_in_pixels(window.rem_size());
        let lines: Vec<_> = std::iter::once(&self.label)
            .chain(self.short.as_ref())
            .map(|text| {
                let run = gpui::TextRun { color: theme::text_faint().into(), ..text_style.to_run(text.len()) };
                window.text_system().shape_line(text.clone(), font_size, &[run], None)
            })
            .collect();
        let natural = lines[0].width + px(2.0 * LABEL_PAD);
        let mut style = gpui::Style { flex_grow: 1.0, flex_shrink: 1.0, ..Default::default() };
        style.min_size.width = px(0.0).into();
        let id = window.request_measured_layout(style, move |known, _, _, _| {
            gpui::size(known.width.unwrap_or(natural), known.height.unwrap_or(line_height))
        });
        (id, StatsShaping { lines, line_height })
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<Pixels>,
        _shaping: &mut StatsShaping,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        shaping: &mut StatsShaping,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(ix) = fitting(shaping.lines.iter().map(|line| line.width), bounds.size.width) else { return };
        let line = &shaping.lines[ix];
        let origin = gpui::point(bounds.right() - px(LABEL_PAD) - line.width, bounds.top());
        let _ = line.paint(origin, shaping.line_height, gpui::TextAlign::Left, None, window, cx);
    }
}

/// The longest comment a tooltip shows.
const COMMENT_LIMIT: usize = 400;

/// `950 rows`, `12K rows`, `1.2M rows`.
pub(super) fn format_rows(rows: u64) -> String {
    if rows == 1 {
        return "1 row".to_owned();
    }
    const UNITS: [&str; 4] = ["", "K", "M", "B"];
    let (mut value, mut unit) = (rows as f64, 0);
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    let mut text = match unit {
        0 => rows.to_string(),
        _ if value < 10.0 => format!("{value:.1}"),
        _ => format!("{value:.0}"),
    };
    // 999.9K rounds to 1000K: that is 1M.
    if text == "1000" && unit < UNITS.len() - 1 {
        text = "1".to_owned();
        unit += 1;
    }
    format!("{}{} rows", text.strip_suffix(".0").unwrap_or(&text), UNITS[unit])
}

/// `512 B`, `45 MB`, `1.2 GB` (binary units).
pub(super) fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut value, mut unit) = (bytes as f64, 0);
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{bytes} B"),
        _ if value < 10.0 => format!("{value:.1} {}", UNITS[unit]),
        _ => format!("{value:.0} {}", UNITS[unit]),
    }
}

/// The statement that reads the estimates of every table and view of a namespace, or `None` for a kind that has none. The names it
/// carries are string literals of the dialect.
pub(super) fn stats_sql(kind: ConnectionKind, database: &str, schema: &str) -> Option<String> {
    let dialect = dialect_for(kind)?;
    // The namespace of MySQL, MariaDB and ClickHouse is the database; of PostgreSQL the schema.
    let place = Some(schema).filter(|schema| !schema.is_empty()).unwrap_or(database);
    Some(match kind {
        ConnectionKind::Postgres => format!(
            "SELECT c.relname, c.relkind::text, c.reltuples::bigint, pg_total_relation_size(c.oid), obj_description(c.oid, 'pg_class') \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {} AND c.relkind IN ('r', 'p', 'v', 'm', 'f')",
            literal(dialect, place)
        ),
        ConnectionKind::Mysql | ConnectionKind::Mariadb => format!(
            "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH, TABLE_COMMENT \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = {}",
            literal(dialect, place)
        ),
        ConnectionKind::Clickhouse => format!(
            "SELECT name, engine, total_rows, total_bytes, comment FROM system.tables WHERE database = {}",
            literal(dialect, place)
        ),
        ConnectionKind::Sqlite | ConnectionKind::Mongodb | ConnectionKind::Redis => return None,
    })
}

/// A count as the catalog prints it (`12`, `12.0`, `1.2e3`); a negative or unreadable one is unknown.
fn count(text: &Option<String>) -> Option<u64> {
    let value: f64 = text.as_deref()?.trim().parse().ok()?;
    (value.is_finite() && value >= 0.0).then_some(value as u64)
}

/// The estimates of the rows `stats_sql` answered, by table name. A row that is a view has no rows or size of its own.
pub(super) fn parse_stats(kind: ConnectionKind, rows: &TextRows) -> HashMap<String, Arc<TableStats>> {
    rows.iter()
        .filter_map(|row| {
            let name = row.first()?.clone()?;
            let kind_of = row.get(1).and_then(|text| text.as_deref()).unwrap_or_default();
            let view = match kind {
                ConnectionKind::Postgres => kind_of == "v",
                ConnectionKind::Clickhouse => kind_of.ends_with("View"),
                _ => kind_of.contains("VIEW"),
            };
            let comment = row.get(4).cloned().flatten();
            let stats = if view {
                // MySQL fills a view's comment with the word "VIEW".
                TableStats::new(None, None, comment.filter(|comment| comment != "VIEW"))
            } else {
                TableStats::new(count(row.get(2)?), count(row.get(3)?), comment)
            };
            Some((name, Arc::new(stats)))
        })
        .collect()
}

/// The estimates a fixture (a demo scenario) makes up for `names`: the same ones every run, from the names alone.
fn fixture_stats(names: &[String]) -> HashMap<String, Arc<TableStats>> {
    names
        .iter()
        .map(|name| {
            let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3));
            let rows = (hash >> 8) % 3_000_000;
            let comment = (hash % 3 == 0).then(|| "Sample data for the demo".to_owned());
            (name.clone(), Arc::new(TableStats::new(Some(rows), Some(rows * (40 + hash % 120)), comment)))
        })
        .collect()
}

/// Where the estimates of one namespace of one connection are kept.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct StatsKey {
    pub(super) connection: String,
    pub(super) database: String,
    pub(super) schema: String,
}

pub(super) enum StatsState {
    Loading,
    Ready(HashMap<String, Arc<TableStats>>),
    /// Read and failed: not asked again until a folder of the namespace reloads.
    Failed,
}

pub(super) struct StatsEntry {
    /// The load generations of the namespace's folders it was read for.
    stamp: u64,
    pub(super) state: StatsState,
}

/// The cache of every namespace read so far.
#[derive(Default)]
pub(super) struct StatsCache {
    entries: HashMap<StatsKey, StatsEntry>,
}

impl StatsCache {
    /// How many statements were started (tests: one per namespace, not one per row).
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl AppRoot {
    /// The namespace a table, view or materialized view row belongs to, when its connection has estimates to read.
    fn stats_key(&self, id: &NavigatorNodeId) -> Option<StatsKey> {
        let NavigatorNodeId::Object { connection, database, schema, .. } = id else { return None };
        let kind = self.saved_connections.iter().find(|saved| saved.matches_id(connection))?.kind;
        stats_sql(kind, database, schema)?;
        Some(StatsKey { connection: connection.clone(), database: database.clone(), schema: schema.clone() })
    }

    /// Changes whenever one of the folders that list tables of the namespace loads again.
    fn stats_stamp(&self, key: &StatsKey) -> u64 {
        [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::MaterializedViews]
            .into_iter()
            .filter_map(|group| self.navigator.node_load_generation(&navigator::object_group_id(&key.connection, &key.database, &key.schema, group)))
            .sum()
    }

    /// Whether the row `id` has estimates to show (a table, view or materialized view of PostgreSQL, MySQL, MariaDB or ClickHouse).
    pub(super) fn table_stats_supported(&self, id: &NavigatorNodeId) -> bool {
        self.stats_key(id).is_some() && object_actions::ObjectKind::of(self.navigator.object_kind(id)).is_some()
    }

    /// The estimates of the table `id`, once read for the folders as they are now.
    pub(super) fn table_stats_of(&self, id: &NavigatorNodeId) -> Option<Arc<TableStats>> {
        let NavigatorNodeId::Object { object, .. } = id else { return None };
        let key = self.stats_key(id)?;
        let entry = self.table_stats.entries.get(&key).filter(|entry| entry.stamp == self.stats_stamp(&key))?;
        match &entry.state {
            StatsState::Ready(found) => found.get(object).cloned(),
            _ => None,
        }
    }

    /// The estimates of the namespace of the table `id` are neither read nor being read for the folders as they are now.
    pub(super) fn table_stats_missing(&self, id: &NavigatorNodeId) -> bool {
        self.stats_key(id).is_some_and(|key| self.table_stats.entries.get(&key).is_none_or(|entry| entry.stamp != self.stats_stamp(&key)))
    }

    /// Reads the estimates of the namespace of the table `id`, unless they are read (or being read) for the folders as they are now.
    /// Called when a table row is drawn with the setting on and when one is hovered. Never connects.
    pub(super) fn ensure_table_stats(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        let Some(key) = self.stats_key(id) else { return };
        let stamp = self.stats_stamp(&key);
        if self.table_stats.entries.get(&key).is_some_and(|entry| entry.stamp == stamp) {
            return;
        }
        let Some(kind) = self.saved_connections.iter().find(|saved| saved.matches_id(&key.connection)).map(|saved| saved.kind) else { return };
        let Some(sql) = stats_sql(kind, &key.database, &key.schema) else { return };
        // A fixture stands in for the server.
        if self.scenario_ui.service.is_some() {
            let names: Vec<String> = [ObjectGroup::Tables, ObjectGroup::Views, ObjectGroup::MaterializedViews]
                .into_iter()
                .filter_map(|group| self.navigator.loaded_children(&navigator::object_group_id(&key.connection, &key.database, &key.schema, group)))
                .flatten()
                .filter_map(|child| match child {
                    NavigatorNodeId::Object { object, .. } => Some(object.clone()),
                    _ => None,
                })
                .collect();
            self.table_stats.entries.insert(key, StatsEntry { stamp, state: StatsState::Ready(fixture_stats(&names)) });
            return;
        }
        let database = (kind == ConnectionKind::Postgres).then(|| key.database.clone());
        let source = match self.table_stats_source(&key.connection, database) {
            Ok(source) => source,
            Err(_) => {
                self.table_stats.entries.insert(key, StatsEntry { stamp, state: StatsState::Failed });
                return;
            }
        };
        self.table_stats.entries.insert(key.clone(), StatsEntry { stamp, state: StatsState::Loading });
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<HashMap<String, Arc<TableStats>>, ReadError> {
                let mut session = source.open()?;
                session.rows(&sql).map(|rows| parse_stats(kind, &rows))
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(entry) = this.table_stats.entries.get_mut(&key).filter(|entry| entry.stamp == stamp && matches!(entry.state, StatsState::Loading)) {
                    entry.state = match result {
                        Ok(found) => StatsState::Ready(found),
                        Err(_) => StatsState::Failed,
                    };
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Where the estimates of `connection` are read from: a second session of it (opened from its profile, off the UI thread).
    /// An error: the connection is not open.
    fn table_stats_source(&self, connection: &str, database: Option<String>) -> Result<StatsSource, String> {
        #[cfg(test)]
        if let Some(make) = &self.table_stats_injected {
            return Ok(StatsSource::Injected(Arc::clone(make), database));
        }
        if !self.driver_runtime.sessions.contains_key(connection) {
            return Err("The connection is not open.".to_owned());
        }
        let saved = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).cloned().ok_or("This connection no longer exists.")?;
        let spec = self.secondary_session_spec(&saved, connection)?;
        Ok(StatsSource::Driver(Box::new(spec), database))
    }

}

/// How a read of estimates opens its session.
enum StatsSource {
    Driver(Box<super::structure_edit::apply::SessionSpec>, Option<String>),
    #[cfg(test)]
    Injected(Arc<accounts::InjectedCatalog>, Option<String>),
}

impl StatsSource {
    fn open(self) -> Result<Box<dyn CatalogSession>, ReadError> {
        match self {
            Self::Driver(spec, database) => Ok(Box::new(accounts::DriverCatalog::open_in(*spec, database)?)),
            #[cfg(test)]
            Self::Injected(make, database) => {
                let session: Box<dyn CatalogSession> = make(database.as_deref());
                Ok(session)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(parts: &[Option<&str>]) -> Vec<Option<String>> {
        parts.iter().map(|part| part.map(str::to_owned)).collect()
    }

    #[test]
    fn counts_and_sizes_are_short_and_say_their_unit() {
        let shown: Vec<String> = [0, 1, 950, 1_000, 12_345, 999_999, 1_200_000, 45_000_000, 3_400_000_000].into_iter().map(format_rows).collect();
        assert_eq!(shown, ["0 rows", "1 row", "950 rows", "1K rows", "12K rows", "1M rows", "1.2M rows", "45M rows", "3.4B rows"]);
        let shown: Vec<String> = [0, 512, 1024, 8192, 1_500_000, 47_185_920, 1_288_490_189, 5_497_558_138_880].into_iter().map(format_bytes).collect();
        assert_eq!(shown, ["0 B", "512 B", "1.0 KB", "8.0 KB", "1.4 MB", "45 MB", "1.2 GB", "5.0 TB"]);
        assert_eq!(TableStats::new(Some(1_200_000), Some(47_185_920), None).label.as_deref(), Some("1.2M rows \u{00B7} 45 MB"));
        assert_eq!(TableStats::new(None, Some(8192), None).label.as_deref(), Some("8.0 KB"));
        assert_eq!(TableStats::new(None, None, Some("a note".into())).label, None);
    }

    #[test]
    fn one_statement_reads_a_namespace_and_only_the_catalogs_that_have_estimates() {
        let postgres = stats_sql(ConnectionKind::Postgres, "app", "public").unwrap();
        assert!(postgres.contains("reltuples") && postgres.contains("pg_total_relation_size") && postgres.contains("obj_description"), "{postgres}");
        assert!(postgres.contains("n.nspname = 'public'") && !postgres.to_lowercase().contains("count("), "{postgres}");
        // MySQL's schema is its database; a quote in a name stays inside its literal.
        let mysql = stats_sql(ConnectionKind::Mysql, "shop", "shop").unwrap();
        assert!(mysql.contains("information_schema.TABLES") && mysql.contains("TABLE_ROWS") && mysql.contains("DATA_LENGTH + INDEX_LENGTH") && mysql.contains("TABLE_COMMENT"), "{mysql}");
        assert!(mysql.ends_with("TABLE_SCHEMA = 'shop'"), "{mysql}");
        assert!(stats_sql(ConnectionKind::Mariadb, "it's", "").unwrap().ends_with("TABLE_SCHEMA = 'it''s'"));
        let clickhouse = stats_sql(ConnectionKind::Clickhouse, "logs", "").unwrap();
        assert!(clickhouse.contains("system.tables") && clickhouse.contains("total_rows") && clickhouse.contains("total_bytes") && clickhouse.ends_with("database = 'logs'"), "{clickhouse}");
        for kind in [ConnectionKind::Sqlite, ConnectionKind::Mongodb, ConnectionKind::Redis] {
            assert!(stats_sql(kind, "main", "main").is_none(), "{kind:?}");
        }
    }

    #[test]
    fn unknown_estimates_and_views_show_nothing_and_a_comment_is_kept() {
        let rows = vec![
            text(&[Some("orders"), Some("r"), Some("1200000"), Some("47185920"), Some("  Customer orders ")]),
            text(&[Some("fresh"), Some("r"), Some("-1"), Some("8192"), None]),
            text(&[Some("recent"), Some("p"), Some("0"), Some("0"), Some("")]),
            text(&[Some("v_orders"), Some("v"), Some("0"), Some("0"), Some("Orders per day")]),
        ];
        let found = parse_stats(ConnectionKind::Postgres, &rows);
        assert_eq!((found["orders"].rows, found["orders"].bytes, found["orders"].comment.as_deref()), (Some(1_200_000), Some(47_185_920), Some("Customer orders")));
        // reltuples = -1: the rows are unknown, the size is not.
        assert_eq!((found["fresh"].rows, found["fresh"].bytes), (None, Some(8192)));
        assert_eq!(found["fresh"].label.as_deref(), Some("8.0 KB"));
        assert_eq!(found["recent"].comment, None);
        // A view has no estimates of its own, only its comment.
        assert_eq!((found["v_orders"].rows, found["v_orders"].bytes, found["v_orders"].label.clone()), (None, None, None));
        assert_eq!(found["v_orders"].tooltip_lines(), ["Orders per day"]);

        let rows = vec![
            text(&[Some("t"), Some("BASE TABLE"), Some("1500"), Some("1646592.0"), Some("a table")]),
            text(&[Some("v"), Some("VIEW"), None, None, Some("VIEW")]),
            text(&[Some("empty"), Some("BASE TABLE"), None, None, Some("")]),
        ];
        let found = parse_stats(ConnectionKind::Mysql, &rows);
        assert_eq!((found["t"].rows, found["t"].bytes), (Some(1500), Some(1_646_592)));
        assert!(found["v"].is_empty(), "MySQL calls a view's comment VIEW");
        assert!(found["empty"].is_empty());

        let rows = vec![text(&[Some("events"), Some("MergeTree"), Some("42"), Some("2048"), Some("clicks")]), text(&[Some("mv"), Some("MaterializedView"), None, None, Some("")])];
        let found = parse_stats(ConnectionKind::Clickhouse, &rows);
        assert_eq!(found["events"].label.as_deref(), Some("42 rows \u{00B7} 2.0 KB"));
        assert!(found["mv"].is_empty());
    }

    #[test]
    fn the_estimate_gives_up_its_size_first_and_then_itself() {
        let widths = [px(90.0), px(50.0)];
        assert_eq!(fitting(widths, px(120.0)), Some(0), "the whole label");
        assert_eq!(fitting(widths, px(80.0)), Some(1), "the rows alone");
        assert_eq!(fitting(widths, px(60.0)), None, "nothing: not a cut-off one");
        let stats = TableStats::new(Some(895_000), Some(35_651_584), None);
        assert_eq!((stats.label.as_deref(), stats.short_label.as_deref()), (Some("895K rows \u{00B7} 34 MB"), Some("895K rows")));
        // Without both parts there is nothing to drop.
        assert_eq!(TableStats::new(None, Some(8192), None).short_label, None);
        assert_eq!(TableStats::new(Some(5), None, None).short_label, None);
    }

    #[test]
    fn a_tooltip_lists_the_comment_then_the_estimates() {
        let stats = TableStats::new(Some(1_200_000), Some(47_185_920), Some("Customer orders".into()));
        assert_eq!(stats.tooltip_lines(), ["Customer orders", "About 1.2M rows (estimate)", "45 MB on disk"]);
        assert!(TableStats::new(None, None, None).tooltip_lines().is_empty());
    }
}
