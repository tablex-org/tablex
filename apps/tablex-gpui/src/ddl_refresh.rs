//! Which statements of the SQL editor change what a schema lists (create, drop, alter, rename an
//! object), and which group of the navigator that touches. Read with the driver kit's lexer for
//! the connection's dialect, so comments, quotes and `$$` bodies are not mistaken for words.
//! Anything else (a `SELECT`, an `INSERT`, `TRUNCATE`, `COMMENT ON`, an object kind the navigator
//! has no group for) is not a change of shape and returns nothing.

use crate::navigator::ObjectGroup;
use engine_driver_kit::sql::{token_iter, SqlLexOptions, SqlToken, SqlTokenKind};
use std::collections::HashSet;

/// What a statement did to the navigator: `group` of a schema listed again. `qualifier` is the
/// schema (or MySQL database) the statement names the object in; without it the tab's own namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DdlTarget {
    pub group: ObjectGroup,
    pub qualifier: Option<String>,
}

/// Statements that change the shape of a schema (`RENAME TABLE` is MySQL's).
const LEADING: &[&str] = &["CREATE", "DROP", "ALTER", "RENAME"];

/// The word that names the kind of object, and the group that lists it. `MATERIALIZED` comes
/// before the `VIEW` after it.
const KINDS: &[(&str, ObjectGroup)] = &[
    ("TABLE", ObjectGroup::Tables),
    ("MATERIALIZED", ObjectGroup::MaterializedViews),
    ("VIEW", ObjectGroup::Views),
    ("FUNCTION", ObjectGroup::Functions),
    ("PROCEDURE", ObjectGroup::Procedures),
    ("SEQUENCE", ObjectGroup::Sequences),
    ("TRIGGER", ObjectGroup::Triggers),
    ("TYPE", ObjectGroup::Types),
    ("DOMAIN", ObjectGroup::Types),
    ("EVENT", ObjectGroup::Events),
];

/// Words between the kind and the object's name.
const BEFORE_NAME: &[&str] = &["IF", "NOT", "EXISTS", "ONLY", "CONCURRENTLY"];

/// How far into a statement its kind is looked for (`CREATE OR REPLACE TEMPORARY ... VIEW`).
const KIND_WITHIN: usize = 10;

/// The groups the statements of `sql` change, each once, in the order they first appear. Read as a
/// stream: a statement that does not begin with a [`LEADING`] word is skipped to its `;` without
/// keeping a token, so a multi-megabyte `INSERT` costs the lexer's pass and no allocation.
pub fn ddl_targets(sql: &str, lex: &SqlLexOptions) -> Vec<DdlTarget> {
    let spelled = |token: &SqlToken| &sql[token.range.clone()];
    let is_word_of = |token: &SqlToken, words: &[&str]| token.kind == SqlTokenKind::Word && words.iter().any(|word| spelled(token).eq_ignore_ascii_case(word));
    let mut tokens = token_iter(sql, lex).peekable();
    let mut found: Vec<DdlTarget> = Vec::new();
    let mut seen: HashSet<DdlTarget> = HashSet::new();
    while let Some(first) = tokens.next() {
        if first.kind == SqlTokenKind::Semicolon {
            continue;
        }
        if is_word_of(&first, LEADING) {
            // The kind: the first word of KINDS within the next KIND_WITHIN tokens, before any `(`.
            let mut group = None;
            for _ in 0..KIND_WITHIN {
                let Some(token) = tokens.next_if(|token| !matches!(token.kind, SqlTokenKind::Semicolon | SqlTokenKind::Open)) else { break };
                if token.kind == SqlTokenKind::Word {
                    group = KINDS.iter().find(|(kind, _)| spelled(&token).eq_ignore_ascii_case(kind)).map(|(_, group)| *group);
                    if group.is_some() {
                        break;
                    }
                }
            }
            if let Some(group) = group {
                // The name: `[[database.]schema.]object`; what is before the last dot is the qualifier.
                while tokens.next_if(|token| is_word_of(token, BEFORE_NAME)).is_some() {}
                let mut parts: Vec<&str> = Vec::new();
                while let Some(token) = tokens.next_if(|token| matches!(token.kind, SqlTokenKind::Word | SqlTokenKind::Quoted)) {
                    parts.push(spelled(&token).trim_matches(['"', '`', '[', ']']));
                    if tokens.next_if(|dot| dot.kind == SqlTokenKind::Other && spelled(dot) == ".").is_none() {
                        break;
                    }
                }
                let qualifier = parts.len().checked_sub(2).map(|qualifier| parts[qualifier].to_owned());
                let target = DdlTarget { group, qualifier };
                if seen.insert(target.clone()) {
                    found.push(target);
                }
            }
        }
        // The rest of the statement, through its `;`.
        for token in tokens.by_ref() {
            if token.kind == SqlTokenKind::Semicolon {
                break;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(sql: &str) -> Vec<(ObjectGroup, Option<String>)> {
        let lex = if sql.contains('`') { SqlLexOptions::mysql() } else { SqlLexOptions::postgres() };
        ddl_targets(sql, &lex).into_iter().map(|target| (target.group, target.qualifier)).collect()
    }

    #[test]
    fn object_shape_changes_name_the_group_they_touch() {
        use ObjectGroup::*;
        for (sql, expected) in [
            ("CREATE TABLE t (id int)", (Tables, None)),
            ("create table if not exists public.t (id int)", (Tables, Some("public"))),
            ("DROP TABLE IF EXISTS \"app\".\"t\"", (Tables, Some("app"))),
            ("ALTER TABLE ONLY db.app.t ADD COLUMN x int", (Tables, Some("app"))),
            ("RENAME TABLE `shop`.`a` TO `shop`.`b`", (Tables, Some("shop"))),
            ("CREATE OR REPLACE VIEW v AS SELECT 1", (Views, None)),
            ("CREATE MATERIALIZED VIEW m AS SELECT 1", (MaterializedViews, None)),
            ("CREATE OR REPLACE FUNCTION f() RETURNS int AS $$ BEGIN DROP TABLE x; END $$ LANGUAGE plpgsql", (Functions, None)),
            ("DROP PROCEDURE p", (Procedures, None)),
            ("CREATE SEQUENCE s", (Sequences, None)),
            ("CREATE TEMPORARY TABLE tmp (id int)", (Tables, None)),
        ] {
            assert_eq!(targets(sql), vec![(expected.0, expected.1.map(str::to_owned))], "{sql}");
        }
    }

    #[test]
    fn other_statements_change_no_shape() {
        for sql in [
            "SELECT * FROM t",
            "INSERT INTO t VALUES (1)",
            "TRUNCATE TABLE t",
            "COMMENT ON TABLE t IS 'x'",
            "SELECT 'CREATE TABLE t (id int)'",
            "-- CREATE TABLE t\nSELECT 1",
            "CREATE INDEX i ON t (id)",
            "",
        ] {
            assert!(targets(sql).is_empty(), "{sql}");
        }
    }

    /// A multi-megabyte statement that is not DDL names nothing (and is read without a token list).
    #[test]
    fn a_large_insert_changes_no_shape() {
        let mut insert = String::from("INSERT INTO t VALUES ");
        while insert.len() < 1 << 20 {
            insert.push_str("(1, 'CREATE TABLE x (id int)', 'a;b'),");
        }
        insert.push_str("(2, 'z');");
        assert!(targets(&insert).is_empty());
        insert.push_str(" CREATE TABLE after (id int)");
        assert_eq!(targets(&insert), vec![(ObjectGroup::Tables, None)]);
    }

    #[test]
    fn a_script_names_each_group_once() {
        let mut script = String::new();
        for n in 0..50 {
            script.push_str(&format!("CREATE TABLE t{n} (id int);\n"));
        }
        script.push_str("CREATE VIEW v AS SELECT 1;");
        assert_eq!(targets(&script), vec![(ObjectGroup::Tables, None), (ObjectGroup::Views, None)]);
    }
}
