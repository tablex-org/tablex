//! The Query History manager without a window: what its rows are, how the filters narrow them and
//! how a page of them is read.
//!
//! Two sources feed it. The SQL queries are the `query_history` table of the app database,
//! searched by `engine_storage::history` a page at a time (an index on the time, `LIKE` for the
//! words, so a page costs the same however long the history is). The MCP calls are the lines of the
//! audit log that `tablex-mcp` writes (`engine_agent::audit`): read, never written. "All" merges
//! the two by time, newest first, from one [`Pager`].

use std::collections::{BTreeSet, HashMap, VecDeque};

use chrono::{Duration, Local, NaiveDate, TimeZone};
use engine_storage::history::{
    self, Cursor, HistoryEntry, HistoryFilter, Scope, StatusFilter, KIND_DATA_CHANGE, KIND_QUERY, KIND_REDIS_COMMAND,
    KIND_SCHEMA_CHANGE,
};
use gpui::SharedString;

/// Rows a page holds, and what one "Load more" adds.
pub(in crate::app) const PAGE: u32 = 200;

/// The `front_end` of the audit lines `tablex-mcp` writes.
const MCP_FRONT_END: &str = "mcp";

/// The most audit lines read (the log keeps at most five files of 2 MiB).
const MCP_READ_MAX: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum Source {
    #[default]
    All,
    Sql,
    Mcp,
}

impl Source {
    pub(in crate::app) const ALL: [Self; 3] = [Self::All, Self::Sql, Self::Mcp];

    pub(in crate::app) fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Sql => "sql",
            Self::Mcp => "mcp",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum KindFilter {
    #[default]
    All,
    Query,
    DataChange,
    SchemaChange,
    RedisCommand,
    Failed,
    /// MCP calls that worked.
    Succeeded,
}

impl KindFilter {
    pub(in crate::app) fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Query => "query",
            Self::DataChange => "data",
            Self::SchemaChange => "schema",
            Self::RedisCommand => "redis",
            Self::Failed => "failed",
            Self::Succeeded => "succeeded",
        }
    }

    /// The kinds the filter bar offers for `source`.
    pub(in crate::app) fn offered(source: Source) -> &'static [Self] {
        match source {
            Source::Mcp => &[Self::All, Self::Failed, Self::Succeeded],
            Source::All | Source::Sql => &[
                Self::All,
                Self::Query,
                Self::DataChange,
                Self::SchemaChange,
                Self::RedisCommand,
                Self::Failed,
            ],
        }
    }

    fn storage_kind(self) -> Option<&'static str> {
        match self {
            Self::Query => Some(KIND_QUERY),
            Self::DataChange => Some(KIND_DATA_CHANGE),
            Self::SchemaChange => Some(KIND_SCHEMA_CHANGE),
            Self::RedisCommand => Some(KIND_REDIS_COMMAND),
            Self::All | Self::Failed | Self::Succeeded => None,
        }
    }
}

/// One call of an MCP tool, as the audit log has it.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct McpCall {
    /// Epoch seconds.
    pub(in crate::app) time: f64,
    pub(in crate::app) tool: String,
    pub(in crate::app) connection_id: Option<String>,
    pub(in crate::app) statement: Option<String>,
    /// `ok`, or the stable error code the call ended with.
    pub(in crate::app) outcome: String,
    pub(in crate::app) rows: Option<u64>,
    pub(in crate::app) duration_ms: u64,
    /// What the person asked about a write decided: `approved`, `declined: ...`, `cancelled`.
    pub(in crate::app) approval: Option<String>,
}

impl McpCall {
    pub(in crate::app) fn ok(&self) -> bool {
        self.outcome == "ok"
    }

    fn from_entry(entry: engine_agent::audit::AuditEntry) -> Self {
        let time = chrono::DateTime::parse_from_rfc3339(&entry.time)
            .map(|time| time.timestamp_millis() as f64 / 1000.0)
            .unwrap_or(0.0);
        Self {
            time,
            tool: entry.tool,
            connection_id: entry.connection_id,
            statement: entry.statement,
            outcome: entry.outcome,
            rows: entry.rows,
            duration_ms: entry.duration_ms,
            approval: entry.approval,
        }
    }
}

/// A row of the manager.
#[derive(Debug, Clone)]
pub(in crate::app) enum Row {
    Sql(HistoryEntry),
    Mcp(McpCall),
}

impl Row {
    pub(in crate::app) fn time(&self) -> f64 {
        match self {
            Self::Sql(entry) => entry.timestamp,
            Self::Mcp(call) => call.time,
        }
    }

    pub(in crate::app) fn failed(&self) -> bool {
        match self {
            Self::Sql(entry) => entry.failed(),
            Self::Mcp(call) => !call.ok(),
        }
    }

    /// The statement a run held, or the SQL an MCP call carried; empty when it carried none.
    pub(in crate::app) fn statement(&self) -> &str {
        match self {
            Self::Sql(entry) => &entry.filter,
            Self::Mcp(call) => call.statement.as_deref().unwrap_or_default(),
        }
    }

    pub(in crate::app) fn connection_id(&self) -> Option<&str> {
        match self {
            Self::Sql(entry) => Some(&entry.conn_uuid),
            Self::Mcp(call) => call.connection_id.as_deref(),
        }
    }

    /// What the run was: a query, a data change, a schema change or a Redis command (a row of an
    /// older release has no kind of its own: it is read from its statement).
    pub(in crate::app) fn kind(&self) -> &'static str {
        match self {
            Self::Sql(entry) => match entry.kind.as_deref() {
                Some(KIND_DATA_CHANGE) => KIND_DATA_CHANGE,
                Some(KIND_SCHEMA_CHANGE) => KIND_SCHEMA_CHANGE,
                Some(KIND_REDIS_COMMAND) => KIND_REDIS_COMMAND,
                Some(_) => KIND_QUERY,
                None => history::classify_sql(&entry.filter),
            },
            Self::Mcp(call) => call.statement.as_deref().map_or(KIND_QUERY, history::classify_sql),
        }
    }

    pub(in crate::app) fn is_mcp(&self) -> bool {
        matches!(self, Self::Mcp(_))
    }
}

/// Everything the filter bar says.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::app) struct Query {
    pub(in crate::app) source: Source,
    pub(in crate::app) text: String,
    pub(in crate::app) scopes: Vec<Scope>,
    pub(in crate::app) kind: KindFilter,
    /// Epoch seconds: from (inclusive) and until (exclusive).
    pub(in crate::app) since: Option<f64>,
    pub(in crate::app) until: Option<f64>,
    pub(in crate::app) tool: Option<String>,
}

impl Query {
    /// The filter of the SQL queries; `None` when this query shows none of them.
    pub(in crate::app) fn sql_filter(&self) -> Option<HistoryFilter> {
        if self.source == Source::Mcp || self.kind == KindFilter::Succeeded {
            return None;
        }
        Some(HistoryFilter {
            text: self.text.clone(),
            scopes: self.scopes.clone(),
            kind: self.kind.storage_kind().map(str::to_owned),
            since: self.since,
            until: self.until,
            status: if self.kind == KindFilter::Failed { StatusFilter::Failed } else { StatusFilter::Any },
        })
    }

    /// This query shows MCP calls: its source takes them, and no kind of SQL statement is asked for.
    pub(in crate::app) fn takes_mcp(&self) -> bool {
        self.source != Source::Sql && self.kind.storage_kind().is_none()
    }

    fn mcp_matches(&self, call: &McpCall) -> bool {
        if self.since.is_some_and(|since| call.time < since) || self.until.is_some_and(|until| call.time >= until) {
            return false;
        }
        match self.kind {
            KindFilter::Failed if call.ok() => return false,
            KindFilter::Succeeded if !call.ok() => return false,
            _ => {}
        }
        if !self.scopes.is_empty()
            && !call.connection_id.as_deref().is_some_and(|id| {
                self.scopes.iter().any(|scope| scope.conn_uuid.eq_ignore_ascii_case(id))
            })
        {
            return false;
        }
        if self.tool.as_deref().is_some_and(|tool| tool != call.tool) {
            return false;
        }
        let words: Vec<String> = self.text.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return true;
        }
        let haystack = format!(
            "{} {} {}",
            call.tool,
            call.statement.as_deref().unwrap_or_default(),
            call.outcome
        )
        .to_lowercase();
        words.iter().all(|word| haystack.contains(word))
    }
}

/// The MCP calls of the audit log at `path`, newest first, and the names of the tools seen.
/// A missing log is no calls; an unreadable one is an error to show.
pub(in crate::app) fn read_mcp(path: &std::path::Path) -> Result<(Vec<McpCall>, Vec<String>), String> {
    let read = engine_agent::audit::read_entries(path, MCP_READ_MAX).map_err(|error| error.to_string())?;
    let calls: Vec<McpCall> = read
        .entries
        .into_iter()
        .filter(|entry| entry.front_end == MCP_FRONT_END)
        .map(McpCall::from_entry)
        .collect();
    let tools: BTreeSet<&str> = calls.iter().map(|call| call.tool.as_str()).collect();
    let tools = tools.into_iter().take(100).map(str::to_owned).collect();
    Ok((calls, tools))
}

/// How a page of SQL queries is read: the storage in the app, a list in a test.
pub(in crate::app) type Fetch =
    Box<dyn FnMut(&HistoryFilter, Option<Cursor>, u32) -> history::Result<Vec<HistoryEntry>> + Send>;

/// The storage's search.
pub(in crate::app) fn storage_fetch() -> Fetch {
    Box::new(history::search)
}

/// The rows of one [`Query`], read page by page: the SQL queries a page at a time from the
/// database, the MCP calls from the list the audit log gave, merged newest first.
pub(in crate::app) struct Pager {
    sql_filter: Option<HistoryFilter>,
    fetch: Fetch,
    after: Option<Cursor>,
    sql: VecDeque<HistoryEntry>,
    sql_done: bool,
    mcp: VecDeque<McpCall>,
}

impl Pager {
    /// `mcp`: every call of the audit log, newest first (the query is applied here).
    pub(in crate::app) fn new(query: &Query, mcp: &[McpCall], fetch: Fetch) -> Self {
        let sql_filter = query.sql_filter();
        Self {
            sql_done: sql_filter.is_none(),
            sql_filter,
            fetch,
            after: None,
            sql: VecDeque::new(),
            mcp: if query.takes_mcp() {
                mcp.iter().filter(|call| query.mcp_matches(call)).cloned().collect()
            } else {
                VecDeque::new()
            },
        }
    }

    /// The next `count` rows, newest first.
    pub(in crate::app) fn next(&mut self, count: usize) -> history::Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(count);
        while rows.len() < count {
            if self.sql.is_empty() && !self.sql_done {
                if let Some(filter) = &self.sql_filter {
                    let page = (self.fetch)(filter, self.after, PAGE)?;
                    if (page.len() as u32) < PAGE {
                        self.sql_done = true;
                    }
                    if let Some(last) = page.last() {
                        self.after = Some(Cursor { timestamp: last.timestamp, id: last.id });
                    }
                    self.sql.extend(page);
                }
            }
            let take_sql = match (self.sql.front(), self.mcp.front()) {
                (Some(entry), Some(call)) => entry.timestamp >= call.time,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => break,
            };
            rows.push(if take_sql {
                Row::Sql(self.sql.pop_front().expect("checked"))
            } else {
                Row::Mcp(self.mcp.pop_front().expect("checked"))
            });
        }
        Ok(rows)
    }

    /// Nothing is left to read.
    pub(in crate::app) fn exhausted(&self) -> bool {
        self.sql.is_empty() && self.sql_done && self.mcp.is_empty()
    }
}

// ---- time ranges ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum Quick {
    #[default]
    AllTime,
    Today,
    Week,
    Month,
}

impl Quick {
    pub(in crate::app) const ALL: [Self; 4] = [Self::Today, Self::Week, Self::Month, Self::AllTime];

    pub(in crate::app) fn key(self) -> &'static str {
        match self {
            Self::AllTime => "all",
            Self::Today => "today",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    /// From when this range counts, at `now`.
    pub(in crate::app) fn since(self, now: chrono::DateTime<Local>) -> Option<f64> {
        let at = |at: chrono::DateTime<Local>| at.timestamp_millis() as f64 / 1000.0;
        match self {
            Self::AllTime => None,
            Self::Today => local_midnight(now.date_naive()).map(at),
            Self::Week => Some(at(now - Duration::days(7))),
            Self::Month => Some(at(now - Duration::days(30))),
        }
    }
}

/// What is wrong with the dates typed in the range popover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum RangeError {
    BadStart,
    BadEnd,
    Inverted,
}

/// `YYYY-MM-DD` (also with `/`).
pub(in crate::app) fn parse_date(text: &str) -> Option<NaiveDate> {
    let text = text.trim().replace('/', "-");
    NaiveDate::parse_from_str(&text, "%Y-%m-%d").ok()
}

fn local_midnight(date: NaiveDate) -> Option<chrono::DateTime<Local>> {
    Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest()
}

/// The span of two typed dates: from the start of the first day to the end of the second; a blank
/// one leaves that side open. Returns (since, until) in epoch seconds, `until` exclusive.
pub(in crate::app) fn range_bounds(start: &str, end: &str) -> Result<(Option<f64>, Option<f64>), RangeError> {
    let secs = |at: chrono::DateTime<Local>| at.timestamp_millis() as f64 / 1000.0;
    let first = match start.trim() {
        "" => None,
        text => Some(parse_date(text).ok_or(RangeError::BadStart)?),
    };
    let last = match end.trim() {
        "" => None,
        text => Some(parse_date(text).ok_or(RangeError::BadEnd)?),
    };
    if let (Some(first), Some(last)) = (first, last) {
        if first > last {
            return Err(RangeError::Inverted);
        }
    }
    let since = first.and_then(local_midnight).map(secs);
    let until = last.and_then(|day| day.succ_opt()).and_then(local_midnight).map(secs);
    Ok((since, until))
}

// ---- how a row reads -----------------------------------------------------------------------------------

/// `MM-DD HH:MM:SS` in the local time zone.
pub(in crate::app) fn format_time(epoch_secs: f64) -> String {
    Local
        .timestamp_millis_opt((epoch_secs * 1000.0) as i64)
        .single()
        .map_or_else(String::new, |time| time.format("%m-%d %H:%M:%S").to_string())
}

/// `YYYY-MM-DD HH:MM:SS` in the local time zone.
pub(in crate::app) fn format_full_time(epoch_secs: f64) -> String {
    Local
        .timestamp_millis_opt((epoch_secs * 1000.0) as i64)
        .single()
        .map_or_else(String::new, |time| time.format("%Y-%m-%d %H:%M:%S").to_string())
}

/// `850 ms`, `1.2 s`, `2 min 5 s`.
pub(in crate::app) fn format_duration(ms: u64) -> String {
    match ms {
        0..=999 => format!("{ms} ms"),
        1_000..=59_999 => format!("{:.1} s", ms as f64 / 1000.0),
        _ => format!("{} min {} s", ms / 60_000, ms % 60_000 / 1000),
    }
}

/// The first non-empty line, whitespace squeezed, at most `max` characters.
pub(in crate::app) fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    let squeezed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    match squeezed.char_indices().nth(max) {
        Some((end, _)) => format!("{}\u{2026}", &squeezed[..end]),
        None => squeezed,
    }
}

/// What a row of the list shows, made once when the row is loaded so that drawing it allocates
/// nothing.
#[derive(Debug, Clone)]
pub(in crate::app) struct RowView {
    pub(in crate::app) row: Row,
    pub(in crate::app) time: SharedString,
    pub(in crate::app) duration: SharedString,
    pub(in crate::app) result: SharedString,
    pub(in crate::app) place: SharedString,
    pub(in crate::app) line: SharedString,
    pub(in crate::app) failed: bool,
}

/// The connection of a row by name: ids in lowercase to saved names.
pub(in crate::app) type Names = HashMap<String, String>;

pub(in crate::app) fn connection_name(names: &Names, id: &str) -> String {
    names.get(&id.to_lowercase()).cloned().unwrap_or_else(|| t!("history.unknown_connection").into_owned())
}

impl RowView {
    pub(in crate::app) fn new(row: Row, names: &Names) -> Self {
        let failed = row.failed();
        let (duration, result, place, line) = match &row {
            Row::Sql(entry) => {
                let result = match (&entry.error, failed) {
                    (Some(error), true) => first_line(error, 120),
                    _ if entry.status.is_none() && entry.result_count == 0 => String::new(),
                    _ => t!("history.rows_n", count = entry.result_count).into_owned(),
                };
                let place = if entry.database.is_empty() {
                    connection_name(names, &entry.conn_uuid)
                } else {
                    format!("{} \u{b7} {}", connection_name(names, &entry.conn_uuid), entry.database)
                };
                (
                    entry.duration_ms.map(|ms| format_duration(ms.max(0) as u64)).unwrap_or_default(),
                    result,
                    place,
                    first_line(&entry.filter, 240),
                )
            }
            Row::Mcp(call) => {
                let result = if call.ok() {
                    call.rows.map(|rows| t!("history.rows_n", count = rows).into_owned()).unwrap_or_default()
                } else {
                    call.outcome.clone()
                };
                let place = call.connection_id.as_deref().map(|id| connection_name(names, id)).unwrap_or_default();
                let statement = call.statement.as_deref().map(|text| first_line(text, 200)).unwrap_or_default();
                (format_duration(call.duration_ms), result, place, format!("{}  {statement}", call.tool))
            }
        };
        Self {
            time: format_time(row.time()).into(),
            duration: duration.into(),
            result: result.into(),
            place: place.into(),
            line: line.into(),
            failed,
            row,
        }
    }
}

// ---- running a statement -------------------------------------------------------------------------------

/// A statement that is not a plain read: running it asks first.
pub(in crate::app) fn needs_confirmation(statement: &str, kind: &str) -> bool {
    if kind != KIND_QUERY {
        return true;
    }
    let lower = statement.to_lowercase();
    let word = |name: &str| lower.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| w == name);
    let first = lower.trim_start().split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or_default();
    let reads = matches!(first, "select" | "with" | "show" | "describe" | "desc" | "explain" | "values" | "table");
    // A script of the Mongo shell: a read when it only finds and counts.
    let shell_read = lower.trim_start().starts_with("db.")
        && ["find", "findone", "count", "countdocuments", "distinct", "estimateddocumentcount"].iter().any(|name| word(name))
        && !["insert", "update", "delete", "remove", "drop", "save", "replace", "bulkwrite", "createindex", "$out", "$merge"]
            .iter()
            .any(|name| lower.contains(name));
    if shell_read {
        return false;
    }
    // A CTE can hold a write.
    let cte_writes = first == "with" && ["insert", "update", "delete", "merge"].iter().any(|name| word(name));
    !reads || cte_writes
}

// ---- tests ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, time: f64, sql: &str) -> HistoryEntry {
        HistoryEntry {
            id,
            conn_uuid: "c1".into(),
            database: "app".into(),
            collection: String::new(),
            filter: sql.into(),
            project: String::new(),
            sort: String::new(),
            skip: 0,
            limit: 200,
            max_time_ms: 0,
            result_count: 3,
            timestamp: time,
            status: Some("ok".into()),
            error: None,
            duration_ms: Some(12),
            kind: Some(KIND_QUERY.into()),
        }
    }

    fn call(time: f64, tool: &str, connection: Option<&str>, outcome: &str) -> McpCall {
        McpCall {
            time,
            tool: tool.into(),
            connection_id: connection.map(str::to_owned),
            statement: Some("SELECT 1".into()),
            outcome: outcome.into(),
            rows: Some(1),
            duration_ms: 4,
            approval: None,
        }
    }

    /// A storage of `rows` (newest first) that answers the way the real one does.
    fn list(rows: Vec<HistoryEntry>) -> Fetch {
        Box::new(move |filter, after, limit| {
            Ok(rows
                .iter()
                .filter(|row| filter.status != StatusFilter::Failed || row.failed())
                .filter(|row| after.is_none_or(|a| row.timestamp < a.timestamp || (row.timestamp == a.timestamp && row.id < a.id)))
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }

    #[test]
    fn all_merges_the_sql_queries_and_the_mcp_calls_newest_first_across_pages() {
        let sql: Vec<_> = (0..450).rev().map(|i| entry(i, 1000.0 + i as f64 * 2.0, "SELECT 1")).collect();
        let mcp: Vec<_> = (0..100).rev().map(|i| call(1001.0 + i as f64 * 9.0, "execute_query", Some("c1"), "ok")).collect();
        let mut pager = Pager::new(&Query::default(), &mcp, list(sql));
        let mut seen = Vec::new();
        while !pager.exhausted() {
            let page = pager.next(100).unwrap();
            assert!(!page.is_empty());
            seen.extend(page.iter().map(Row::time));
        }
        assert_eq!(seen.len(), 550);
        assert!(seen.windows(2).all(|pair| pair[0] >= pair[1]), "newest first, across the page edges");
    }

    #[test]
    fn a_source_and_a_kind_decide_which_stores_are_read() {
        let mcp = vec![call(10.0, "execute_query", Some("c1"), "ok"), call(9.0, "execute_query", Some("c1"), "agent.write_refused")];
        let rows = |query: Query| {
            let mut pager = Pager::new(&query, &mcp, list(vec![entry(1, 5.0, "SELECT 1")]));
            pager.next(10).unwrap()
        };
        assert_eq!(rows(Query::default()).len(), 3);
        assert_eq!(rows(Query { source: Source::Sql, ..Default::default() }).len(), 1);
        assert_eq!(rows(Query { source: Source::Mcp, ..Default::default() }).len(), 2);
        assert_eq!(rows(Query { source: Source::Mcp, kind: KindFilter::Failed, ..Default::default() }).len(), 1);
        assert_eq!(rows(Query { source: Source::Mcp, kind: KindFilter::Succeeded, ..Default::default() }).len(), 1);
        // A kind of SQL statement is not asked of an MCP call.
        assert_eq!(rows(Query { kind: KindFilter::DataChange, ..Default::default() }).len(), 1);
        // Failed is the one kind that both sources have.
        assert_eq!(rows(Query { kind: KindFilter::Failed, ..Default::default() }).len(), 1);
    }

    #[test]
    fn mcp_calls_are_narrowed_by_words_connection_tool_and_time() {
        let calls = [
            call(100.0, "execute_query", Some("C1"), "ok"),
            call(200.0, "list_tables", Some("c2"), "ok"),
        ];
        let by = |query: Query| calls.iter().filter(|c| query.mcp_matches(c)).count();
        assert_eq!(by(Query { text: "EXECUTE select".into(), ..Default::default() }), 1);
        assert_eq!(by(Query { tool: Some("list_tables".into()), ..Default::default() }), 1);
        let scope = |id: &str| Scope { conn_uuid: id.into(), databases: vec![] };
        assert_eq!(by(Query { scopes: vec![scope("c1")], ..Default::default() }), 1, "ids are compared without case");
        assert_eq!(by(Query { since: Some(150.0), ..Default::default() }), 1);
        assert_eq!(by(Query { until: Some(200.0), ..Default::default() }), 1, "until is exclusive");
    }

    #[test]
    fn typed_dates_are_checked_and_make_a_day_range() {
        assert_eq!(range_bounds("", "").unwrap(), (None, None));
        let (since, until) = range_bounds("2026-03-01", "2026-03-01").unwrap();
        let (since, until) = (since.unwrap(), until.unwrap());
        assert!((until - since - 86_400.0).abs() <= 3_600.0, "one day (a time change may move an hour)");
        assert_eq!(range_bounds("2026/03/01", "").unwrap().1, None);
        assert_eq!(range_bounds("2026-13-01", "").unwrap_err(), RangeError::BadStart);
        assert_eq!(range_bounds("", "yesterday").unwrap_err(), RangeError::BadEnd);
        assert_eq!(range_bounds("2026-03-02", "2026-03-01").unwrap_err(), RangeError::Inverted);
    }

    #[test]
    fn quick_ranges_start_where_they_say() {
        let now = Local.with_ymd_and_hms(2026, 6, 15, 14, 30, 0).unwrap();
        let secs = |at: chrono::DateTime<Local>| at.timestamp_millis() as f64 / 1000.0;
        assert_eq!(Quick::AllTime.since(now), None);
        assert_eq!(Quick::Today.since(now), Some(secs(Local.with_ymd_and_hms(2026, 6, 15, 0, 0, 0).unwrap())));
        assert_eq!(Quick::Week.since(now), Some(secs(now - Duration::days(7))));
        assert_eq!(Quick::Month.since(now), Some(secs(now - Duration::days(30))));
    }

    #[test]
    fn a_row_reads_as_one_short_line_with_its_outcome() {
        let names: Names = [("c1".to_string(), "Shop".to_string())].into();
        let ok = RowView::new(Row::Sql(entry(1, 100.0, "\n  SELECT   id\nFROM t")), &names);
        assert_eq!(ok.line, "SELECT id");
        assert_eq!(ok.place, "Shop \u{b7} app");
        assert_eq!(ok.duration, "12 ms");
        assert!(!ok.failed);
        let mut failed = entry(2, 200.0, "SELECT * FROM nope");
        failed.status = Some("failed".into());
        failed.error = Some("no such table: nope\nsecond line".into());
        let view = RowView::new(Row::Sql(failed), &names);
        assert!(view.failed);
        assert_eq!(view.result, "no such table: nope");
        // An old row: no status, no duration; and a connection that is gone.
        let mut old = entry(3, 50.0, "DELETE FROM t");
        (old.status, old.duration_ms, old.kind, old.conn_uuid) = (None, None, None, "gone".into());
        let view = RowView::new(Row::Sql(old.clone()), &names);
        assert_eq!(view.duration, "");
        assert_eq!(Row::Sql(old).kind(), KIND_DATA_CHANGE, "read from the statement");
        assert_eq!(first_line(&"x".repeat(300), 10), format!("{}\u{2026}", "x".repeat(10)));
        assert_eq!(format_duration(1500), "1.5 s");
        assert_eq!(format_duration(125_000), "2 min 5 s");
    }

    #[test]
    fn running_asks_first_unless_the_statement_is_a_plain_read() {
        for read in ["SELECT 1", "  with t as (select 1) select * from t", "SHOW TABLES", "EXPLAIN SELECT 1", "db.users.find({})", "db.users.countDocuments({a: 1})"] {
            assert!(!needs_confirmation(read, history::classify_sql(read)), "{read}");
        }
        for write in [
            "UPDATE t SET a = 1",
            "DROP TABLE t",
            "CALL cleanup()",
            "VACUUM",
            "WITH x AS (DELETE FROM t RETURNING *) SELECT * FROM x",
            "db.users.updateMany({}, {})",
            "db.users.find({}); db.users.drop()",
        ] {
            assert!(needs_confirmation(write, history::classify_sql(write)), "{write}");
        }
        assert!(needs_confirmation("GET key", KIND_REDIS_COMMAND));
    }
}
