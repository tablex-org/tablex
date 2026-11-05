//! Which source column goes to which target column, what a new table looks like, and what the sample says
//! about the mapping before anything is loaded.

use super::convert::Converter;
use super::run::{ColumnPlan, ExtraColumn};
use crate::catalog::CatalogColumn;
use crate::connections::ConnectionKind;
use crate::import::infer::{Hints, Inferred};
use crate::import::source::Sample;
use crate::sql_ddl::{checked_name, plan_with_lock_timeout, Change, ColumnSpec, DdlDialect, TableId, DEFAULT_LOCK_TIMEOUT_SECS};

/// A column of the table the rows go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct TargetColumn {
    pub(in crate::app) name: String,
    pub(in crate::app) data_type: String,
    pub(in crate::app) nullable: bool,
    /// A value is made when none is given: a default, an auto-increment or a generated column.
    pub(in crate::app) filled_by_server: bool,
    /// The server computes it: nothing can be written.
    pub(in crate::app) generated: bool,
}

impl From<&CatalogColumn> for TargetColumn {
    fn from(column: &CatalogColumn) -> Self {
        let generated = column.is_generated == Some(true);
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            nullable: column.nullable,
            filled_by_server: generated || column.auto_increment == Some(true) || column.default_expression.is_some(),
            generated,
        }
    }
}

/// For each source column, the target column it goes to by name (case does not matter), each target at most
/// once; the source columns without a namesake are skipped, and so are generated target columns.
pub(in crate::app) fn auto_map(sources: &[String], targets: &[TargetColumn]) -> Vec<Option<usize>> {
    let mut taken = vec![false; targets.len()];
    sources
        .iter()
        .map(|source| {
            let found = targets
                .iter()
                .enumerate()
                .find(|(index, target)| !taken[*index] && !target.generated && target.name.eq_ignore_ascii_case(source.trim()))?;
            taken[found.0] = true;
            Some(found.0)
        })
        .collect()
}

/// What the sample finds wrong with the mapping: the problems of the whole load (`source` is `None`) and of one
/// source column. None of it stops anything: the load applies its error policy to the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct Note {
    pub(in crate::app) source: Option<usize>,
    pub(in crate::app) message: String,
}

/// Checks the first rows of the sample against the columns they are mapped to.
pub(in crate::app) fn check_sample(sample: &Sample, assignments: &[Option<usize>], targets: &[TargetColumn], hints: &Hints) -> Vec<Note> {
    let mut notes = Vec::new();
    for (source, assignment) in assignments.iter().enumerate() {
        let Some(target) = assignment.and_then(|index| targets.get(index)) else { continue };
        let name = |source: usize| sample.columns.get(source).map_or("", String::as_str);
        if target.generated {
            notes.push(Note { source: Some(source), message: format!("\"{}\" is a generated column: the server computes it and refuses a value", target.name) });
            continue;
        }
        let converter = Converter::new(&target.data_type);
        let (mut failing, mut first, mut empty) = (0usize, None, 0usize);
        for row in &sample.rows {
            match row.cells.get(source).and_then(|cell| cell.as_deref()) {
                None => empty += 1,
                Some(text) if text.trim().is_empty() && converter.empty_is_null() => empty += 1,
                Some(text) => {
                    if let Err(problem) = converter.convert(text, hints) {
                        failing += 1;
                        first.get_or_insert_with(|| (row.number, text.chars().take(30).collect::<String>(), problem));
                    }
                }
            }
        }
        if let Some((row, text, problem)) = first {
            notes.push(Note {
                source: Some(source),
                message: format!(
                    "{failing} of {} values {problem} for {} \"{}\" (row {row}: \"{text}\")",
                    sample.rows.len(),
                    target.data_type,
                    name(source)
                ),
            });
        }
        if empty > 0 && !target.nullable && !target.filled_by_server {
            notes.push(Note {
                source: Some(source),
                message: format!("{empty} of {} values of \"{}\" are empty, and \"{}\" does not accept NULL", sample.rows.len(), name(source), target.name),
            });
        }
    }
    for (index, target) in targets.iter().enumerate() {
        let mapped = assignments.contains(&Some(index));
        if !mapped && !target.nullable && !target.filled_by_server {
            notes.push(Note {
                source: None,
                message: format!("\"{}\" does not accept NULL and has no default, and no column of the file goes to it: every row will be refused", target.name),
            });
        }
    }
    if !sample.rows.is_empty() && assignments.iter().all(Option::is_none) {
        notes.push(Note { source: None, message: "No column of the file is mapped to a column of the table".to_owned() });
    }
    notes
}

/// Engines that roll a transaction back (the driver's own list: it treats any other as having none).
pub(in crate::app) fn is_transactional_engine(engine: &str) -> bool {
    ["innodb", "xtradb", "tokudb", "rocksdb", "ndbcluster", "ndb"].iter().any(|known| engine.eq_ignore_ascii_case(known))
}

/// The storage engine a `SHOW CREATE TABLE` text names. Only the table options after the closing parenthesis of the
/// column list are read, and quoted text in them (a `COMMENT 'engine=InnoDB'`) is skipped, so nothing but the
/// real `ENGINE=` clause can answer.
pub(in crate::app) fn engine_of_ddl(ddl: &str) -> Option<String> {
    let options = table_options(ddl)?;
    let mut rest = options;
    while let Some(character) = rest.chars().next() {
        match character {
            '\'' | '"' | '`' => rest = after_quoted(rest, character),
            c if c.is_ascii_alphabetic() => {
                let word_end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(rest.len());
                let (word, tail) = rest.split_at(word_end);
                if word.eq_ignore_ascii_case("engine") {
                    if let Some(value) = tail.trim_start().strip_prefix('=') {
                        let name: String = value.trim_start().chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                        return Some(name).filter(|name| !name.is_empty());
                    }
                }
                rest = tail;
            }
            c => rest = &rest[c.len_utf8()..],
        }
    }
    None
}

/// What follows the parenthesis that closes the column list of a `CREATE TABLE`. Quoted names and strings are skipped
/// from the start, so a parenthesis in the table's own name (`` `a(b` ``) does not open the list.
fn table_options(ddl: &str) -> Option<&str> {
    let mut rest = ddl;
    let mut depth = 0usize;
    while let Some(character) = rest.chars().next() {
        match character {
            '\'' | '"' | '`' => rest = after_quoted(rest, character),
            '(' => {
                depth += 1;
                rest = &rest[1..];
            }
            ')' if depth > 0 => {
                depth -= 1;
                rest = &rest[1..];
                if depth == 0 {
                    return Some(rest);
                }
            }
            c => rest = &rest[c.len_utf8()..],
        }
    }
    None
}

/// `text` (which starts with the quote character `quote`) after the quoted part: a doubled quote and, in strings, a
/// backslash escape are inside it.
fn after_quoted(text: &str, quote: char) -> &str {
    let mut chars = text.char_indices().skip(1);
    while let Some((at, character)) = chars.next() {
        if character == '\\' && quote != '`' {
            chars.next();
        } else if character == quote {
            if text[at + 1..].starts_with(quote) {
                chars.next();
            } else {
                return &text[at + 1..];
            }
        }
    }
    ""
}

/// The assignments (a target index per file column) after the table was described again: each column keeps the target
/// it had, found by name.
pub(in crate::app) fn remap_assignments(previous: &[Option<String>], targets: &[TargetColumn]) -> Vec<Option<usize>> {
    previous.iter().map(|name| name.as_ref().and_then(|name| targets.iter().position(|target| target.name == *name))).collect()
}

/// The columns a JSON key that is not in the preview goes to: the columns of the table nothing of the file is mapped to,
/// by their own names (case does not matter).
pub(in crate::app) fn extras_for(assignments: &[Option<usize>], targets: &[TargetColumn]) -> Vec<ExtraColumn> {
    targets
        .iter()
        .enumerate()
        .filter(|(index, target)| !target.generated && !assignments.contains(&Some(*index)))
        .map(|(_, target)| ExtraColumn { key: target.name.clone(), target: target.name.clone(), converter: Converter::new(&target.data_type) })
        .collect()
}

/// The column plans of the load for `assignments` (existing table).
pub(in crate::app) fn plans_for(assignments: &[Option<usize>], targets: &[TargetColumn]) -> Vec<ColumnPlan> {
    assignments
        .iter()
        .enumerate()
        .filter_map(|(source, assignment)| {
            let target = targets.get((*assignment)?)?;
            Some(ColumnPlan { source, target: target.name.clone(), converter: Converter::new(&target.data_type) })
        })
        .collect()
}

/// A column of the table a load creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct NewColumn {
    pub(in crate::app) include: bool,
    pub(in crate::app) name: String,
    pub(in crate::app) data_type: String,
    /// The column accepts NULL (a column of a file always does; a transfer keeps the source column's own).
    pub(in crate::app) nullable: bool,
}

impl NewColumn {
    pub(in crate::app) fn target(&self) -> TargetColumn {
        TargetColumn { name: self.name.clone(), data_type: self.data_type.clone(), nullable: self.nullable, filled_by_server: false, generated: false }
    }
}

/// The type a new table gives a column of the file, in the words of `kind`'s dialect.
pub(in crate::app) fn proposed_type(kind: ConnectionKind, inferred: Inferred) -> &'static str {
    use ConnectionKind::{Clickhouse, Mariadb, Mysql, Postgres, Sqlite};
    match (kind, inferred) {
        (Sqlite, Inferred::Integer) => "INTEGER",
        (Sqlite, Inferred::Decimal | Inferred::Float) => "REAL",
        (Sqlite, Inferred::Boolean) => "BOOLEAN",
        (Sqlite, Inferred::Date) => "DATE",
        (Sqlite, Inferred::Timestamp) => "DATETIME",
        (Sqlite, Inferred::Text) => "TEXT",
        (Postgres, Inferred::Integer) => "bigint",
        (Postgres, Inferred::Decimal) => "numeric",
        (Postgres, Inferred::Float) => "double precision",
        (Postgres, Inferred::Boolean) => "boolean",
        (Postgres, Inferred::Date) => "date",
        (Postgres, Inferred::Timestamp) => "timestamp",
        (Postgres, Inferred::Text) => "text",
        (Mysql | Mariadb, Inferred::Integer) => "bigint",
        (Mysql | Mariadb, Inferred::Decimal) => "decimal(38,10)",
        (Mysql | Mariadb, Inferred::Float) => "double",
        (Mysql | Mariadb, Inferred::Boolean) => "tinyint(1)",
        (Mysql | Mariadb, Inferred::Date) => "date",
        (Mysql | Mariadb, Inferred::Timestamp) => "datetime",
        (Mysql | Mariadb, Inferred::Text) => "text",
        (Clickhouse, Inferred::Integer) => "Int64",
        (Clickhouse, Inferred::Decimal | Inferred::Float) => "Float64",
        (Clickhouse, Inferred::Boolean) => "Bool",
        (Clickhouse, Inferred::Date) => "Date",
        (Clickhouse, Inferred::Timestamp) => "DateTime",
        (Clickhouse, Inferred::Text) => "String",
        // Not SQL kinds: no table is created there.
        (_, Inferred::Text) => "text",
        (_, _) => "text",
    }
}

/// The columns of a new table: one per column of the file, typed by its sample.
pub(in crate::app) fn propose_columns(sample: &Sample, kind: ConnectionKind, hints: &Hints) -> Vec<NewColumn> {
    (0..sample.columns.len())
        .map(|index| {
            let cells = sample.rows.iter().map(|row| row.cells.get(index).and_then(|cell| cell.as_deref()));
            NewColumn { include: true, name: sample.columns[index].clone(), data_type: proposed_type(kind, Inferred::of(cells, hints)).to_owned(), nullable: true }
        })
        .collect()
}

/// The plans of the load for a new table: its included columns, from the file column each stands for.
pub(in crate::app) fn plans_for_new(columns: &[NewColumn]) -> Vec<ColumnPlan> {
    columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.include)
        .map(|(source, column)| ColumnPlan { source, target: column.name.clone(), converter: Converter::new(&column.data_type) })
        .collect()
}

/// The `CreateTable` for the included columns, checked the way the New table form checks its own: the names
/// through the dialect, the type text through the dialect's own generation. `primary_key` names columns that are
/// included (a transfer keeps the key of its source table; a file has none).
pub(in crate::app) fn create_table_change(
    dialect: &dyn DdlDialect,
    table: TableId,
    taken: &std::collections::HashSet<String>,
    columns: &[NewColumn],
    primary_key: &[String],
) -> Result<Change, String> {
    checked_name(dialect, "table", &table.name).map_err(|error| error.to_string())?;
    if taken.contains(&table.name) {
        return Err(format!("A table or view named \"{}\" exists already.", table.name));
    }
    let rules = dialect.name_rules();
    let mut specs: Vec<ColumnSpec> = Vec::new();
    for column in columns.iter().filter(|column| column.include) {
        let label = |problem: String| format!("Column \"{}\": {problem}", column.name);
        checked_name(dialect, "column", &column.name).map_err(|error| label(error.to_string()))?;
        if column.data_type.trim().is_empty() {
            return Err(label("needs a type.".to_owned()));
        }
        if specs.iter().any(|existing| rules.same(&existing.name, &column.name)) {
            return Err(label("there is a column with this name already.".to_owned()));
        }
        let nullable = column.nullable && !primary_key.contains(&column.name);
        specs.push(ColumnSpec { name: column.name.clone(), data_type: column.data_type.trim().to_owned(), nullable, ..ColumnSpec::default() });
    }
    if specs.is_empty() {
        return Err("A table needs at least one column: include one of the file's.".to_owned());
    }
    if let Some(missing) = primary_key.iter().find(|key| !specs.iter().any(|spec| spec.name == **key)) {
        return Err(format!("The primary key column \"{missing}\" is left out: include it, or the table has no primary key."));
    }
    let change = Change::CreateTable { table, columns: specs, primary_key: primary_key.to_vec(), comment: None };
    match plan_with_lock_timeout(dialect, std::slice::from_ref(&change), DEFAULT_LOCK_TIMEOUT_SECS).first_error() {
        Some((_, error)) => Err(error.to_string()),
        None => Ok(change),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::source::Row;

    fn target(name: &str, data_type: &str, nullable: bool) -> TargetColumn {
        TargetColumn { name: name.into(), data_type: data_type.into(), nullable, filled_by_server: false, generated: false }
    }

    fn sample(columns: &[&str], rows: &[&[Option<&str>]]) -> Sample {
        Sample {
            columns: columns.iter().map(|name| (*name).to_owned()).collect(),
            rows: rows
                .iter()
                .enumerate()
                .map(|(index, cells)| Row { number: index as u64 + 1, line: index as u64 + 2, cells: cells.iter().map(|cell| cell.map(str::to_owned)).collect(), extra: Vec::new() })
                .collect(),
            bad: Vec::new(),
            more: false,
            file_size: 0,
        }
    }

    #[test]
    fn columns_map_by_name_ignoring_case_each_target_once() {
        let targets = [target("id", "int", false), target("Name", "text", true), target("note", "text", true)];
        let sources: Vec<String> = ["NAME", "id", "extra", "name"].iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(auto_map(&sources, &targets), [Some(1), Some(0), None, None]);
    }

    #[test]
    fn a_generated_column_is_never_mapped_by_name() {
        let mut generated = target("total", "int", true);
        generated.generated = true;
        assert_eq!(auto_map(&["total".to_owned()], &[generated]), [None]);
    }

    #[test]
    fn the_sample_flags_values_the_column_would_refuse() {
        let data = sample(&["id", "price"], &[&[Some("1"), Some("1.5")], &[Some("x"), Some("2")], &[Some("3"), Some("abc")], &[None, Some("4")]]);
        let targets = [target("id", "integer", false), target("price", "numeric(10,2)", true)];
        let notes = check_sample(&data, &[Some(0), Some(1)], &targets, &Hints::default());
        let messages: Vec<&str> = notes.iter().map(|note| note.message.as_str()).collect();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(messages[0].contains("1 of 4 values is not a whole number for integer \"id\" (row 2: \"x\")"), "{}", messages[0]);
        assert!(messages[1].contains("1 of 4 values of \"id\" are empty, and \"id\" does not accept NULL"), "{}", messages[1]);
        assert!(messages[2].contains("is not a number for numeric(10,2) \"price\" (row 3: \"abc\")"), "{}", messages[2]);
    }

    #[test]
    fn an_empty_cell_for_a_not_null_binary_column_is_the_usual_not_null_note() {
        let data = sample(&["id", "data"], &[&[Some("1"), Some("")], &[Some("2"), Some("0x00ff")]]);
        let mut nullable = target("data", "varbinary(8)", true);
        let targets = [target("id", "int", false), nullable.clone()];
        assert!(check_sample(&data, &[Some(0), Some(1)], &targets, &Hints::default()).is_empty(), "empty is NULL where the column allows it");
        nullable.nullable = false;
        let notes = check_sample(&data, &[Some(0), Some(1)], &[target("id", "int", false), nullable], &Hints::default());
        assert!(notes.iter().any(|note| note.message.contains("1 of 2 values of \"data\" are empty") && note.message.contains("does not accept NULL")), "{notes:?}");
    }

    #[test]
    fn a_not_null_column_nothing_goes_to_is_flagged_unless_the_server_fills_it() {
        let data = sample(&["a"], &[&[Some("1")]]);
        let mut filled = target("serial_id", "int", false);
        filled.filled_by_server = true;
        let targets = [target("a", "int", true), target("required", "text", false), filled];
        let notes = check_sample(&data, &[Some(0)], &targets, &Hints::default());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].source.is_none() && notes[0].message.contains("\"required\" does not accept NULL"), "{notes:?}");
    }

    #[test]
    fn the_engine_is_read_from_the_create_statement_and_only_known_ones_have_transactions() {
        assert_eq!(engine_of_ddl("CREATE TABLE `t` (\n  `id` int\n) ENGINE=MyISAM DEFAULT CHARSET=utf8mb4").as_deref(), Some("MyISAM"));
        assert_eq!(engine_of_ddl("CREATE TABLE t (id int) ENGINE = InnoDB AUTO_INCREMENT=5").as_deref(), Some("InnoDB"));
        assert_eq!(engine_of_ddl("CREATE TABLE t (id int)"), None);
        // A comment that says engine=InnoDB on a MyISAM table, in the table or on a column, answers nothing.
        let tricked = "CREATE TABLE `t` (\n  `id` int COMMENT 'engine=InnoDB (not really)',\n  `n` int DEFAULT 0\n) ENGINE=MyISAM DEFAULT CHARSET=utf8mb4 COMMENT='ENGINE=InnoDB; it\\'s \")engine = Aria\"'";
        assert_eq!(engine_of_ddl(tricked).as_deref(), Some("MyISAM"));
        let before = "CREATE TABLE `engine=InnoDB` (\n  `engine` int\n) COMMENT='engine=InnoDB' ENGINE=Aria";
        assert_eq!(engine_of_ddl(before).as_deref(), Some("Aria"));
        assert_eq!(engine_of_ddl("CREATE TABLE t (id int) COMMENT='engine=InnoDB'"), None, "a comment is not an engine");
        // Column definitions with parentheses and quotes do not end the column list early.
        assert_eq!(engine_of_ddl("CREATE TABLE t (a decimal(10,2), b enum('x)','y(')) ENGINE=MEMORY").as_deref(), Some("MEMORY"));
        // A parenthesis or a doubled backtick in the table's own name does not open the column list.
        let named = "CREATE TABLE `a(b``c` (\n  `id` int\n) ENGINE=MyISAM COMMENT='x`y) ENGINE=InnoDB'";
        assert_eq!(engine_of_ddl(named).as_deref(), Some("MyISAM"));
        assert!(is_transactional_engine("InnoDB") && is_transactional_engine("innodb"));
        assert!(!is_transactional_engine("MyISAM") && !is_transactional_engine("Aria") && !is_transactional_engine("MEMORY"));
    }

    #[test]
    fn each_column_keeps_its_target_by_name_when_the_table_is_described_again() {
        let targets = [target("b", "int", true), target("a", "int", true)];
        let previous = [Some("a".to_owned()), None, Some("b".to_owned()), Some("gone".to_owned())];
        assert_eq!(remap_assignments(&previous, &targets), [Some(1), None, Some(0), None]);
    }

    #[test]
    fn json_keys_after_the_preview_go_to_the_unmapped_columns_of_the_table_by_name() {
        let targets = [target("id", "int", false), target("Qty", "int", true), target("note", "text", true)];
        let extras = extras_for(&[Some(0)], &targets);
        assert_eq!(extras.iter().map(|extra| extra.target.as_str()).collect::<Vec<_>>(), ["Qty", "note"]);
        assert!(extras.iter().any(|extra| extra.key.eq_ignore_ascii_case("qty")));
        assert!(extras_for(&[Some(0), Some(1), Some(2)], &targets).is_empty());
    }

    #[test]
    fn nothing_mapped_is_one_note() {
        let data = sample(&["a"], &[&[Some("1")]]);
        let notes = check_sample(&data, &[None], &[target("b", "int", true)], &Hints::default());
        assert!(notes.iter().any(|note| note.message.contains("No column of the file is mapped")));
    }

    #[test]
    fn a_new_table_is_typed_by_its_sample_in_the_words_of_the_dialect() {
        let data = sample(
            &["id", "price", "ok", "born", "seen", "name"],
            &[&[Some("1"), Some("1.5"), Some("true"), Some("2020-01-01"), Some("2020-01-01 10:00:00"), Some("a")], &[Some("2"), Some("2"), Some("false"), None, None, None]],
        );
        let types = |kind| propose_columns(&data, kind, &Hints::default()).into_iter().map(|column| column.data_type).collect::<Vec<_>>();
        assert_eq!(types(ConnectionKind::Postgres), ["bigint", "numeric", "boolean", "date", "timestamp", "text"]);
        assert_eq!(types(ConnectionKind::Sqlite), ["INTEGER", "REAL", "BOOLEAN", "DATE", "DATETIME", "TEXT"]);
        assert_eq!(types(ConnectionKind::Mysql), ["bigint", "decimal(38,10)", "tinyint(1)", "date", "datetime", "text"]);
        assert_eq!(types(ConnectionKind::Clickhouse), ["Int64", "Float64", "Bool", "Date", "DateTime", "String"]);
    }

    #[test]
    fn the_create_table_is_checked_by_the_dialect_before_it_is_reviewed() {
        let dialect = crate::sql_ddl::dialect_for(ConnectionKind::Postgres).unwrap();
        let table = |name: &str| TableId { schema: Some("public".into()), name: name.into() };
        let column = |name: &str, data_type: &str, include: bool| NewColumn { include, name: name.into(), data_type: data_type.into(), nullable: true };
        let taken: std::collections::HashSet<String> = ["existing".to_owned()].into();
        let ok = create_table_change(dialect, table("fresh"), &taken, &[column("id", "bigint", true), column("skipped", "text", false)], &[]).unwrap();
        assert!(matches!(&ok, Change::CreateTable { columns, .. } if columns.len() == 1));
        assert!(create_table_change(dialect, table("existing"), &taken, &[column("id", "bigint", true)], &[]).unwrap_err().contains("exists already"));
        assert!(create_table_change(dialect, table("fresh"), &taken, &[column("a", "int", true), column("a", "int", true)], &[]).unwrap_err().contains("already"));
        assert!(create_table_change(dialect, table("fresh"), &taken, &[column("a", "", true)], &[]).unwrap_err().contains("needs a type"));
        assert!(create_table_change(dialect, table("fresh"), &taken, &[column("a", "int", false)], &[]).unwrap_err().contains("at least one column"));
        assert!(create_table_change(dialect, table("fresh"), &taken, &[column("a", "int); DROP TABLE x; --", true)], &[]).is_err());
    }
}
