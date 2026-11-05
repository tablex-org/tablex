use engine_plan::{explain_sql as explain_with, ExplainDialect as D, ExplainError as E, ExplainMode as M, ExplainStatement, PlanFormat, SqlLexOptions};

/// `explain_with` using the dialect's default lexer options.
fn explain_sql(d: D, m: M, s: &str, read_only: bool) -> Result<ExplainStatement, E> {
    explain_with(d, m, s, read_only, &d.default_lex_options())
}

fn sql(d: D, m: M, s: &str) -> String {
    explain_sql(d, m, s, true).unwrap().sql
}

fn err(d: D, m: M, s: &str, read_only: bool) -> E {
    explain_sql(d, m, s, read_only).unwrap_err()
}

#[test]
fn postgres_statements() {
    let est = explain_sql(D::Postgres, M::Estimate, "select 1", true).unwrap();
    assert_eq!(est.sql, "EXPLAIN (FORMAT JSON) select 1");
    assert_eq!(est.format, PlanFormat::Json);
    let ana = explain_sql(D::Postgres, M::Analyze, "select 1", true).unwrap();
    assert_eq!(ana.sql, "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) select 1");
    assert_eq!(ana.format, PlanFormat::Json);
}

#[test]
fn mysql_statements() {
    let est = explain_sql(D::MySql, M::Estimate, "select 1", true).unwrap();
    assert_eq!(est.sql, "EXPLAIN FORMAT=JSON select 1");
    assert_eq!(est.format, PlanFormat::Json);
    let ana = explain_sql(D::MySql, M::Analyze, "select 1", true).unwrap();
    assert_eq!(ana.sql, "EXPLAIN ANALYZE select 1");
    assert_eq!(ana.format, PlanFormat::Tree);
}

#[test]
fn mariadb_statements() {
    let est = explain_sql(D::MariaDb, M::Estimate, "select 1", true).unwrap();
    assert_eq!(est.sql, "EXPLAIN FORMAT=JSON select 1");
    let ana = explain_sql(D::MariaDb, M::Analyze, "select 1", true).unwrap();
    assert_eq!(ana.sql, "ANALYZE FORMAT=JSON select 1");
    assert_eq!(ana.format, PlanFormat::Json);
}

#[test]
fn sqlite_and_clickhouse_statements() {
    let lite = explain_sql(D::Sqlite, M::Estimate, "select 1", true).unwrap();
    assert_eq!(lite.sql, "EXPLAIN QUERY PLAN select 1");
    assert_eq!(lite.format, PlanFormat::Rows);
    let ch = explain_sql(D::ClickHouse, M::Estimate, "select 1", true).unwrap();
    assert_eq!(ch.sql, "EXPLAIN json = 1, actions = 1, indexes = 1 select 1");
    assert_eq!(ch.format, PlanFormat::Json);
}

#[test]
fn estimate_does_not_need_read_only() {
    let s = explain_sql(D::Postgres, M::Estimate, "delete from t", false).unwrap();
    assert_eq!(s.sql, "EXPLAIN (FORMAT JSON) delete from t");
}

#[test]
fn trailing_semicolon_and_whitespace_are_trimmed() {
    for s in ["select 1;", "select 1 ;  \n", "select 1;;", "  select 1  ", "select 1; -- done", "select 1; /* y */"] {
        assert_eq!(sql(D::Postgres, M::Estimate, s), "EXPLAIN (FORMAT JSON) select 1", "{s:?}");
    }
}

#[test]
fn comments_around_the_statement_are_harmless() {
    // The classifier's span keeps comments next to the statement; they stay inert.
    assert_eq!(sql(D::Postgres, M::Estimate, "/* lead */ select 1;"), "EXPLAIN (FORMAT JSON) /* lead */ select 1");
    assert_eq!(sql(D::Postgres, M::Estimate, "-- lead\nselect 1"), "EXPLAIN (FORMAT JSON) -- lead\nselect 1");
}

#[test]
fn interior_comments_are_kept() {
    assert_eq!(
        sql(D::Postgres, M::Estimate, "select /* c */ 1 -- tail\n, 2;"),
        "EXPLAIN (FORMAT JSON) select /* c */ 1 -- tail\n, 2"
    );
}

#[test]
fn refuses_empty() {
    for s in ["", "   ", ";", " ; ; ", "-- only a comment", "/* x */", "/* x */ ; -- y"] {
        assert_eq!(err(D::Postgres, M::Estimate, s, true), E::EmptyStatement, "{s:?}");
    }
}

#[test]
fn refuses_several_statements() {
    for s in ["select 1; select 2", "select 1;\nselect 2;", "select 1; /* c */ select 2", "select 1;-- c\nselect 2"] {
        assert_eq!(err(D::Postgres, M::Estimate, s, true), E::MultipleStatements, "{s:?}");
    }
}

#[test]
fn refuses_already_explain() {
    for (s, kw) in [
        ("explain select 1", "EXPLAIN"),
        ("  EXPLAIN (ANALYZE) select 1", "EXPLAIN"),
        ("-- c\nanalyze t", "ANALYZE"),
        ("Describe t", "DESCRIBE"),
        ("desc t", "DESC"),
        ("/* c */ ExPlAiN select 1", "EXPLAIN"),
    ] {
        assert_eq!(err(D::MySql, M::Estimate, s, true), E::AlreadyExplain(kw.into()), "{s:?}");
    }
}

#[test]
fn explain_like_identifiers_are_not_refused() {
    for s in ["select 1", "with explain as (select 1) select * from explain", "(select 1) union (select 2)"] {
        assert!(explain_sql(D::Postgres, M::Estimate, s, true).is_ok(), "{s:?}");
    }
    // A word that only starts like a keyword.
    assert!(explain_sql(D::Postgres, M::Estimate, "explained_view_refresh()", true).is_ok());
}

#[test]
fn analyze_needs_read_only() {
    for d in [D::Postgres, D::MySql, D::MariaDb] {
        assert_eq!(err(d, M::Analyze, "delete from t", false), E::AnalyzeRequiresReadOnly);
        assert!(explain_sql(d, M::Analyze, "select 1", true).is_ok());
    }
}

#[test]
fn analyze_is_refused_for_sqlite_and_clickhouse() {
    assert_eq!(err(D::Sqlite, M::Analyze, "select 1", true), E::AnalyzeUnsupported(D::Sqlite));
    assert_eq!(err(D::ClickHouse, M::Analyze, "select 1", true), E::AnalyzeUnsupported(D::ClickHouse));
    assert_eq!(err(D::Sqlite, M::Analyze, "select 1", false), E::AnalyzeUnsupported(D::Sqlite));
}

#[test]
fn error_messages_are_readable() {
    assert_eq!(E::AnalyzeUnsupported(D::Sqlite).to_string(), "SQLite has no EXPLAIN ANALYZE");
    assert_eq!(E::AlreadyExplain("EXPLAIN".into()).to_string(), "the statement already starts with EXPLAIN");
}

// ---- scanner: semicolons inside literals, comments, quoted identifiers -------------------------

fn single(d: D, s: &str) {
    let out = explain_sql(d, M::Estimate, s, true).unwrap_or_else(|e| panic!("{s:?}: {e}"));
    assert!(out.sql.ends_with(s.trim().trim_end_matches(';').trim_end()), "{s:?} -> {}", out.sql);
}

#[test]
fn semicolon_in_string_literals() {
    for d in [D::Postgres, D::MySql, D::MariaDb, D::Sqlite, D::ClickHouse] {
        single(d, "select 'a;b'");
        single(d, "select 'it''s; fine'");
        single(d, "select \"a;b\" from t");
        if d != D::Postgres {
            single(d, "select `a;b` from t;");
        }
    }
}

#[test]
fn semicolon_in_comments() {
    for d in [D::Postgres, D::MySql, D::MariaDb, D::Sqlite, D::ClickHouse] {
        single(d, "select 1 -- a; b\n, 2");
        single(d, "select 1 /* a; b */, 2");
    }
}

#[test]
fn postgres_dollar_quotes_and_nested_comments() {
    single(D::Postgres, "select $$a;b$$");
    single(D::Postgres, "select $tag$ a; 'b ; $$ c $tag$, 2");
    single(D::Postgres, "select $_t1$;$_t1$");
    single(D::Postgres, "select /* a /* b; */ c; */ 1");
    // `$1` is a parameter, not a dollar quote.
    assert_eq!(err(D::Postgres, M::Estimate, "select $1; select $2", true), E::MultipleStatements);
    // Unterminated dollar quote swallows the rest: still one statement.
    single(D::Postgres, "select $x$ a; b");
}

#[test]
fn postgres_e_strings_use_backslash_escapes_but_plain_strings_do_not() {
    single(D::Postgres, r"select E'it\'s; here'");
    // Plain string: the backslash does not escape, so the quote closes and `;` splits.
    assert_eq!(err(D::Postgres, M::Estimate, r"select 'a\'; select 2", true), E::MultipleStatements);
    // Identifier ending in e is not an E-string prefix.
    assert_eq!(err(D::Postgres, M::Estimate, r"select name'\'; select 1", true), E::MultipleStatements);
}

#[test]
fn mysql_backslash_escapes() {
    for d in [D::MySql, D::MariaDb] {
        single(d, r"select 'it\'s; fine'");
        single(d, r#"select "say \"hi; there\"""#);
        assert_eq!(err(d, M::Estimate, "select 1; select 2", true), E::MultipleStatements);
    }
    // Without backslash escapes (SQLite) the same text ends the string early.
    assert_eq!(err(D::Sqlite, M::Estimate, r"select 'a\'; select 2", true), E::MultipleStatements);
}

#[test]
fn mysql_hash_and_dash_comments() {
    single(D::MySql, "select 1 # a; b\n, 2");
    // `--x` is not a comment in MySQL: it is `- -x`, so the `;` is a real separator.
    assert_eq!(err(D::MySql, M::Estimate, "select 1 --x; select 2", true), E::MultipleStatements);
    // ...but it is a comment elsewhere.
    // The rest of the line is a comment there, so it is one statement.
    assert_eq!(sql(D::Postgres, M::Estimate, "select 1 --x; select 2"), "EXPLAIN (FORMAT JSON) select 1 --x; select 2");
    // `#` is an operator in PostgreSQL.
    assert_eq!(err(D::Postgres, M::Estimate, "select 1 # 2; select 3", true), E::MultipleStatements);
}

#[test]
fn sqlite_bracket_identifiers() {
    single(D::Sqlite, "select [a;b] from t");
}

#[test]
fn clickhouse_heredoc_and_hash_comment() {
    single(D::ClickHouse, "select $$a;b$$");
    single(D::ClickHouse, "select 1 # a; b\n, 2");
}

#[test]
fn non_ascii_text_is_kept_intact() {
    let out = sql(D::Postgres, M::Estimate, "select 'héllo; wörld' as ñ; -- ünï");
    assert_eq!(out, "EXPLAIN (FORMAT JSON) select 'héllo; wörld' as ñ");
    assert_eq!(sql(D::Postgres, M::Estimate, "select 日本語;"), "EXPLAIN (FORMAT JSON) select 日本語");
}

#[test]
fn unterminated_constructs_do_not_panic() {
    for s in ["select 'abc", "select \"abc", "select `abc", "select /* abc", "select 1 -- abc", "select $a$ abc", "select 'abc\\"] {
        for d in [D::Postgres, D::MySql, D::MariaDb, D::Sqlite, D::ClickHouse] {
            let _ = explain_sql(d, M::Estimate, s, true);
        }
    }
}

// ---- executable comments, session modes -------------------------------------------------------

#[test]
fn mysql_executable_comments_are_code_not_comments() {
    for d in [D::MySql, D::MariaDb] {
        for s in [
            "SELECT 1 /*!; DELETE FROM t WHERE 1 */ = 1",
            "SELECT 1 /*!50000; DELETE FROM t WHERE 1 */ = 1",
            "SELECT 1 /*!50000 ; DELETE FROM t */",
        ] {
            assert_eq!(err(d, M::Estimate, s, true), E::MultipleStatements, "{d:?} {s:?}");
        }
    }
    assert_eq!(
        err(D::MariaDb, M::Estimate, "SELECT 1 /*M!; DELETE FROM t WHERE 1 */ = 1", true),
        E::MultipleStatements
    );
    // A plain comment with the same text is inert.
    assert!(explain_sql(D::MySql, M::Estimate, "SELECT 1 /* ; DELETE FROM t */", true).is_ok());
    // `/*M!` is only code for MariaDB; MySQL reads it as a comment.
    assert!(explain_sql(D::MySql, M::Estimate, "SELECT 1 /*M!; DELETE FROM t */", true).is_ok());
    // An executable comment without a separator is just SQL.
    assert!(explain_sql(D::MySql, M::Estimate, "SELECT /*!50000 1 */", true).is_ok());
}

#[test]
fn mysql_session_modes_change_how_quotes_are_read() {
    let run = |lex: SqlLexOptions, s: &str| explain_with(D::MySql, M::Estimate, s, true, &lex);
    let inside = r"SELECT 'it\'s; fine'";
    // Default: `\'` is an escaped quote, the `;` stays inside the string.
    assert!(run(SqlLexOptions::mysql(), inside).is_ok());
    // NO_BACKSLASH_ESCAPES: the quote after `\` closes the string, the `;` splits.
    assert_eq!(run(SqlLexOptions::mysql().without_backslash_escapes(), inside).unwrap_err(), E::MultipleStatements);
    // ANSI_QUOTES: "..." is a quoted identifier, `;` inside is safe.
    assert!(run(SqlLexOptions::mysql().with_ansi_quotes(), r#"SELECT "a;b" FROM t"#).is_ok());
    // Without ANSI_QUOTES `"` opens a string with backslash escapes; with it, `\` is plain in an identifier.
    let dq = r#"SELECT "a\"; DELETE FROM t; --""#;
    assert!(run(SqlLexOptions::mysql(), dq).is_ok());
    assert_eq!(run(SqlLexOptions::mysql().with_ansi_quotes(), dq).unwrap_err(), E::MultipleStatements);
}

#[test]
fn postgres_e_strings_and_dollar_quotes_via_the_kit() {
    assert!(explain_sql(D::Postgres, M::Estimate, r"select E'it\'s; here'", true).is_ok());
    assert_eq!(
        err(D::Postgres, M::Estimate, r"select 'it\'s; delete from t; --'", true),
        E::MultipleStatements
    );
    assert!(explain_sql(D::Postgres, M::Estimate, "select $q$ a; 'b $q$", true).is_ok());
    assert_eq!(err(D::Postgres, M::Estimate, "select $q$x$q$; delete from t", true), E::MultipleStatements);
}
