//! The real `tablex-sqruff`: its executable through the host runner (hello, request, answer), the
//! layout it produces per dialect, and the property the whole tool rests on: with this
//! configuration sqruff changes whitespace and keyword case and nothing else. The property is
//! checked with the app's own guard over a corpus that includes the cases that bit the built-in
//! formatter.

use engine_driver_api::sql_format::{Dialect, FailureKind, FormatRequest, KeywordCase, PROTOCOL_VERSION};
use engine_driver_host::{FormatCancel, FormatHostError, SqlFormatTool};
use engine_driver_kit::sql::SqlLexOptions;
use engine_driver_kit::sql_guard::{verify, GuardViolation};
use std::time::Duration;

fn request(sql: &str, dialect: Dialect, keyword_case: KeywordCase) -> FormatRequest {
    FormatRequest {
        protocol: PROTOCOL_VERSION,
        sql: sql.into(),
        dialect,
        keyword_case,
        indent: 2,
    }
}

fn lex(dialect: Dialect) -> SqlLexOptions {
    match dialect {
        Dialect::Postgres | Dialect::Ansi => SqlLexOptions::postgres(),
        Dialect::Mysql => SqlLexOptions::mysql(),
        Dialect::Mariadb => SqlLexOptions::mariadb(),
        Dialect::Sqlite => SqlLexOptions::sqlite(),
        Dialect::Clickhouse => SqlLexOptions::clickhouse(),
    }
}

fn tool() -> SqlFormatTool {
    SqlFormatTool::new(env!("CARGO_BIN_EXE_tablex-sqruff"))
}

fn run(sql: &str, dialect: Dialect, case: KeywordCase) -> Result<String, FormatHostError> {
    tool().format(request(sql, dialect, case), Duration::from_secs(60), &FormatCancel::new())
}

#[test]
fn the_executable_formats_through_the_host_runner() {
    let formatted = run("select a,b from t where x=1 and y='k' order by a", Dialect::Postgres, KeywordCase::Upper).unwrap();
    assert_eq!(formatted.trim(), "SELECT\n  a,\n  b\nFROM t\nWHERE x = 1 AND y = 'k'\nORDER BY a");
    let lower = run("SELECT A FROM T WHERE X=1", Dialect::Mysql, KeywordCase::Lower).unwrap();
    assert_eq!(lower.trim(), "select A from T\nwhere X = 1");
    let kept = run("Select a From t", Dialect::Sqlite, KeywordCase::Keep).unwrap();
    assert_eq!(kept.trim(), "Select a From t");
}

#[test]
fn it_says_hello_and_exits_when_stdin_is_closed() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tablex-sqruff"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(&output.stdout[4..], br#"{"hello":{"protocol":1}}"#);
}

#[test]
fn text_that_does_not_parse_is_a_typed_failure_not_a_guess() {
    match run("select from where ((", Dialect::Postgres, KeywordCase::Upper) {
        Err(FormatHostError::Failed { kind: FailureKind::Parse, message }) => assert!(message.starts_with("line 1"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn indent_width_is_the_requests() {
    let mut four = request("select a, b from t", Dialect::Postgres, KeywordCase::Upper);
    four.indent = 4;
    let text = tool().format(four, Duration::from_secs(60), &FormatCancel::new()).unwrap();
    assert_eq!(text.trim(), "SELECT\n    a,\n    b\nFROM t");
}

#[test]
fn every_dialect_formats_a_statement_of_its_own() {
    let cases = [
        (Dialect::Ansi, "select * from t where a is not null"),
        (Dialect::Postgres, "select a::int, b->>'k' from t where c = any($1)"),
        (Dialect::Mysql, "select `a`, count(*) from `db`.`t` where b <=> 1 limit 5"),
        (Dialect::Mariadb, "select `a` from `t` where b regexp '^x' order by 1"),
        (Dialect::Sqlite, "select a from t where b glob 'x*' order by a limit 1"),
        (Dialect::Clickhouse, "select toDate(ts) as d, count() from events where d > today() - 7 group by d"),
    ];
    for (dialect, sql) in cases {
        let text = run(sql, dialect, KeywordCase::Upper).unwrap_or_else(|error| panic!("{dialect:?}: {error}"));
        verify(sql, text.trim(), &lex(dialect)).unwrap_or_else(|violation| panic!("{dialect:?}: {violation}\n{text}"));
        assert!(text.contains("SELECT"), "{dialect:?}: {text}");
    }
}

/// Statements that changed meaning under other formatters, or that stress the guard.
const CORPUS: &[&str] = &[
    "select a,b from t where x=1 and y='k' order by a",
    "select action, key, name, value, type from audit where action = 'x'",
    "create table c (id int primary key, p int references p (id) on delete set null on update cascade)",
    "create table c (id int, constraint fk foreign key (id) references p (id) on delete no action)",
    "select \"Order Id\", \"a b\" from \"My Table\" where \"X\" = 1",
    "select a<>b, a!=b, a<=b, a>=b, a||b from t",
    "select @a := 1, @@version, :id, $1, ?",
    "select count (*), max (a), coalesce(a,b) from t group by 1 having count(*)>1",
    "select a from t -- trailing\nwhere b = 1 /* inline */ and c = 2",
    "-- leading\nselect 1; -- after\nselect 2",
    "select * from a join b using (id) left join c on c.id=b.id and c.k = 'v'",
    "with recent as (select * from events where at>now()-interval '1 day') select kind, count(*) from recent group by kind",
    "insert into audit(action, at, payload) values ('login', now(), '{\"user\": 1}'), ('logout', now(), null)",
    "update accounts set balance=balance-10, updated_at=now() where id=1 and balance>=10",
    "delete from t where id in (select id from u where flag is not true)",
    "select case when a then 1 when b then 2 else 3 end as x from t",
    "select a.b.c, t.* from s.t",
    "select 1 union all select 2 union select 3",
    "select NULL, TRUE, False, null is null from t",
    "select 'it''s', 'tab\there' from t",
    "select a from t where b between 1 and 2 and c like 'x%' escape '\\'",
    "select cast(a as int), a::text from t",
    "select * from t order by a desc nulls last limit 10 offset 5",
    "select\t a ,\n\n\n b\nfrom   t",
];

const POSTGRES_ONLY: &[&str] = &[
    "create function f() returns int as $$ begin return 1; end $$ language plpgsql",
    "create function g() returns text as $body$ select 'a  b' $body$ language sql",
    "select a->'b'->>'c', a#>>'{x,y}', a @> '{}'::jsonb from t",
    "select * from t where a = any(array[1,2,3]) and b ~* 'x'",
    "insert into t (a) values (1) on conflict (a) do update set a = excluded.a returning *",
    "select \"user\", \"Action\" from \"Audit\"",
];

const MYSQL_ONLY: &[&str] = &[
    "select `Order Id`, \"a b\", 'x' from `My Table` where `key` = 1",
    "select `a` from `t` where b <=> 1 /*! and c = 2 */",
    "insert into t (a,b) values (1,2) on duplicate key update b = values(b)",
    "select a from t force index (idx) where b = 1 limit 1",
];

fn corpus_for(dialect: Dialect) -> Vec<&'static str> {
    let mut all: Vec<&'static str> = CORPUS.to_vec();
    match dialect {
        Dialect::Postgres => all.extend(POSTGRES_ONLY),
        Dialect::Mysql | Dialect::Mariadb => all.extend(MYSQL_ONLY),
        _ => {}
    }
    all
}

/// Prefixed literals: sqruff's lexer does not know every prefix of every dialect and may put a space
/// between the prefix and the quote. Not a layout change, a different token pair: the guard has to
/// catch it (the app then asks the user), and the tool's answer must otherwise be the same text.
const PREFIXED_LITERALS: &[&str] = &["select N'x', E'a\\nb', x'ff', b'101' from t"];

/// What the guard may say about sqruff's output without the tool having changed a token: a space it
/// adds where the dialect reads one differently (`audit (a)`, `VALUES (b)`) or at an operator the
/// guard does not know (`#>>`), and text the guard never formats (`/*! ... */`).
fn is_a_layout_refusal(violation: &GuardViolation) -> bool {
    matches!(violation, GuardViolation::Spacing { .. } | GuardViolation::ExecutableComment)
}

const DIALECTS: [Dialect; 6] =
    [Dialect::Postgres, Dialect::Mysql, Dialect::Mariadb, Dialect::Sqlite, Dialect::Clickhouse, Dialect::Ansi];
const CASES: [KeywordCase; 3] = [KeywordCase::Upper, KeywordCase::Lower, KeywordCase::Keep];

/// Runs `run` on every (dialect, case, statement) of the corpus on a few threads.
fn over_the_corpus<R: Send>(statements: impl Fn(Dialect) -> Vec<&'static str>, check: impl Fn(Dialect, KeywordCase, &'static str) -> R + Sync) -> Vec<R> {
    let work: Vec<_> = DIALECTS
        .into_iter()
        .flat_map(|dialect| CASES.into_iter().flat_map(move |case| [(dialect, case)]))
        .flat_map(|(dialect, case)| statements(dialect).into_iter().map(move |sql| (dialect, case, sql)))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..6 {
            scope.spawn(|| loop {
                let at = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(&(dialect, case, sql)) = work.get(at) else { return };
                let result = check(dialect, case, sql);
                results.lock().unwrap().push(result);
            });
        }
    });
    results.into_inner().unwrap()
}

enum Outcome {
    Clean,
    Layout(String),
    Refused(String),
}

#[test]
fn the_configuration_never_changes_what_a_statement_says() {
    let outcomes = over_the_corpus(corpus_for, |dialect, case, sql| match run(sql, dialect, case) {
        Ok(text) => match verify(sql, text.trim(), &lex(dialect)) {
            Ok(_) => Outcome::Clean,
            // Never a changed, added or lost token, and never a moved comment.
            Err(violation) if is_a_layout_refusal(&violation) => Outcome::Layout(format!("{dialect:?}: {sql}")),
            Err(violation) => panic!("{dialect:?}/{case:?}: {violation:?}\n--- input\n{sql}\n--- output\n{text}"),
        },
        // Text sqruff cannot parse in that dialect is refused, never rewritten.
        Err(FormatHostError::Failed { kind: FailureKind::Parse, .. }) => Outcome::Refused(format!("{dialect:?}: {sql}")),
        Err(other) => panic!("{dialect:?}/{case:?}: {sql}: {other}"),
    });
    let clean = outcomes.iter().filter(|outcome| matches!(outcome, Outcome::Clean)).count();
    let mut layout: Vec<_> = outcomes.iter().filter_map(|o| if let Outcome::Layout(s) = o { Some(s.clone()) } else { None }).collect();
    let mut refused: Vec<_> = outcomes.iter().filter_map(|o| if let Outcome::Refused(s) = o { Some(s.clone()) } else { None }).collect();
    layout.sort();
    layout.dedup();
    refused.sort();
    refused.dedup();
    eprintln!("{clean} outputs passed the guard unchanged");
    eprintln!("the guard refuses the spacing of {} statements:\n{}", layout.len(), layout.join("\n"));
    eprintln!("sqruff cannot parse {}:\n{}", refused.len(), refused.join("\n"));
    assert!(clean > 250, "only {clean} outputs passed the guard unchanged");
    assert!(layout.len() <= 14, "{} statements need a decision from the user: {layout:?}", layout.len());
}

#[test]
fn a_prefixed_literal_that_sqruff_splits_is_caught_by_the_guard() {
    let mut caught = 0;
    for dialect in [Dialect::Postgres, Dialect::Mysql, Dialect::Ansi] {
        for sql in PREFIXED_LITERALS {
            let Ok(text) = run(sql, dialect, KeywordCase::Upper) else { continue };
            // Same words with the blanks squeezed out: only blanks differ, and the guard says so.
            let squeeze = |text: &str| text.split_whitespace().collect::<String>().to_lowercase();
            assert_eq!(squeeze(sql), squeeze(&text), "{dialect:?}: {text}");
            if verify(sql, text.trim(), &lex(dialect)).is_err() {
                caught += 1;
            }
        }
    }
    // `N 'x'` (MySQL and PostgreSQL read it differently) is among them.
    assert!(caught >= 1, "the splitting was not seen: the corpus no longer shows the guard's job");
}
