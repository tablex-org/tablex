//! Splitting a SQL script into statements for "Run current statement". A statement
//! ends at a top-level `;`, found by the driver kit's lexer for the connection's
//! dialect, so quotes, `$$` bodies and comments (a `--` comment ends at `\r` on
//! PostgreSQL only) are read the way the server reads them. Ranges are byte ranges
//! of the trimmed statements that hold at least one token (the `;` is not part of the
//! range). A script too large to hold is read the same way by [`ScriptReader`].

mod stream;

pub use stream::{Item, Piece, ScriptError, ScriptReader, Syntax, CHUNK};

use engine_driver_kit::sql::SqlLexOptions;
use std::ops::Range;

/// The statements of `text`. The same cutting as [`ScriptReader`] does over a file (one [`stream::Splitter`]),
/// with the text held whole.
pub fn statement_ranges(text: &str, lex: &SqlLexOptions) -> Vec<Range<usize>> {
    let mut splitter = stream::Splitter::new(*lex, Syntax::default());
    let mut ranges = Vec::new();
    // The whole text is the last piece: the splitter never has to ask for more.
    while let stream::Step::Piece(piece) = splitter.step(text.as_bytes(), true) {
        ranges.push(piece.range());
    }
    ranges
}

/// The statement the cursor is in (or the closest one before it, else the
/// first one).
pub fn statement_at(text: &str, cursor: usize, lex: &SqlLexOptions) -> Option<Range<usize>> {
    let ranges = statement_ranges(text, lex);
    ranges
        .iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
        .or_else(|| ranges.iter().rev().find(|range| range.end <= cursor))
        .or_else(|| ranges.first())
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements_on<'a>(text: &'a str, lex: &SqlLexOptions) -> Vec<&'a str> {
        statement_ranges(text, lex).into_iter().map(|range| &text[range]).collect()
    }

    fn statements(text: &str) -> Vec<&str> {
        statements_on(text, &SqlLexOptions::postgres())
    }

    #[test]
    fn splits_on_top_level_semicolons_only() {
        assert_eq!(statements("select 1; select 2;\n\nselect 3"), ["select 1", "select 2", "select 3"]);
        assert_eq!(
            statements("select ';' as a, \";\" as b; select 'it''s;ok'"),
            ["select ';' as a, \";\" as b", "select 'it''s;ok'"]
        );
        assert_eq!(
            statements_on("select ';' as a, `;` as c; select 2", &SqlLexOptions::mysql()),
            ["select ';' as a, `;` as c", "select 2"]
        );
        assert_eq!(
            statements("-- a; b\nselect 1; /* x; y */ select 2"),
            ["-- a; b\nselect 1", "/* x; y */ select 2"]
        );
        assert_eq!(
            statements("create function f() returns int as $$ begin return 1; end $$ language plpgsql; select 2"),
            ["create function f() returns int as $$ begin return 1; end $$ language plpgsql", "select 2"]
        );
        assert_eq!(statements("select $t$ a; b $t$; select 2"), ["select $t$ a; b $t$", "select 2"]);
    }

    #[test]
    fn a_sqlite_trigger_is_one_statement() {
        let lex = SqlLexOptions::sqlite();
        let text = "create table t(a); CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN update t set a = case when 1 then 2 end; delete from t; END; select 1";
        assert_eq!(
            statements_on(text, &lex),
            [
                "create table t(a)",
                "CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN update t set a = case when 1 then 2 end; delete from t; END",
                "select 1"
            ]
        );
        // Another dialect does not read a trigger this way, and a table named like one is no trigger.
        assert_eq!(statements_on("create trigger x begin select 1; end", &SqlLexOptions::postgres()).len(), 2);
        assert_eq!(statements_on("create table trigger_t(a); select 1", &lex), ["create table trigger_t(a)", "select 1"]);
    }

    #[test]
    fn empty_and_comment_only_pieces_are_dropped() {
        assert!(statements("  ;; \n ; -- nothing\n").is_empty());
        assert_eq!(statements("select 1;;select 2"), ["select 1", "select 2"]);
    }

    #[test]
    fn a_carriage_return_ends_a_line_comment_on_postgres_only() {
        // PostgreSQL ends the comment at `\r`, so the `;` after it splits; the others run the
        // comment to `\n`, so the whole text is one statement there.
        let text = "SELECT 2 -- x\r; DELETE FROM t WHERE id = 1";
        assert_eq!(statements(text), ["SELECT 2 -- x", "DELETE FROM t WHERE id = 1"]);
        for lex in [SqlLexOptions::sqlite(), SqlLexOptions::mysql(), SqlLexOptions::clickhouse()] {
            assert_eq!(statements_on(text, &lex), [text]);
        }
    }

    #[test]
    fn the_cursor_picks_its_statement_or_the_previous_one() {
        let lex = SqlLexOptions::postgres();
        let text = "select 1;\nselect 2;\n\n";
        assert_eq!(&text[statement_at(text, 3, &lex).unwrap()], "select 1");
        assert_eq!(&text[statement_at(text, 12, &lex).unwrap()], "select 2");
        // After the last `;` the closest earlier statement wins.
        assert_eq!(&text[statement_at(text, text.len(), &lex).unwrap()], "select 2");
        assert!(statement_at("  ", 0, &lex).is_none());
    }

    #[test]
    fn the_statement_at_the_cursor_follows_a_carriage_return_comment() {
        let lex = SqlLexOptions::postgres();
        let text = "-- c\rDELETE FROM t WHERE x = 1;\nSELECT 2";
        let cursor = text.len();
        assert_eq!(&text[statement_at(text, cursor, &lex).unwrap()], "SELECT 2");
        // On the first statement the comment belongs to the DELETE, as PostgreSQL runs it.
        assert_eq!(&text[statement_at(text, 8, &lex).unwrap()], "-- c\rDELETE FROM t WHERE x = 1");
    }
}
