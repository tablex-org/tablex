//! What the wizard of a transfer holds about each table, apart from its widgets: the columns of the source, the table
//! the rows go to (an existing one, mapped by name, or one to create), and what the mapping finds wrong with it.

use std::collections::HashSet;

use gpui::SharedString;

use super::super::data_import::mapping::{self, NewColumn, TargetColumn};
use super::job::MappedColumn;
use super::reader::Query;
use super::typemap;
use crate::catalog::CatalogColumn;
use crate::connections::ConnectionKind;

/// Something that is read off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Loaded<T> {
    Pending,
    Ready(T),
    Failed(String),
}

impl<T> Loaded<T> {
    pub(super) fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// A column of the source: of a described table, or of the first page of a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourceColumn {
    pub(super) name: String,
    pub(super) data_type: String,
    pub(super) nullable: bool,
    pub(super) primary_key: bool,
}

impl From<&CatalogColumn> for SourceColumn {
    fn from(column: &CatalogColumn) -> Self {
        Self { name: column.name.clone(), data_type: column.data_type.clone(), nullable: column.nullable, primary_key: column.primary_key }
    }
}

/// The table the rows go to, as described.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TargetDesc {
    pub(super) columns: Vec<TargetColumn>,
    /// MySQL and MariaDB: the storage engine, which decides whether a transaction can be rolled back.
    pub(super) engine: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    /// Into the table of that name, which exists.
    Append,
    /// Into a table created first (through the review of its `CREATE TABLE`).
    Create,
    /// Not transferred.
    Leave,
}

impl Action {
    pub(super) const ALL: [Self; 3] = [Self::Append, Self::Create, Self::Leave];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Append => "Append to the table",
            Self::Create => "Create the table",
            Self::Leave => "Leave out",
        }
    }
}

pub(super) struct Job {
    /// The table (or `Query`) as the lists name it.
    pub(super) label: SharedString,
    /// The table's name; empty for a query.
    pub(super) source_table: String,
    pub(super) query: Query,
    pub(super) columns: Loaded<Vec<SourceColumn>>,
    pub(super) target_name: String,
    pub(super) action: Action,
    pub(super) target: Loaded<TargetDesc>,
    /// The target column of each source column (`Append`).
    pub(super) assignments: Vec<Option<usize>>,
    /// The columns of the table to create (`Create`).
    pub(super) new_columns: Vec<NewColumn>,
    /// The primary key of the table to create.
    pub(super) primary_key: Vec<String>,
    /// What the user should know about the type of each new column (a number rounded, a value refused).
    pub(super) type_notes: Vec<Option<&'static str>>,
    /// The primary key of the source in key order (a ClickHouse sorting key included, which is not unique).
    pub(super) source_key: Vec<String>,
    /// The table of that name exists in the target namespace.
    pub(super) exists: Option<bool>,
    /// Counts the lookups of the target table: an answer to an older one is dropped.
    pub(super) epoch: u64,
}

impl Job {
    pub(super) fn new(label: impl Into<SharedString>, source_table: String, query: Query) -> Self {
        let target_name = source_table.clone();
        Self {
            label: label.into(),
            source_table,
            query,
            columns: Loaded::Pending,
            target_name,
            action: Action::Create,
            target: Loaded::Pending,
            assignments: Vec::new(),
            new_columns: Vec::new(),
            primary_key: Vec::new(),
            type_notes: Vec::new(),
            source_key: Vec::new(),
            exists: None,
            epoch: 0,
        }
    }

    /// Whether the table is transferred at all.
    pub(super) fn active(&self) -> bool {
        self.action != Action::Leave
    }

    /// How many source columns are loaded.
    pub(super) fn mapped_count(&self) -> usize {
        match self.action {
            Action::Append => self.assignments.iter().flatten().count(),
            Action::Create => self.new_columns.iter().filter(|column| column.include).count(),
            Action::Leave => 0,
        }
    }

    pub(super) fn column_count(&self) -> usize {
        self.columns.ready().map_or(0, Vec::len)
    }

    /// The target columns of the load, by name, once the table exists.
    pub(super) fn mapped_columns(&self) -> Vec<MappedColumn> {
        let (Some(source), Some(target)) = (self.columns.ready(), self.target.ready()) else { return Vec::new() };
        self.assignments
            .iter()
            .enumerate()
            .filter_map(|(index, assignment)| {
                let target = target.columns.get((*assignment)?)?;
                Some(MappedColumn { source: source.get(index)?.name.clone(), target: target.name.clone(), data_type: target.data_type.clone() })
            })
            .collect()
    }

    /// Columns are read, and the table to append to is described.
    pub(super) fn ready(&self) -> bool {
        match self.action {
            Action::Leave => true,
            Action::Create => self.columns.ready().is_some(),
            Action::Append => self.columns.ready().is_some() && self.target.ready().is_some(),
        }
    }

    /// The table exists and is described: the columns of the source go to its columns of the same name.
    pub(super) fn append_to(&mut self, target: TargetDesc) {
        let sources: Vec<String> = self.columns.ready().map(|columns| columns.iter().map(|column| column.name.clone()).collect()).unwrap_or_default();
        self.assignments = mapping::auto_map(&sources, &target.columns);
        self.target = Loaded::Ready(target);
        self.action = Action::Append;
    }

    /// The table does not exist: it gets the columns of the source, typed in the words of the target dialect.
    pub(super) fn create_as(&mut self, from: ConnectionKind, to: ConnectionKind) {
        let columns = self.columns.ready().cloned().unwrap_or_default();
        let labels = if self.source_table.is_empty() { typemap::Labels::Result } else { typemap::Labels::Catalog };
        let (new_columns, mut primary_key, notes) = propose_columns(&columns, from, to, labels);
        // The primary key of a ClickHouse table is a sorting key: rows share it, and a table of another dialect would refuse them.
        if from == ConnectionKind::Clickhouse {
            primary_key.clear();
        } else if self.source_key.len() == primary_key.len() && self.source_key.iter().all(|name| primary_key.contains(name)) {
            // The key in the order the catalog gives it, not the order of the columns of the table.
            primary_key = self.source_key.clone();
        }
        self.new_columns = new_columns;
        self.primary_key = primary_key;
        self.type_notes = notes;
        self.action = Action::Create;
    }

    /// What the mapping finds wrong, apart from what the server will say about a row.
    pub(super) fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        match self.action {
            Action::Append => {
                let (Some(target), Some(columns)) = (self.target.ready(), self.columns.ready()) else { return notes };
                if !self.assignments.iter().any(Option::is_some) {
                    notes.push("No column of the source goes to a column of the table".to_owned());
                }
                for (index, target) in target.columns.iter().enumerate() {
                    if !self.assignments.contains(&Some(index)) && !target.nullable && !target.filled_by_server {
                        notes.push(format!("\"{}\" does not accept NULL and has no default, and no column goes to it: every row will be refused", target.name));
                    }
                }
                for (source, assignment) in self.assignments.iter().enumerate() {
                    let Some(target) = assignment.and_then(|index| target.columns.get(index)) else { continue };
                    if target.generated {
                        notes.push(format!("\"{}\" is a generated column: the server computes it and refuses a value", target.name));
                    }
                    if let (Some(column), false) = (columns.get(source), target.nullable) {
                        if column.nullable && !target.filled_by_server {
                            notes.push(format!("\"{}\" can hold NULL but \"{}\" does not accept it: those rows are refused", column.name, target.name));
                        }
                    }
                }
            }
            Action::Create | Action::Leave => {}
        }
        notes
    }
}

/// The columns of a table created for `columns` of a `from` server on a `to` server, and its primary key. A key column
/// never gets a type that has to be given a length to be indexed (MySQL).
pub(super) fn propose_columns(columns: &[SourceColumn], from: ConnectionKind, to: ConnectionKind, labels: typemap::Labels) -> (Vec<NewColumn>, Vec<String>, Vec<Option<&'static str>>) {
    let mysql = matches!(to, ConnectionKind::Mysql | ConnectionKind::Mariadb);
    let mapped: Vec<typemap::Mapped> = columns.iter().map(|column| typemap::map_type_noted(from, &column.data_type, to, labels)).collect();
    let notes = mapped.iter().map(|mapped| mapped.note).collect();
    let new_columns: Vec<NewColumn> = columns
        .iter()
        .zip(mapped)
        .map(|(column, mapped)| {
            let mut data_type = mapped.data_type;
            if mysql && column.primary_key && matches!(data_type.as_str(), "text" | "longtext" | "longblob") {
                data_type = if data_type == "longblob" { "varbinary(255)" } else { "varchar(255)" }.to_owned();
            }
            NewColumn { include: true, name: column.name.clone(), data_type, nullable: column.nullable }
        })
        .collect();
    let primary_key = columns.iter().filter(|column| column.primary_key).map(|column| column.name.clone()).collect();
    (new_columns, primary_key, notes)
}

/// The names `taken` lacks of those the jobs write to: two jobs may not write to one table.
pub(super) fn duplicate_targets(jobs: &[Job]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut duplicates = Vec::new();
    for job in jobs.iter().filter(|job| job.active()) {
        let name = job.target_name.to_lowercase();
        if !seen.insert(name) && !duplicates.contains(&job.target_name) {
            duplicates.push(job.target_name.clone());
        }
    }
    duplicates
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionKind::{Clickhouse, Mysql, Postgres, Sqlite};

    fn column(name: &str, data_type: &str, nullable: bool, primary_key: bool) -> SourceColumn {
        SourceColumn { name: name.into(), data_type: data_type.into(), nullable, primary_key }
    }

    fn orders() -> Vec<SourceColumn> {
        vec![column("id", "bigint", false, true), column("code", "text", false, true), column("note", "text", true, false), column("total", "numeric(12,2)", true, false)]
    }

    #[test]
    fn a_new_table_keeps_names_nullability_and_the_primary_key_in_the_types_of_the_target() {
        let (columns, key, _) = propose_columns(&orders(), Postgres, Mysql, typemap::Labels::Catalog);
        assert_eq!(key, ["id", "code"]);
        let shown: Vec<(&str, &str, bool)> = columns.iter().map(|column| (column.name.as_str(), column.data_type.as_str(), column.nullable)).collect();
        // `code` is a key: MySQL cannot index a longtext without a length.
        assert_eq!(shown, [("id", "bigint", false), ("code", "varchar(255)", false), ("note", "longtext", true), ("total", "decimal(12,2)", true)]);
        let (columns, _, _) = propose_columns(&orders(), Postgres, Clickhouse, typemap::Labels::Catalog);
        assert_eq!(columns[3].data_type, "Decimal(12, 2)");
        let (columns, _, _) = propose_columns(&orders(), Postgres, Sqlite, typemap::Labels::Catalog);
        assert_eq!(columns.iter().map(|column| column.data_type.as_str()).collect::<Vec<_>>(), ["INTEGER", "TEXT", "TEXT", "NUMERIC"]);
    }

    fn job_with(columns: Vec<SourceColumn>) -> Job {
        let mut job = Job::new("public.orders", "orders".into(), Query::new(None, "SELECT * FROM orders".into()));
        job.columns = Loaded::Ready(columns);
        job
    }

    fn target(columns: &[(&str, &str, bool, bool)]) -> TargetDesc {
        TargetDesc {
            columns: columns
                .iter()
                .map(|(name, data_type, nullable, filled)| TargetColumn { name: (*name).into(), data_type: (*data_type).into(), nullable: *nullable, filled_by_server: *filled, generated: false })
                .collect(),
            engine: None,
        }
    }

    #[test]
    fn an_existing_table_is_mapped_by_name_and_the_mapping_is_what_the_load_gets() {
        let mut job = job_with(orders());
        job.append_to(target(&[("TOTAL", "numeric(14,2)", true, false), ("id", "int8", false, false), ("extra", "text", true, false)]));
        assert_eq!(job.assignments, [Some(1), None, None, Some(0)]);
        assert_eq!((job.mapped_count(), job.column_count()), (2, 4));
        assert_eq!(
            job.mapped_columns(),
            [
                MappedColumn { source: "id".into(), target: "id".into(), data_type: "int8".into() },
                MappedColumn { source: "total".into(), target: "TOTAL".into(), data_type: "numeric(14,2)".into() },
            ]
        );
        assert!(job.notes().is_empty(), "{:?}", job.notes());
    }

    #[test]
    fn the_mapping_flags_a_column_the_target_would_refuse() {
        let mut job = job_with(orders());
        job.append_to(target(&[("id", "int8", false, false), ("note", "text", false, false), ("required", "text", false, false), ("serial_id", "int", false, true)]));
        let notes = job.notes();
        assert!(notes.iter().any(|note| note.contains("\"required\" does not accept NULL")), "{notes:?}");
        assert!(notes.iter().any(|note| note.contains("\"note\" can hold NULL but \"note\" does not accept it")), "{notes:?}");
        assert!(!notes.iter().any(|note| note.contains("serial_id")), "the server fills it: {notes:?}");
        let mut none = job_with(orders());
        none.append_to(target(&[("other", "text", true, false)]));
        assert!(none.notes().iter().any(|note| note.contains("No column of the source goes")));
    }

    #[test]
    fn a_clickhouse_sorting_key_is_not_made_a_primary_key_of_the_new_table() {
        let mut job = job_with(vec![column("id", "Int64", false, true), column("day", "Date", false, true), column("v", "String", true, false)]);
        job.create_as(Clickhouse, Postgres);
        assert!(job.primary_key.is_empty(), "rows share a ClickHouse key: {:?}", job.primary_key);
        let mut kept = job_with(vec![column("id", "bigint", false, true)]);
        kept.create_as(Postgres, Clickhouse);
        assert_eq!(kept.primary_key, ["id"]);
    }

    #[test]
    fn the_type_of_a_new_column_comes_with_what_it_cannot_hold() {
        let mut job = job_with(vec![column("n", "numeric", true, false), column("i", "integer", true, false)]);
        job.create_as(Postgres, Mysql);
        assert_eq!(job.new_columns[0].data_type, "decimal(65,30)");
        assert!(job.type_notes[0].is_some_and(|note| note.contains("35 digits before the point and 30 after")) && job.type_notes[1].is_none());
    }

    #[test]
    fn a_job_is_ready_when_what_it_needs_is_known() {
        let mut job = Job::new("t", "t".into(), Query::new(None, "SELECT 1".into()));
        assert!(!job.ready());
        job.columns = Loaded::Ready(orders());
        job.create_as(Postgres, Postgres);
        assert!(job.ready(), "a table to create needs only the columns of the source");
        assert_eq!(job.mapped_count(), 4);
        job.action = Action::Append;
        assert!(!job.ready(), "a table to append to has to be described");
        job.action = Action::Leave;
        assert!(job.ready() && !job.active());
    }

    #[test]
    fn two_tables_may_not_be_written_to_one_name() {
        let mut a = job_with(orders());
        let mut b = job_with(orders());
        b.target_name = "ORDERS".into();
        assert_eq!(duplicate_targets(&[a, b]), ["ORDERS"]);
        a = job_with(orders());
        let mut left = job_with(orders());
        left.action = Action::Leave;
        assert!(duplicate_targets(&[a, left]).is_empty(), "a table that is left out writes nowhere");
    }
}
