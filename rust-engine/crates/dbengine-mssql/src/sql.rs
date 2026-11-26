//! Pure SQL text helpers: identifier quoting, what a text is (`analyze`), the `GO` batch
//! separator and server-side paging. Nothing here touches the network.
//!
//! `engine_driver_kit::sql::classify` splits statements at `;` only, but T-SQL does not need a
//! `;` between statements (`SELECT 1 DELETE FROM t` is two statements), so a kit "pure read" can
//! hide a write. `analyze` therefore also scans the *words* of every statement, outside quotes
//! and comments, for what writes or executes (`INSERT`, `UPDATE`, `DELETE`, `MERGE`, `EXEC`,
//! `INTO`, `CREATE`, `DROP`, ...). A text that has any such word is never a pageable read and is
//! refused by a read-only run; a hidden `DELETE`/`UPDATE`/`DROP` after another statement needs the
//! user's confirmation. The scan is conservative: a word that merely looks like one (a `MERGE
//! JOIN` hint) makes the text look riskier, never safer. T-SQL functions cannot write tables,
//! so a text without such words cannot write (the blind spots are `OPENQUERY`/`OPENROWSET` over a
//! linked server, which are listed among the words, and CLR code a login may call).

use engine_driver_kit::sql::{classify, token_iter, tokens, Keyword, SqlLexOptions, SqlTokenKind};
use std::ops::Range;

pub(crate) const fn lex_options() -> SqlLexOptions {
    SqlLexOptions::mssql()
}

/// Appends `name` as a bracket-quoted identifier (`]` doubled).
pub(crate) fn push_ident(out: &mut String, name: &str) {
    out.push('[');
    for ch in name.chars() {
        if ch == ']' {
            out.push(']');
        }
        out.push(ch);
    }
    out.push(']');
}

/// `[database].[schema].[object]`, leaving out the parts that are absent.
pub(crate) fn push_qualified(
    out: &mut String,
    database: Option<&str>,
    schema: Option<&str>,
    object: &str,
) {
    for part in [database, schema].into_iter().flatten().filter(|part| !part.is_empty()) {
        push_ident(out, part);
        out.push('.');
    }
    push_ident(out, object);
}

// ---------------------------------------------------------------------------- words

/// What the words of one statement say it does.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Words {
    /// `INSERT`/`UPDATE`/`DELETE`/`MERGE` words.
    dml: u32,
    /// `DROP`, `TRUNCATE`, `ALTER`, `GRANT`, `REVOKE`, `DENY`, `SHUTDOWN`, `KILL`, `RESTORE`.
    destructive: bool,
    /// `EXEC`/`EXECUTE`.
    exec: bool,
    /// `INTO`, `CREATE`, `BULK`, `BACKUP`, `DBCC`, `WRITETEXT`, `UPDATETEXT`, `OPENQUERY`,
    /// `OPENROWSET`, `OPENDATASOURCE`.
    other_write: bool,
    /// A `USE` outside parentheses.
    use_database: bool,
}

impl Words {
    fn writes(&self) -> bool {
        self.dml > 0 || self.destructive || self.exec || self.other_write || self.use_database
    }

    /// What the text does, for a refusal message.
    fn describe(&self) -> &'static str {
        if self.dml > 0 {
            "INSERT/UPDATE/DELETE/MERGE"
        } else if self.destructive {
            "DROP/TRUNCATE/ALTER/GRANT/REVOKE/DENY/KILL"
        } else if self.exec {
            "EXEC"
        } else if self.use_database {
            "USE"
        } else {
            "INTO/CREATE/BULK/BACKUP/DBCC/OPENQUERY/OPENROWSET"
        }
    }
}

fn lower<'a>(word: &str, buffer: &'a mut [u8; 16]) -> &'a str {
    let bytes = word.as_bytes();
    if bytes.len() > buffer.len() {
        return "";
    }
    for (slot, byte) in buffer.iter_mut().zip(bytes) {
        *slot = byte.to_ascii_lowercase();
    }
    std::str::from_utf8(&buffer[..bytes.len()]).unwrap_or("")
}

fn scan_words(text: &str, lex: &SqlLexOptions) -> Words {
    let mut words = Words::default();
    let mut depth = 0_i32;
    let mut buffer = [0u8; 16];
    for token in token_iter(text, lex) {
        match token.kind {
            SqlTokenKind::Open => depth += 1,
            SqlTokenKind::Close => depth -= 1,
            SqlTokenKind::Word => match lower(&text[token.range], &mut buffer) {
                "insert" | "update" | "delete" | "merge" => words.dml += 1,
                "drop" | "truncate" | "alter" | "grant" | "revoke" | "deny" | "shutdown" | "kill"
                | "restore" => words.destructive = true,
                "exec" | "execute" => words.exec = true,
                "into" | "create" | "bulk" | "backup" | "dbcc" | "writetext" | "updatetext"
                | "openquery" | "openrowset" | "opendatasource" => words.other_write = true,
                "use" if depth <= 0 => words.use_database = true,
                _ => {}
            },
            _ => {}
        }
    }
    words
}

// ---------------------------------------------------------------------------- analysis

/// One batch of the text: what runs between two `GO` lines, `repeat` times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Batch {
    pub range: Range<usize>,
    pub repeat: u32,
}

/// What a text is, for every rule of the SQL contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Analysis {
    pub batches: Vec<Batch>,
    /// Exactly one batch, once, holding exactly one statement that is a pure read.
    pub pageable: bool,
    /// The first reason the user must confirm, if any.
    pub danger: Option<&'static str>,
    /// Some statement may change the session's database (`USE`, `DROP`).
    pub changes_database: bool,
    /// Why a read-only run refuses the text, if it does.
    pub read_only_refusal: Option<String>,
}

fn contains_go(sql: &str) -> bool {
    sql.as_bytes().windows(2).any(|pair| pair.eq_ignore_ascii_case(b"go"))
}

/// Splits at lines that hold only `GO` (optionally followed by a repeat count and a `--`
/// comment), the batch separator of SSMS and `sqlcmd`: the server itself rejects it.
/// Batches with nothing but whitespace and comments are dropped.
pub(crate) fn split_batches(sql: &str, lex: &SqlLexOptions) -> Vec<Batch> {
    let whole = || {
        vec![Batch {
            range: 0..sql.len(),
            repeat: 1,
        }]
    };
    if !contains_go(sql) {
        return whole();
    }
    let scan = tokens(sql, lex);
    let mut batches = Vec::new();
    let mut from = 0;
    let mut found = false;
    for token in scan.tokens.iter().filter(|token| token.kind == SqlTokenKind::Word) {
        if !sql[token.range.clone()].eq_ignore_ascii_case("go") {
            continue;
        }
        let line_start = sql[..token.range.start].rfind('\n').map_or(0, |at| at + 1);
        if !sql[line_start..token.range.start].trim().is_empty() || line_start < from {
            continue;
        }
        let rest = &sql[token.range.end..];
        let line_end = rest.find('\n').map_or(rest.len(), |at| at + 1);
        let tail = rest[..line_end].split("--").next().unwrap_or("").trim();
        let repeat = if tail.is_empty() {
            1
        } else if tail.bytes().all(|byte| byte.is_ascii_digit()) {
            tail.parse::<u32>().unwrap_or(u32::MAX).clamp(1, 1000)
        } else {
            continue;
        };
        found = true;
        push_batch(&mut batches, sql, lex, from..line_start, repeat);
        from = token.range.end + line_end;
    }
    if !found {
        return whole();
    }
    push_batch(&mut batches, sql, lex, from..sql.len(), 1);
    batches
}

fn push_batch(
    batches: &mut Vec<Batch>,
    sql: &str,
    lex: &SqlLexOptions,
    range: Range<usize>,
    repeat: u32,
) {
    if token_iter(&sql[range.clone()], lex).next().is_some() {
        batches.push(Batch { range, repeat });
    }
}

pub(crate) fn analyze(sql: &str, lex: &SqlLexOptions) -> Analysis {
    let batches = split_batches(sql, lex);
    let mut analysis = Analysis {
        pageable: false,
        danger: None,
        changes_database: false,
        read_only_refusal: None,
        batches: Vec::new(),
    };
    let mut statements = 0_usize;
    let mut single_read = false;
    for batch in &batches {
        let text = &sql[batch.range.clone()];
        let kit = classify(text, lex);
        statements += kit.statements.len();
        analysis.changes_database |= kit.changes_database;
        if analysis.danger.is_none() {
            analysis.danger = kit.danger.map(|danger| danger.reason);
        }
        if analysis.read_only_refusal.is_none() {
            analysis.read_only_refusal = kit.read_only_refusal().map(|refusal| refusal.to_string());
        }
        let creates = kit
            .statements
            .iter()
            .any(|statement| matches!(statement.keyword, Some(Keyword::Create | Keyword::Merge)));
        for statement in &kit.statements {
            let words = scan_words(&text[statement.range.clone()], lex);
            analysis.changes_database |= words.use_database
                || statement.keyword == Some(Keyword::Drop)
                || (words.destructive && !creates);
            if statement.read_only && words.writes() {
                // A pure read in the kit's eyes with a write behind it, no `;` between.
                analysis.read_only_refusal.get_or_insert_with(|| {
                    format!(
                        "a read-only execution takes plain reads; this text also has {}",
                        words.describe()
                    )
                });
                if analysis.danger.is_none() && !creates && (words.dml > 0 || words.destructive) {
                    analysis.danger = Some(
                        "a statement follows a SELECT without `;` and writes or drops (INSERT/UPDATE/DELETE/DROP/ALTER ...)",
                    );
                }
            } else if !statement.read_only
                && !creates
                && analysis.danger.is_none()
                && matches!(
                    statement.keyword,
                    Some(Keyword::Insert | Keyword::Update | Keyword::Delete)
                )
                && (words.dml >= 2 || words.destructive)
            {
                analysis.danger = Some(
                    "several INSERT/UPDATE/DELETE statements without `;`: the WHERE check can only look at the first",
                );
            }
        }
        single_read = kit.pageable && kit.statements.iter().all(|statement| {
            !scan_words(&text[statement.range.clone()], lex).writes()
        });
    }
    analysis.pageable = batches.len() == 1 && batches[0].repeat == 1 && statements == 1 && single_read;
    analysis.batches = batches;
    analysis
}

// ---------------------------------------------------------------------------- paging

/// `statement` (one pure read) with `OFFSET n ROWS FETCH NEXT limit+1 ROWS ONLY` appended, so
/// the server skips the rows of earlier pages and the caller can tell by the extra row whether
/// the result continues. `None` when the statement cannot take it, which is when it has no
/// top-level `ORDER BY` (`OFFSET` needs one), or has its own `TOP`/`OFFSET`/`FETCH`, a `FOR`
/// clause (`FOR XML/JSON/BROWSE`), `INTO` or `OPTION`: the caller then pages client-side.
pub(crate) fn with_offset_fetch(
    statement: &str,
    lex: &SqlLexOptions,
    offset: u64,
    limit: u64,
) -> Option<String> {
    let mut depth = 0_i32;
    let mut first = true;
    let mut order_by = false;
    let mut previous_was_order = false;
    let mut buffer = [0u8; 16];
    for token in token_iter(statement, lex) {
        let was_order = std::mem::take(&mut previous_was_order);
        match token.kind {
            SqlTokenKind::Open => depth += 1,
            SqlTokenKind::Close => depth -= 1,
            SqlTokenKind::Word => {
                let word = lower(&statement[token.range], &mut buffer);
                if first && !matches!(word, "select" | "with") {
                    return None;
                }
                if depth == 0 {
                    match word {
                        "order" => previous_was_order = true,
                        "by" if was_order => order_by = true,
                        "top" | "offset" | "fetch" | "for" | "into" | "option" => return None,
                        _ => {}
                    }
                }
            }
            SqlTokenKind::Semicolon => return None,
            _ => {}
        }
        first = false;
    }
    if !order_by {
        return None;
    }
    // A newline first: the statement may end in a `-- comment`.
    Some(format!(
        "{statement}\nOFFSET {offset} ROWS FETCH NEXT {} ROWS ONLY",
        limit.saturating_add(1)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEX: SqlLexOptions = lex_options();

    fn analysis(sql: &str) -> Analysis {
        analyze(sql, &LEX)
    }

    #[test]
    fn identifiers_double_their_closing_bracket() {
        let mut out = String::new();
        push_qualified(&mut out, Some("my]db"), Some("dbo"), "t[1]");
        assert_eq!(out, "[my]]db].[dbo].[t[1]]]");
        let mut out = String::new();
        push_qualified(&mut out, None, None, "t");
        assert_eq!(out, "[t]");
    }

    #[test]
    fn a_single_select_is_pageable_and_anything_with_a_write_word_is_not() {
        assert!(analysis("SELECT * FROM t WHERE a = 'delete'").pageable);
        assert!(analysis("select [update], \"drop\" from t -- insert").pageable);
        assert!(analysis("WITH c AS (SELECT 1 a) SELECT * FROM c").pageable);
        assert!(!analysis("SELECT * INTO #t FROM t").pageable);
        assert!(!analysis("SELECT 1; SELECT 2").pageable);
        assert!(!analysis("EXEC sp_who").pageable);
        assert!(!analysis("SELECT 1 DELETE FROM t").pageable);
        assert!(!analysis("WITH c AS (SELECT 1 a) DELETE FROM c").pageable);
        assert!(!analysis("SELECT * FROM OPENROWSET('x','y','z')").pageable);
    }

    #[test]
    fn a_write_hidden_behind_a_select_needs_confirmation_and_is_not_a_read() {
        let hidden = analysis("SELECT 1\nDELETE FROM t");
        assert!(hidden.danger.is_some(), "{hidden:?}");
        assert!(hidden.read_only_refusal.is_some());
        assert!(analysis("SELECT 1 DROP TABLE t").danger.is_some());
        // Two writes in one statement without `;`: the kit's WHERE check sees only the first.
        assert!(analysis("DELETE FROM t WHERE a = 1 DELETE FROM u").danger.is_some());
        assert!(analysis("UPDATE t SET a = 1 WHERE b = 2 DELETE FROM u").danger.is_some());
        // The ordinary forms stay quiet.
        assert!(analysis("DELETE FROM t WHERE a = 1").danger.is_none());
        assert!(analysis("UPDATE t SET a = 1 WHERE b = 2").danger.is_none());
        assert!(analysis("INSERT INTO t SELECT * FROM u").danger.is_none());
        assert!(analysis("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)").danger.is_none());
        assert!(analysis("CREATE PROCEDURE p AS DELETE FROM a WHERE x = 1 DELETE FROM b WHERE y = 1")
            .danger
            .is_none());
        // What the kit already asks about.
        assert!(analysis("DELETE FROM t").danger.is_some());
        assert!(analysis("DROP TABLE t").danger.is_some());
        assert!(analysis("TRUNCATE TABLE t").danger.is_some());
    }

    #[test]
    fn a_read_only_run_takes_reads_only() {
        assert_eq!(analysis("SELECT 1; SELECT 2").read_only_refusal, None);
        assert!(analysis("SELECT 1; DELETE FROM t WHERE a = 1").read_only_refusal.is_some());
        assert!(analysis("EXEC sp_who").read_only_refusal.is_some());
        assert!(analysis("SELECT 1 INSERT INTO t VALUES (1)").read_only_refusal.is_some());
        assert!(analysis("BEGIN TRAN SELECT 1").read_only_refusal.is_some());
        assert!(analysis("SELECT 1; COMMIT").read_only_refusal.is_some());
        assert!(analysis("SELECT * FROM OPENQUERY(ls, 'DELETE FROM x')").read_only_refusal.is_some());
        assert_eq!(analysis("SELECT 'x' AS [use], 1 -- exec").read_only_refusal, None);
    }

    #[test]
    fn use_and_drop_move_the_session_database() {
        assert!(analysis("USE other").changes_database);
        assert!(analysis("SELECT 1; USE other").changes_database);
        assert!(analysis("DROP DATABASE d").changes_database);
        assert!(!analysis("SELECT * FROM t OPTION (USE HINT('FORCE_LEGACY_CARDINALITY_ESTIMATION'))").changes_database);
    }

    #[test]
    fn go_separates_batches_only_on_a_line_of_its_own() {
        let sql = "SELECT 1\nGO\nSELECT 2\ngo 3 -- three times\n  \nSELECT 'a\nGO\nb'\nGO";
        let batches = split_batches(sql, &LEX);
        let texts: Vec<_> = batches.iter().map(|b| (sql[b.range.clone()].trim(), b.repeat)).collect();
        assert_eq!(
            texts,
            [("SELECT 1", 1), ("SELECT 2", 3), ("SELECT 'a\nGO\nb'", 1)]
        );
        // `GO` inside a statement line, a comment or a name is not a separator.
        for text in ["SELECT go FROM t", "SELECT 1 -- \nGOTO x", "SELECT [\nGO\n]", "SELECT 1 /*\nGO\n*/"] {
            assert_eq!(split_batches(text, &LEX).len(), 1, "{text}");
            assert_eq!(split_batches(text, &LEX)[0].range, 0..text.len(), "{text}");
        }
        // Empty batches vanish; a text of nothing but `GO` has none.
        assert!(split_batches("GO\nGO\n", &LEX).is_empty());
        assert_eq!(split_batches("-- c\nGO\nSELECT 1", &LEX).len(), 1);
    }

    #[test]
    fn several_batches_are_never_pageable_but_each_is_checked() {
        assert!(!analysis("SELECT 1\nGO\nSELECT 2").pageable);
        assert!(!analysis("SELECT 1\nGO 2").pageable);
        assert!(analysis("SELECT 1\nGO\nDROP TABLE t").danger.is_some());
        assert!(analysis("SELECT 1\nGO").pageable);
    }

    #[test]
    fn offset_fetch_is_appended_after_a_top_level_order_by_only() {
        let page = |sql: &str| with_offset_fetch(sql, &LEX, 40, 20);
        assert_eq!(
            page("SELECT a FROM t ORDER BY a").as_deref(),
            Some("SELECT a FROM t ORDER BY a\nOFFSET 40 ROWS FETCH NEXT 21 ROWS ONLY")
        );
        assert!(page("WITH c AS (SELECT 1 a) SELECT a FROM c ORDER BY a DESC").is_some());
        assert!(page("SELECT a FROM t UNION ALL SELECT a FROM u ORDER BY 1").is_some());
        // The `ORDER BY` of a window or a subquery is not the statement's.
        assert_eq!(page("SELECT ROW_NUMBER() OVER (ORDER BY a) FROM t"), None);
        assert_eq!(page("SELECT a FROM (SELECT TOP 5 a FROM t ORDER BY a) x"), None);
        assert_eq!(page("SELECT a FROM t"), None);
        // Clauses that cannot be followed by `OFFSET`.
        for sql in [
            "SELECT TOP 5 a FROM t ORDER BY a",
            "SELECT a FROM t ORDER BY a OFFSET 5 ROWS",
            "SELECT a FROM t ORDER BY a OPTION (RECOMPILE)",
            "SELECT a FROM t ORDER BY a FOR XML AUTO",
            "SELECT a INTO #x FROM t ORDER BY a",
            "EXEC p",
        ] {
            assert_eq!(page(sql), None, "{sql}");
        }
        // A trailing line comment does not swallow the clause.
        assert!(page("SELECT a FROM t ORDER BY a -- last").unwrap().contains("\nOFFSET 40 ROWS"));
        // An absurd page size does not wrap.
        assert!(with_offset_fetch("SELECT a FROM t ORDER BY a", &LEX, 0, u64::MAX).is_some());
    }
}
