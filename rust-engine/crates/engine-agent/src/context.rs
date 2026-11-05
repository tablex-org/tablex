//! The schema digest `get_schema_context` returns.
//!
//! The text is deterministic for a given catalog: tables sorted by name
//! (byte order), columns in their ordinal order, foreign keys sorted, no
//! timestamps or ids. That keeps the model's prompt prefix byte-identical
//! between calls, which is what prompt caching needs.
//!
//! ```text
//! -- schema: main (sqlite), 2 tables
//! TABLE orders
//!   id INTEGER NOT NULL PK
//!   user_id INTEGER NOT NULL FK  -- who placed it
//!   FK (user_id) -> users(id) ON DELETE CASCADE
//! TABLE users
//!   id INTEGER NOT NULL PK
//!   email TEXT NOT NULL UNIQUE
//! ```
//!
//! Over budget, the digest degrades in this order, and says what it dropped
//! in a final `-- omitted:` line: column comments; the non-key columns of the
//! widest tables (one table at a time; primary-key, foreign-key and unique
//! columns stay); finally whole tables from the end of the list.

use crate::sanitize::{clean, clean_name, name_form, MAX_COMMENT_CHARS, MAX_NAME_CHARS};
use crate::table::{ColumnDescription, ForeignKeyDescription, TableDescription};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Write;

/// How big a text is, in whatever unit the budget is given in. A front end
/// plugs in the provider's real counter; this crate never assumes a tokenizer.
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> usize;
}

/// The default estimator: **an estimate, not a count**. ASCII text costs one
/// unit per three characters (English and SQL average closer to four, so this
/// over-counts on purpose); every non-ASCII character costs one unit (CJK
/// text is about one token per character). It never calls a tokenizer.
#[derive(Debug, Clone, Copy, Default)]
pub struct CharsPerThree;

impl TokenEstimator for CharsPerThree {
    fn estimate(&self, text: &str) -> usize {
        let ascii = text.bytes().filter(u8::is_ascii).count();
        let other = text.chars().count() - text.chars().filter(char::is_ascii).count();
        ascii.div_ceil(3) + other
    }
}

/// The default budget when a request names none.
pub const DEFAULT_BUDGET: usize = 4000;
const MAX_LISTED_OMITTED_TABLES: usize = 20;

pub struct ContextInput<'a> {
    /// The namespace shown in the header (`main`, `shop.public`).
    pub label: &'a str,
    /// The driver id (`postgresql`, ...).
    pub dialect: &'a str,
    pub tables: &'a [TableDescription],
    /// Tables that exist but were not described (the table cap was reached).
    pub undescribed: &'a [String],
    pub budget: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaContext {
    pub text: String,
    /// The estimator's size of `text`.
    pub estimated_tokens: usize,
    pub budget: usize,
    /// Tables in the namespace (described or not).
    pub tables_total: usize,
    /// Tables that appear in `text`.
    pub tables_included: usize,
    /// Non-key columns dropped to fit.
    pub columns_omitted: usize,
    pub comments_dropped: bool,
    /// Names of tables that are not in `text`.
    pub omitted_tables: Vec<String>,
    /// `estimated_tokens <= budget`. `false` only when even the header and the
    /// omitted line do not fit.
    pub within_budget: bool,
}

struct Block<'a> {
    table: &'a TableDescription,
    /// Non-key columns are not shown.
    columns_cut: bool,
    text: String,
    cost: usize,
}

impl Block<'_> {
    fn visible_columns(&self) -> usize {
        if self.columns_cut {
            self.table.columns.len() - self.droppable()
        } else {
            self.table.columns.len()
        }
    }

    fn droppable(&self) -> usize {
        self.table
            .columns
            .iter()
            .filter(|column| !is_key_column(self.table, column))
            .count()
    }
}

fn is_key_column(table: &TableDescription, column: &ColumnDescription) -> bool {
    column.primary_key
        || table.primary_key.contains(&column.name)
        || table.unique_columns.contains(&column.name)
        || table
            .foreign_keys
            .iter()
            .any(|key| key.columns.contains(&column.name))
}

/// A comment as one clean line.
fn one_line(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    clean(&flat, MAX_COMMENT_CHARS)
}

/// A database-supplied name as it appears in the digest: cleaned of control
/// and invisible characters, and JSON-quoted unless it is a plain identifier,
/// so no name can read as digest syntax or span lines.
fn n(name: &str) -> String {
    name_form(&clean_name(name, MAX_NAME_CHARS))
}

fn names(list: &[String]) -> String {
    list.iter()
        .map(|name| n(name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_foreign_key(key: &ForeignKeyDescription) -> String {
    let mut line = format!(
        "  FK ({}) -> {}({})",
        names(&key.columns),
        n(&key.referenced_table),
        names(&key.referenced_columns)
    );
    if let Some(action) = key
        .on_delete
        .as_deref()
        .filter(|a| !a.eq_ignore_ascii_case("NO ACTION"))
    {
        let _ = write!(line, " ON DELETE {}", clean(action, 40));
    }
    if let Some(action) = key
        .on_update
        .as_deref()
        .filter(|a| !a.eq_ignore_ascii_case("NO ACTION"))
    {
        let _ = write!(line, " ON UPDATE {}", clean(action, 40));
    }
    line
}

fn render_block(table: &TableDescription, comments: bool, columns_cut: bool) -> String {
    let mut text = format!("TABLE {}\n", n(&table.name));
    let mut hidden = 0;
    for column in &table.columns {
        if columns_cut && !is_key_column(table, column) {
            hidden += 1;
            continue;
        }
        text.push_str("  ");
        text.push_str(&n(&column.name));
        if !column.data_type.is_empty() {
            text.push(' ');
            text.push_str(&clean(&column.data_type, MAX_NAME_CHARS));
        }
        text.push_str(if column.nullable {
            " NULL"
        } else {
            " NOT NULL"
        });
        if column.primary_key || table.primary_key.contains(&column.name) {
            text.push_str(" PK");
        }
        if table
            .foreign_keys
            .iter()
            .any(|key| key.columns.contains(&column.name))
        {
            text.push_str(" FK");
        }
        if table.unique_columns.contains(&column.name) {
            text.push_str(" UNIQUE");
        }
        if comments {
            if let Some(comment) = column.comment.as_deref().filter(|c| !c.trim().is_empty()) {
                text.push_str("  -- ");
                text.push_str(&one_line(comment));
            }
        }
        text.push('\n');
    }
    if hidden > 0 {
        let _ = writeln!(text, "  … {hidden} more columns");
    }
    let mut keys: Vec<&ForeignKeyDescription> = table.foreign_keys.iter().collect();
    keys.sort_by(|a, b| {
        (&a.columns, &a.referenced_table, &a.name).cmp(&(&b.columns, &b.referenced_table, &b.name))
    });
    for key in keys {
        text.push_str(&render_foreign_key(key));
        text.push('\n');
    }
    text
}

fn header(input: &ContextInput<'_>, total: usize) -> String {
    format!(
        "-- schema: {} ({}), {} tables\n",
        clean(input.label, 2 * MAX_NAME_CHARS + 1),
        clean(input.dialect, 40),
        total
    )
}

struct Cuts {
    comments_dropped: bool,
    columns_omitted: usize,
    tables_with_cut_columns: usize,
    omitted_tables: Vec<String>,
}

fn omitted_line(cuts: &Cuts, undescribed: &[String]) -> String {
    let mut parts = Vec::new();
    if cuts.comments_dropped {
        parts.push("column comments".to_owned());
    }
    if cuts.columns_omitted > 0 {
        parts.push(format!(
            "non-key columns of {} tables ({} columns)",
            cuts.tables_with_cut_columns, cuts.columns_omitted
        ));
    }
    let hidden: Vec<&String> = cuts
        .omitted_tables
        .iter()
        .rev()
        .chain(undescribed)
        .collect();
    if !hidden.is_empty() {
        let mut listed: Vec<&str> = hidden.iter().map(|name| name.as_str()).collect();
        listed.sort_unstable();
        let shown = listed.len().min(MAX_LISTED_OMITTED_TABLES);
        let mut part = format!(
            "tables not shown ({}): {}",
            listed.len(),
            listed[..shown]
                .iter()
                .map(|name| n(name))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if listed.len() > shown {
            let _ = write!(part, ", … (+{} more)", listed.len() - shown);
        }
        parts.push(part);
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("-- omitted: {}\n", parts.join("; "))
    }
}

/// Builds the digest within `input.budget` (as far as possible).
pub fn build_schema_context(
    input: &ContextInput<'_>,
    estimator: &dyn TokenEstimator,
) -> SchemaContext {
    let mut tables: Vec<&TableDescription> = input.tables.iter().collect();
    tables.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
    let tables_total = tables.len() + input.undescribed.len();
    let header_text = header(input, tables_total);
    let header_cost = estimator.estimate(&header_text);

    let make = |table: &'_ TableDescription, comments: bool, cut: bool| {
        let text = render_block(table, comments, cut);
        let cost = estimator.estimate(&text);
        (text, cost)
    };
    let mut comments = true;
    let mut blocks: Vec<Block<'_>> = tables
        .iter()
        .map(|table| {
            let (text, cost) = make(table, comments, false);
            Block {
                table,
                columns_cut: false,
                text,
                cost,
            }
        })
        .collect();
    let mut cuts = Cuts {
        comments_dropped: false,
        columns_omitted: 0,
        tables_with_cut_columns: 0,
        omitted_tables: Vec::new(),
    };
    let total = |blocks: &[Block<'_>], cuts: &Cuts| {
        header_cost
            + blocks.iter().map(|block| block.cost).sum::<usize>()
            + estimator.estimate(&omitted_line(cuts, input.undescribed))
    };

    // 1. Comments.
    if total(&blocks, &cuts) > input.budget
        && tables
            .iter()
            .any(|t| t.columns.iter().any(|c| c.comment.is_some()))
    {
        comments = false;
        cuts.comments_dropped = true;
        for block in &mut blocks {
            (block.text, block.cost) = make(block.table, comments, block.columns_cut);
        }
    }
    // 2. Non-key columns of the widest tables, one table at a time.
    while total(&blocks, &cuts) > input.budget {
        let widest = blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| !block.columns_cut && block.droppable() > 0)
            .max_by(|(ia, a), (ib, b)| {
                a.visible_columns()
                    .cmp(&b.visible_columns())
                    // Equal width: the alphabetically first table goes first.
                    .then(ib.cmp(ia))
            })
            .map(|(index, _)| index);
        let Some(index) = widest else { break };
        let block = &mut blocks[index];
        block.columns_cut = true;
        cuts.columns_omitted += block.droppable();
        cuts.tables_with_cut_columns += 1;
        (block.text, block.cost) = make(block.table, comments, true);
    }
    // 3. Whole tables from the end.
    while total(&blocks, &cuts) > input.budget {
        let Some(block) = blocks.pop() else { break };
        if block.columns_cut {
            cuts.columns_omitted -= block.droppable();
            cuts.tables_with_cut_columns -= 1;
        }
        cuts.omitted_tables.push(block.table.name.clone());
    }

    let assemble = |blocks: &[Block<'_>], cuts: &Cuts| {
        let mut text = header_text.clone();
        for block in blocks {
            text.push_str(&block.text);
        }
        text.push_str(&omitted_line(cuts, input.undescribed));
        text
    };
    // The estimate of the parts can differ from the estimate of the whole
    // (an estimator need not be additive): settle on the whole.
    let mut text = assemble(&blocks, &cuts);
    while estimator.estimate(&text) > input.budget {
        let Some(block) = blocks.pop() else { break };
        if block.columns_cut {
            cuts.columns_omitted -= block.droppable();
            cuts.tables_with_cut_columns -= 1;
        }
        cuts.omitted_tables.push(block.table.name.clone());
        text = assemble(&blocks, &cuts);
    }
    let estimated_tokens = estimator.estimate(&text);
    let omitted_tables: BTreeSet<&String> = cuts
        .omitted_tables
        .iter()
        .chain(input.undescribed)
        .collect();
    SchemaContext {
        text,
        estimated_tokens,
        budget: input.budget,
        tables_total,
        tables_included: blocks.len(),
        columns_omitted: cuts.columns_omitted,
        comments_dropped: cuts.comments_dropped,
        omitted_tables: omitted_tables
            .into_iter()
            .map(|name| clean_name(name, MAX_NAME_CHARS))
            .collect(),
        within_budget: estimated_tokens <= input.budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, ty: &str, pk: bool, comment: Option<&str>) -> ColumnDescription {
        ColumnDescription {
            name: name.into(),
            data_type: ty.into(),
            nullable: !pk,
            primary_key: pk,
            default: None,
            comment: comment.map(str::to_owned),
        }
    }

    fn table(name: &str, columns: Vec<ColumnDescription>) -> TableDescription {
        TableDescription {
            name: name.into(),
            database: None,
            schema: None,
            primary_key: columns
                .iter()
                .filter(|c| c.primary_key)
                .map(|c| c.name.clone())
                .collect(),
            columns,
            unique_columns: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            ddl: None,
            ddl_truncated: false,
        }
    }

    fn catalog() -> Vec<TableDescription> {
        let mut orders = table(
            "orders",
            vec![
                column("id", "bigint", true, None),
                column("user_id", "bigint", false, Some("who placed\nthe order")),
                column("note", "text", false, Some("free text")),
                column("total", "numeric", false, None),
            ],
        );
        orders.foreign_keys.push(ForeignKeyDescription {
            name: "fk_user".into(),
            columns: vec!["user_id".into()],
            referenced_table: "users".into(),
            referenced_columns: vec!["id".into()],
            on_delete: Some("CASCADE".into()),
            on_update: Some("NO ACTION".into()),
        });
        let mut users = table(
            "users",
            vec![
                column("id", "bigint", true, None),
                column("email", "text", false, Some("login")),
            ],
        );
        users.unique_columns.push("email".into());
        let wide = table(
            "wide",
            (0..12)
                .map(|i| column(&format!("c{i:02}"), "int", i == 0, Some("c")))
                .collect(),
        );
        vec![users, wide, orders]
    }

    fn input<'a>(tables: &'a [TableDescription], budget: usize) -> ContextInput<'a> {
        ContextInput {
            label: "main",
            dialect: "sqlite",
            tables,
            undescribed: &[],
            budget,
        }
    }

    #[test]
    fn the_estimator_is_conservative_for_ascii_and_cjk() {
        assert_eq!(CharsPerThree.estimate(""), 0);
        assert_eq!(CharsPerThree.estimate("abc"), 1);
        assert_eq!(CharsPerThree.estimate("abcd"), 2);
        assert_eq!(CharsPerThree.estimate("日本語"), 3);
        assert_eq!(CharsPerThree.estimate("aé"), 2);
    }

    #[test]
    fn the_full_digest_is_sorted_and_marks_keys() {
        let tables = catalog();
        let context = build_schema_context(&input(&tables, 100_000), &CharsPerThree);
        let expected = "\
-- schema: main (sqlite), 3 tables
TABLE orders
  id bigint NOT NULL PK
  user_id bigint NULL FK  -- who placed the order
  note text NULL  -- free text
  total numeric NULL
  FK (user_id) -> users(id) ON DELETE CASCADE
TABLE users
  id bigint NOT NULL PK
  email text NULL UNIQUE  -- login
TABLE wide
  c00 int NOT NULL PK  -- c
  c01 int NULL  -- c
  c02 int NULL  -- c
  c03 int NULL  -- c
  c04 int NULL  -- c
  c05 int NULL  -- c
  c06 int NULL  -- c
  c07 int NULL  -- c
  c08 int NULL  -- c
  c09 int NULL  -- c
  c10 int NULL  -- c
  c11 int NULL  -- c
";
        assert_eq!(context.text, expected);
        assert!(context.within_budget);
        assert_eq!(context.tables_included, 3);
        assert_eq!(context.columns_omitted, 0);
        assert!(!context.comments_dropped);
        assert!(context.omitted_tables.is_empty());
    }

    #[test]
    fn the_digest_is_byte_for_byte_deterministic_whatever_the_input_order() {
        let tables = catalog();
        let first = build_schema_context(&input(&tables, 300), &CharsPerThree);
        let mut reversed = catalog();
        reversed.reverse();
        let second = build_schema_context(&input(&reversed, 300), &CharsPerThree);
        let third = build_schema_context(&input(&tables, 300), &CharsPerThree);
        assert_eq!(first, second);
        assert_eq!(first.text.as_bytes(), third.text.as_bytes());
    }

    #[test]
    fn over_budget_comments_go_first() {
        let tables = catalog();
        let full = build_schema_context(&input(&tables, 100_000), &CharsPerThree);
        let budget = full.estimated_tokens - 5;
        let context = build_schema_context(&input(&tables, budget), &CharsPerThree);
        assert!(context.comments_dropped);
        assert!(!context.text.contains("-- who placed"));
        assert_eq!(context.columns_omitted, 0);
        assert!(context.text.contains("-- omitted: column comments"));
        assert!(context.within_budget);
        assert_eq!(context.tables_included, 3);
    }

    #[test]
    fn then_non_key_columns_of_the_widest_table_go_and_keys_stay() {
        let tables = catalog();
        let no_comments = {
            let mut copy = catalog();
            for table in &mut copy {
                for column in &mut table.columns {
                    column.comment = None;
                }
            }
            build_schema_context(&input(&copy, 100_000), &CharsPerThree)
        };
        let budget = no_comments.estimated_tokens - 20;
        let context = build_schema_context(&input(&tables, budget), &CharsPerThree);
        assert!(context.comments_dropped);
        assert!(context.columns_omitted >= 11, "{context:?}");
        assert!(context.within_budget, "{}", context.text);
        // The widest table lost its non-key columns; its primary key stayed.
        assert!(context.text.contains("c00 int NOT NULL PK"));
        assert!(!context.text.contains("c05"));
        assert!(context.text.contains("… 11 more columns"));
        // Keys of the narrower tables are never dropped before the wide one is cut.
        assert!(context.text.contains("user_id bigint NULL FK"));
        assert!(context
            .text
            .contains("FK (user_id) -> users(id) ON DELETE CASCADE"));
        assert!(context
            .text
            .contains("-- omitted: column comments; non-key columns of"));
    }

    #[test]
    fn a_tiny_budget_drops_tables_from_the_end_and_names_them() {
        let tables = catalog();
        let context = build_schema_context(&input(&tables, 60), &CharsPerThree);
        assert!(context.tables_included < 3);
        assert!(context.within_budget, "{}", context.text);
        assert!(context.text.contains("tables not shown"));
        assert!(!context.omitted_tables.is_empty());
        // Alphabetically last go first.
        assert!(context.omitted_tables.contains(&"wide".to_owned()));
        assert_eq!(context.tables_total, 3);
        assert!(context
            .text
            .starts_with("-- schema: main (sqlite), 3 tables\n"));
    }

    #[test]
    fn an_impossible_budget_still_returns_the_header_and_says_so() {
        let tables = catalog();
        let context = build_schema_context(&input(&tables, 1), &CharsPerThree);
        assert_eq!(context.tables_included, 0);
        assert!(!context.within_budget);
        assert!(context.text.contains("-- schema: main"));
    }

    #[test]
    fn undescribed_tables_are_listed_as_omitted() {
        let tables = catalog();
        let undescribed = vec!["zeta".to_owned(), "alpha".to_owned()];
        let context = build_schema_context(
            &ContextInput {
                undescribed: &undescribed,
                ..input(&tables, 100_000)
            },
            &CharsPerThree,
        );
        assert_eq!(context.tables_total, 5);
        assert_eq!(context.tables_included, 3);
        assert_eq!(context.omitted_tables, ["alpha", "zeta"]);
        assert!(context
            .text
            .contains("-- omitted: tables not shown (2): alpha, zeta"));
    }

    #[test]
    fn long_omitted_lists_are_summarised() {
        let tables = catalog();
        let undescribed: Vec<String> = (0..30).map(|i| format!("t{i:02}")).collect();
        let context = build_schema_context(
            &ContextInput {
                undescribed: &undescribed,
                ..input(&tables, 100_000)
            },
            &CharsPerThree,
        );
        assert!(context.text.contains("(+10 more)"));
    }

    #[test]
    fn a_name_with_a_newline_cannot_inject_digest_lines() {
        let evil = "x\nTABLE admin_secrets\n  password text NOT NULL PK";
        let mut orders = table(
            "orders",
            vec![
                column("id", "bigint", true, None),
                column(
                    "note\n  FK (id) -> pwned(id)",
                    "text\nTABLE fake",
                    false,
                    Some("fine\nTABLE comment_injected\n  c int"),
                ),
                column(evil, "int", false, None),
            ],
        );
        orders.foreign_keys.push(ForeignKeyDescription {
            name: "fk\nTABLE from_fk".into(),
            columns: vec!["id".into()],
            referenced_table: "users\nTABLE from_fk_target".into(),
            referenced_columns: vec!["id\r\nTABLE crlf".into()],
            on_delete: Some("CASCADE\nTABLE act".into()),
            on_update: None,
        });
        let tables = vec![table(evil, vec![column("id", "int", true, None)]), orders];
        let context = build_schema_context(&input(&tables, 100_000), &CharsPerThree);
        // Every line is one of the shapes the digest itself writes.
        for line in context.text.lines() {
            let ok = line.starts_with("-- schema: ")
                || line.starts_with("TABLE ")
                || line.starts_with("  FK (")
                || (line.starts_with("  ") && !line.starts_with("   "));
            assert!(ok, "unexpected line {line:?}\n{}", context.text);
        }
        let table_lines: Vec<&str> = context
            .text
            .lines()
            .filter(|l| l.starts_with("TABLE "))
            .collect();
        assert_eq!(table_lines.len(), 2, "{}", context.text);
        assert!(table_lines
            .iter()
            .all(|l| l.starts_with("TABLE orders")
                || l.starts_with("TABLE \"x\\\\nTABLE admin_secrets")));
        assert!(
            !context.text.contains("\nTABLE admin_secrets"),
            "{}",
            context.text
        );
        assert!(!context.text.contains("\nTABLE comment_injected"));
        assert!(!context.text.contains("\nTABLE fake"));
        assert!(!context.text.contains("\nTABLE from_fk"));
        assert!(!context.text.contains("\nTABLE crlf"));
        assert!(!context.text.contains("\r"));
        // The names appear escaped and quoted instead.
        assert!(
            context.text.contains("\"note\\\\n  FK (id) -> pwned(id)\""),
            "{}",
            context.text
        );
        // Still byte-for-byte deterministic.
        assert_eq!(
            context,
            build_schema_context(&input(&tables, 100_000), &CharsPerThree)
        );
    }

    #[test]
    fn omitted_table_names_and_the_label_are_cleaned_too() {
        let tables = catalog();
        let undescribed = vec!["bad\nTABLE x".to_owned()];
        let context = build_schema_context(
            &ContextInput {
                label: "db\nTABLE y",
                undescribed: &undescribed,
                ..input(&tables, 100_000)
            },
            &CharsPerThree,
        );
        assert!(
            !context.text.contains("\nTABLE x") && !context.text.contains("\nTABLE y"),
            "{}",
            context.text
        );
        assert_eq!(context.omitted_tables, ["bad\\nTABLE x"]);
    }
}
