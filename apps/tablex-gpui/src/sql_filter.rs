//! Structured filter builder of `sqlTable` tabs (the "Filter" popover).
//!
//! A builder filter is a list of rules (`column  mode  value`) that is turned
//! into one SQL `WHERE` fragment. The fragment is written into the tab's WHERE
//! field and applied like any hand-typed one, so the builder adds no query
//! path of its own: paging, refresh and the fragment validator
//! (`crate::sql_table::validate_fragment`) all stay as they are.
//!
//! Semantics:
//!
//! * rules are skipped when disabled or incomplete (empty column / value);
//! * conjunctions fold left-to-right, each rule carrying the AND/OR that joins
//!   it to the previous one (`(a) AND (b)` then `((a) AND (b)) OR (c)`); here a
//!   pair of parentheses is only emitted where the operator changes, which is
//!   the same grouping with less noise;
//! * `In` / `Not in` split the value on commas / newlines (quote aware), an
//!   unquoted `null` item adds an `IS NULL` branch;
//! * "blank" means NULL or empty string, but only for text-like columns (a
//!   numeric column cannot hold `''`, and `n = ''` is a type error in
//!   PostgreSQL), so it degrades to `IS NULL` there.
//!
//! # Literal safety
//!
//! Values are user text spliced into SQL, so every literal is rendered by
//! [`sql_string_literal`] / [`literal`]:
//!
//! * strings are `'...'` with `'` doubled. PostgreSQL and SQLite treat a
//!   backslash as an ordinary character (`standard_conforming_strings` is on
//!   by default since 9.1), so they never get backslash escapes. MySQL
//!   treats `\` as an escape inside strings, so there it is doubled too;
//! * a number is emitted bare only when the value matches a strict numeric
//!   literal (`[+-]digits[.digits][e[+-]digits]`) and the column is numeric
//!   (or its type is unknown); anything else is quoted;
//! * `LIKE` patterns escape `%`, `_` and the escape character itself with an
//!   explicit `ESCAPE '!'` (one approach for all three dialects, independent
//!   of the dialect's default LIKE escape);
//! * a NUL character is rejected (PostgreSQL cannot store it in text).

use crate::sql_table::SqlDialect;
use serde::{Deserialize, Serialize};

/// Character that escapes `%` and `_` in generated `LIKE` patterns.
const LIKE_ESCAPE: char = '!';

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqlFilterMode {
    #[default]
    Equals,
    NotEquals,
    IsNull,
    IsNotNull,
    IsBlank,
    IsNotBlank,
    Like,
    NotLike,
    BeginsWith,
    EndsWith,
    LessThan,
    LessThanOrEqual,
    GreaterThan,
    GreaterThanOrEqual,
    In,
    NotIn,
    Between,
    NotBetween,
}

impl SqlFilterMode {
    /// Menu order.
    pub const ALL: [Self; 18] = [
        Self::Equals,
        Self::NotEquals,
        Self::IsNull,
        Self::IsNotNull,
        Self::IsBlank,
        Self::IsNotBlank,
        Self::Like,
        Self::NotLike,
        Self::BeginsWith,
        Self::EndsWith,
        Self::LessThan,
        Self::LessThanOrEqual,
        Self::GreaterThan,
        Self::GreaterThanOrEqual,
        Self::In,
        Self::NotIn,
        Self::Between,
        Self::NotBetween,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Equals => "Equals",
            Self::NotEquals => "Not equals",
            Self::IsNull => "Is NULL",
            Self::IsNotNull => "Is not NULL",
            Self::IsBlank => "Is blank",
            Self::IsNotBlank => "Is not blank",
            Self::Like => "Contains (LIKE)",
            Self::NotLike => "Not contains",
            Self::BeginsWith => "Begins with",
            Self::EndsWith => "Ends with",
            Self::LessThan => "<",
            Self::LessThanOrEqual => "<=",
            Self::GreaterThan => ">",
            Self::GreaterThanOrEqual => ">=",
            Self::In => "In list",
            Self::NotIn => "Not in list",
            Self::Between => "Between",
            Self::NotBetween => "Not between",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.label() == label)
    }

    pub fn needs_value(self) -> bool {
        !matches!(
            self,
            Self::IsNull | Self::IsNotNull | Self::IsBlank | Self::IsNotBlank
        )
    }

    pub fn uses_list(self) -> bool {
        matches!(self, Self::In | Self::NotIn)
    }

    pub fn uses_range(self) -> bool {
        matches!(self, Self::Between | Self::NotBetween)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SqlFilterConjunction {
    #[default]
    And,
    Or,
}

impl SqlFilterConjunction {
    pub fn label(self) -> &'static str {
        match self {
            Self::And => "AND",
            Self::Or => "OR",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Self::And => Self::Or,
            Self::Or => Self::And,
        }
    }
}

/// How a column's values are written as literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlFilterColumnKind {
    /// No type known (column not in the result / catalog).
    Unknown,
    Text,
    Number,
    Boolean,
    Temporal,
    /// uuid, json, binary, arrays: always quoted, never blank-compared to `''`.
    Other,
}

impl SqlFilterColumnKind {
    /// Whether `col = ''` is meaningful (so "blank" includes the empty string).
    fn holds_empty_string(self) -> bool {
        matches!(self, Self::Text | Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlFilterRule {
    /// Stable per tab; keys the rule's editors.
    pub id: u64,
    pub column: String,
    pub mode: SqlFilterMode,
    /// Value, range start, or the raw comma / newline list.
    pub value: String,
    /// Range end (Between / Not between).
    pub end_value: String,
    /// Joins this rule to the previous one (ignored for the first rule).
    pub conjunction: SqlFilterConjunction,
    pub disabled: bool,
}

impl Default for SqlFilterRule {
    fn default() -> Self {
        Self {
            id: 0,
            column: String::new(),
            mode: SqlFilterMode::Equals,
            value: String::new(),
            end_value: String::new(),
            conjunction: SqlFilterConjunction::And,
            disabled: false,
        }
    }
}

impl SqlFilterRule {
    /// Whether the rule contributes a condition.
    pub fn is_complete(&self) -> bool {
        if self.column.is_empty() {
            return false;
        }
        if !self.mode.needs_value() {
            return true;
        }
        if self.mode.uses_list() {
            return !split_list(&self.value).is_empty();
        }
        if self.mode.uses_range() {
            return !self.value.trim().is_empty() && !self.end_value.trim().is_empty();
        }
        !self.value.trim().is_empty()
    }

    pub fn is_active(&self) -> bool {
        !self.disabled && self.is_complete()
    }
}

/// Per-tab builder state. Not serialized as a unit: the session stores `rules`
/// and `generated_where` as two plain fields.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SqlFilterState {
    pub rules: Vec<SqlFilterRule>,
    /// The WHERE text the builder last wrote (`Apply`). `None` before the first
    /// Apply and after the WHERE field was cleared; used to detect a manual edit.
    pub generated_where: Option<String>,
}

impl SqlFilterState {
    pub fn from_saved(rules: Vec<SqlFilterRule>, generated_where: Option<String>) -> Self {
        let mut state = Self {
            rules,
            generated_where,
        };
        // Old / hand-edited files: guarantee unique non-zero ids.
        let mut seen = std::collections::HashSet::new();
        let mut next = state.next_id();
        for rule in &mut state.rules {
            if rule.id == 0 || !seen.insert(rule.id) {
                rule.id = next;
                next += 1;
                seen.insert(rule.id);
            }
        }
        state
    }

    fn next_id(&self) -> u64 {
        self.rules.iter().map(|rule| rule.id).max().unwrap_or(0) + 1
    }

    /// Appends a blank rule on `column`; returns its id.
    pub fn add_rule(&mut self, column: impl Into<String>) -> u64 {
        let id = self.next_id();
        self.rules.push(SqlFilterRule {
            id,
            column: column.into(),
            ..Default::default()
        });
        id
    }

    pub fn ensure_rule(&mut self, column: impl Into<String>) {
        if self.rules.is_empty() {
            self.add_rule(column);
        }
    }

    pub fn rule_mut(&mut self, id: u64) -> Option<&mut SqlFilterRule> {
        self.rules.iter_mut().find(|rule| rule.id == id)
    }

    pub fn remove_rule(&mut self, id: u64) {
        self.rules.retain(|rule| rule.id != id);
    }

    /// Moves rule `id` so it sits at `target_index` (clamped) of the list.
    pub fn move_rule(&mut self, id: u64, target_index: usize) -> bool {
        let Some(from) = self.rules.iter().position(|rule| rule.id == id) else {
            return false;
        };
        let to = target_index.min(self.rules.len() - 1);
        if from == to {
            return false;
        }
        let rule = self.rules.remove(from);
        self.rules.insert(to, rule);
        true
    }

    /// Rules that contribute to the generated WHERE.
    pub fn active_count(&self) -> usize {
        self.rules.iter().filter(|rule| rule.is_active()).count()
    }
}

/// Splits a raw list value on commas and newlines, honouring `'..'` / `".."`
/// quoting. Items are trimmed, empty items dropped; the flag says the item was
/// quoted (a quoted `null` is the text "null", not NULL).
fn split_list(raw: &str) -> Vec<(String, bool)> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if let Some(open) = quote {
            current.push(ch);
            if ch == open {
                if chars.peek() == Some(&open) {
                    current.push(open);
                    chars.next();
                } else {
                    quote = None;
                }
            }
            continue;
        }
        match ch {
            '\'' | '"' if current.trim().is_empty() => {
                quote = Some(ch);
                current.push(ch);
            }
            ',' | '\n' | '\r' => items.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    items.push(current);
    items
        .into_iter()
        .filter_map(|token| {
            let text = token.trim();
            if text.is_empty() {
                return None;
            }
            Some(match unquote(text) {
                Some(unquoted) => (unquoted, true),
                None => (text.to_string(), false),
            })
        })
        .collect()
}

/// The text inside one pair of matching `'..'` / `".."` quotes, with each
/// doubled quote un-doubled (`'O''Brien'` is `O'Brien`); `None` when `text`
/// (already trimmed) is not quoted. The one rule behind single values and list
/// items, so the same typing gives the same literal in both.
fn unquote(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let (first, last) = (chars.next()?, text.chars().last()?);
    if text.len() >= 2 && (first == '\'' || first == '"') && first == last {
        let doubled = format!("{first}{first}");
        Some(text[1..text.len() - 1].replace(&doubled, &first.to_string()))
    } else {
        None
    }
}

/// `text` without one pair of matching surrounding quotes (see [`unquote`]).
fn unwrap_quotes(text: &str) -> String {
    let text = text.trim();
    unquote(text).unwrap_or_else(|| text.to_string())
}

/// `[+-]digits[.digits][(e|E)[+-]digits]` (at least one digit before the
/// exponent). Deliberately narrower than the SQL grammar: no `inf`, `nan`,
/// hex or underscores, so a bare emission can never be anything but a number.
pub fn is_numeric_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }
    let digits = |from: usize| bytes[from..].iter().take_while(|b| b.is_ascii_digit()).count();
    let int = digits(index);
    index += int;
    let mut frac = 0;
    if bytes.get(index) == Some(&b'.') {
        frac = digits(index + 1);
        index += 1 + frac;
    }
    if int + frac == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exp = digits(index);
        if exp == 0 {
            return false;
        }
        index += exp;
    }
    index == bytes.len()
}

/// `'...'` with quotes doubled (and backslashes doubled for MySQL).
pub fn sql_string_literal(dialect: SqlDialect, text: &str) -> String {
    let quote = dialect.string_quote;
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        if ch == quote {
            out.push(quote);
        } else if ch == '\\' && dialect.backslash_escapes {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push(quote);
    out
}

/// Literal for a user-typed comparison value (quotes around it are optional).
pub fn literal(dialect: SqlDialect, kind: SqlFilterColumnKind, raw: &str) -> String {
    literal_of(dialect, kind, &unwrap_quotes(raw))
}

fn literal_of(dialect: SqlDialect, kind: SqlFilterColumnKind, text: &str) -> String {
    match kind {
        SqlFilterColumnKind::Number | SqlFilterColumnKind::Unknown if is_numeric_literal(text) => {
            text.to_string()
        }
        SqlFilterColumnKind::Boolean if text.eq_ignore_ascii_case("true") => "TRUE".into(),
        SqlFilterColumnKind::Boolean if text.eq_ignore_ascii_case("false") => "FALSE".into(),
        _ => sql_string_literal(dialect, text),
    }
}

/// `%`, `_` and the escape character escaped for use inside a LIKE pattern.
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_') || ch == LIKE_ESCAPE {
            out.push(LIKE_ESCAPE);
        }
        out.push(ch);
    }
    out
}

fn like_condition(
    dialect: SqlDialect,
    kind: SqlFilterColumnKind,
    column: &str,
    negated: bool,
    prefix: &str,
    value: &str,
    suffix: &str,
) -> String {
    let subject = if dialect.like_requires_text_cast && !matches!(kind, SqlFilterColumnKind::Text) {
        // LIKE is a text operator in PostgreSQL (`integer ~~ unknown` fails).
        format!("{column}::text")
    } else {
        column.to_string()
    };
    let pattern = format!("{prefix}{}{suffix}", escape_like(&unwrap_quotes(value)));
    format!(
        "{subject} {}LIKE {} ESCAPE {}",
        if negated { "NOT " } else { "" },
        sql_string_literal(dialect, &pattern),
        sql_string_literal(dialect, &LIKE_ESCAPE.to_string()),
    )
}

/// The condition of one rule, `None` when it is not [`SqlFilterRule::is_active`].
pub fn rule_condition(
    dialect: SqlDialect,
    kind: SqlFilterColumnKind,
    rule: &SqlFilterRule,
) -> Option<String> {
    if !rule.is_active() {
        return None;
    }
    let column = dialect.quote(&rule.column);
    let col = column.as_str();
    let value = || literal(dialect, kind, &rule.value);
    let compare = |operator: &str| format!("{col} {operator} {}", value());
    Some(match rule.mode {
        SqlFilterMode::Equals => compare("="),
        SqlFilterMode::NotEquals => compare("<>"),
        SqlFilterMode::LessThan => compare("<"),
        SqlFilterMode::LessThanOrEqual => compare("<="),
        SqlFilterMode::GreaterThan => compare(">"),
        SqlFilterMode::GreaterThanOrEqual => compare(">="),
        SqlFilterMode::IsNull => format!("{col} IS NULL"),
        SqlFilterMode::IsNotNull => format!("{col} IS NOT NULL"),
        SqlFilterMode::IsBlank if kind.holds_empty_string() => {
            format!("({col} IS NULL OR {col} = '')")
        }
        SqlFilterMode::IsBlank => format!("{col} IS NULL"),
        SqlFilterMode::IsNotBlank if kind.holds_empty_string() => {
            format!("({col} IS NOT NULL AND {col} <> '')")
        }
        SqlFilterMode::IsNotBlank => format!("{col} IS NOT NULL"),
        SqlFilterMode::Like => like_condition(dialect, kind, col, false, "%", &rule.value, "%"),
        SqlFilterMode::NotLike => like_condition(dialect, kind, col, true, "%", &rule.value, "%"),
        SqlFilterMode::BeginsWith => {
            like_condition(dialect, kind, col, false, "", &rule.value, "%")
        }
        SqlFilterMode::EndsWith => like_condition(dialect, kind, col, false, "%", &rule.value, ""),
        SqlFilterMode::In | SqlFilterMode::NotIn => {
            let negated = rule.mode == SqlFilterMode::NotIn;
            let mut has_null = false;
            let mut literals: Vec<String> = Vec::new();
            for (item, quoted) in split_list(&rule.value) {
                if !quoted && item.eq_ignore_ascii_case("null") {
                    has_null = true;
                    continue;
                }
                let literal = literal_of(dialect, kind, &item);
                if !literals.contains(&literal) {
                    literals.push(literal);
                }
            }
            let list = literals.join(", ");
            match (negated, literals.is_empty(), has_null) {
                (false, false, false) => format!("{col} IN ({list})"),
                (false, false, true) => format!("({col} IS NULL OR {col} IN ({list}))"),
                (false, true, _) => format!("{col} IS NULL"),
                (true, false, false) => format!("{col} NOT IN ({list})"),
                (true, false, true) => {
                    format!("({col} IS NOT NULL AND {col} NOT IN ({list}))")
                }
                (true, true, _) => format!("{col} IS NOT NULL"),
            }
        }
        SqlFilterMode::Between | SqlFilterMode::NotBetween => format!(
            "{col} {}BETWEEN {} AND {}",
            if rule.mode == SqlFilterMode::NotBetween {
                "NOT "
            } else {
                ""
            },
            value(),
            literal(dialect, kind, &rule.end_value),
        ),
    })
}

/// WHERE fragment (no keyword) for all active rules; empty when none is.
pub fn build_where(
    dialect: SqlDialect,
    rules: &[SqlFilterRule],
    kind_of: impl Fn(&str) -> SqlFilterColumnKind,
) -> Result<String, String> {
    for rule in rules.iter().filter(|rule| rule.is_active()) {
        let has_nul = rule.value.contains('\0') || rule.end_value.contains('\0');
        if has_nul {
            return Err(format!(
                "Filter value for \"{}\" contains a NUL character",
                rule.column
            ));
        }
    }
    let conditions: Vec<(SqlFilterConjunction, String)> = rules
        .iter()
        .filter_map(|rule| {
            rule_condition(dialect, kind_of(&rule.column), rule)
                .map(|condition| (rule.conjunction, condition))
        })
        .collect();
    let multiple = conditions.len() > 1;
    let mut result = String::new();
    let mut last: Option<SqlFilterConjunction> = None;
    for (index, (conjunction, condition)) in conditions.into_iter().enumerate() {
        // `x BETWEEN a AND b AND y` reads badly; fence it when combined.
        let condition = if multiple && condition.contains(" BETWEEN ") {
            format!("({condition})")
        } else {
            condition
        };
        if index == 0 {
            result = condition;
            continue;
        }
        if last.is_some_and(|previous| previous != conjunction) {
            result = format!("({result})");
        }
        result = format!("{result} {} {condition}", conjunction.label());
        last = Some(conjunction);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    const POSTGRES: SqlDialect = SqlDialect::POSTGRES;
    const SQLITE: SqlDialect = SqlDialect::SQLITE;
    const MYSQL: SqlDialect = SqlDialect::MYSQL;
    use SqlFilterColumnKind as Kind;

    fn rule(column: &str, mode: SqlFilterMode, value: &str) -> SqlFilterRule {
        SqlFilterRule {
            id: 1,
            column: column.into(),
            mode,
            value: value.into(),
            ..Default::default()
        }
    }

    fn cond(dialect: SqlDialect, kind: Kind, column: &str, mode: SqlFilterMode, v: &str) -> String {
        rule_condition(dialect, kind, &rule(column, mode, v)).expect("complete rule")
    }

    /// Every mode against every dialect: the documented SQL table.
    #[test]
    fn every_mode_per_dialect() {
        use SqlFilterMode::*;
        // (mode, value, text-column SQL, number-column SQL) with `"c"` quoting for
        // PG/SQLite and backticks for MySQL substituted from the `Q` marker.
        let cases: [(SqlFilterMode, &str, &str, &str); 14] = [
            (Equals, "7", "Q = '7'", "Q = 7"),
            (NotEquals, "7", "Q <> '7'", "Q <> 7"),
            (LessThan, "7", "Q < '7'", "Q < 7"),
            (LessThanOrEqual, "7", "Q <= '7'", "Q <= 7"),
            (GreaterThan, "7", "Q > '7'", "Q > 7"),
            (GreaterThanOrEqual, "7", "Q >= '7'", "Q >= 7"),
            (IsNull, "", "Q IS NULL", "Q IS NULL"),
            (IsNotNull, "", "Q IS NOT NULL", "Q IS NOT NULL"),
            (IsBlank, "", "(Q IS NULL OR Q = '')", "Q IS NULL"),
            (IsNotBlank, "", "(Q IS NOT NULL AND Q <> '')", "Q IS NOT NULL"),
            (In, "1, 2,\n'x'", "Q IN ('1', '2', 'x')", "Q IN (1, 2, 'x')"),
            (NotIn, "1,2", "Q NOT IN ('1', '2')", "Q NOT IN (1, 2)"),
            (Between, "1", "Q BETWEEN '1' AND '9'", "Q BETWEEN 1 AND 9"),
            (NotBetween, "1", "Q NOT BETWEEN '1' AND '9'", "Q NOT BETWEEN 1 AND 9"),
        ];
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            let q = dialect.quote("c");
            for (mode, value, text_sql, number_sql) in cases {
                let mut r = rule("c", mode, value);
                r.end_value = "9".into();
                let text = rule_condition(dialect, Kind::Text, &r).unwrap();
                assert_eq!(text, text_sql.replace('Q', &q), "{dialect:?} {mode:?} text");
                let number = rule_condition(dialect, Kind::Number, &r).unwrap();
                assert_eq!(number, number_sql.replace('Q', &q), "{dialect:?} {mode:?} number");
            }
        }
    }

    #[test]
    fn a_quoted_single_value_and_a_quoted_list_item_are_unquoted_the_same_way() {
        use SqlFilterMode::*;
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            // `'O''Brien'` is the text O'Brien: one literal, not four quotes.
            let single = cond(dialect, Kind::Text, "n", Equals, "'O''Brien'");
            assert_eq!(single, format!("{} = 'O''Brien'", dialect.quote("n")));
            let list = cond(dialect, Kind::Text, "n", In, "'O''Brien', plain");
            assert_eq!(list, format!("{} IN ('O''Brien', 'plain')", dialect.quote("n")));
            // The double-quoted spelling un-doubles its own quote.
            let double = cond(dialect, Kind::Text, "n", Equals, r#""say ""hi""""#);
            assert_eq!(double, format!(r#"{} = 'say "hi"'"#, dialect.quote("n")));
            // LIKE reads the same text.
            let like = cond(dialect, Kind::Text, "n", BeginsWith, "'O''B'");
            assert!(like.contains("'O''B%'"), "{like}");
        }
        // Unquoted text and a lone quote are untouched.
        assert_eq!(unquote("O'Brien"), None);
        assert_eq!(unquote("'"), None);
        assert_eq!(unquote("''").as_deref(), Some(""));
    }

    #[test]
    fn a_backslash_follows_the_sessions_escape_mode() {
        use SqlFilterMode::*;
        let strict = SqlDialect {
            backslash_escapes: false,
            ..MYSQL
        };
        assert_eq!(cond(MYSQL, Kind::Text, "p", Equals, r"a\b"), r"`p` = 'a\\b'");
        assert_eq!(cond(strict, Kind::Text, "p", Equals, r"a\b"), r"`p` = 'a\b'");
        assert_eq!(sql_string_literal(MYSQL, r"a\b"), r"'a\\b'");
        assert_eq!(sql_string_literal(strict, r"a\b"), r"'a\b'");
    }

    #[test]
    fn like_modes_escape_wildcards_with_one_explicit_escape_character() {
        use SqlFilterMode::*;
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            let q = dialect.quote("name");
            assert_eq!(
                cond(dialect, Kind::Text, "name", Like, "50%_off!"),
                format!("{q} LIKE '%50!%!_off!!%' ESCAPE '!'")
            );
            assert_eq!(
                cond(dialect, Kind::Text, "name", NotLike, "a"),
                format!("{q} NOT LIKE '%a%' ESCAPE '!'")
            );
            assert_eq!(
                cond(dialect, Kind::Text, "name", BeginsWith, "ab"),
                format!("{q} LIKE 'ab%' ESCAPE '!'")
            );
            assert_eq!(
                cond(dialect, Kind::Text, "name", EndsWith, "ab"),
                format!("{q} LIKE '%ab' ESCAPE '!'")
            );
        }
        // PostgreSQL: LIKE needs text, so a non-text column is cast; others are not.
        assert_eq!(
            cond(POSTGRES, Kind::Number, "id", Like, "1"),
            "\"id\"::text LIKE '%1%' ESCAPE '!'"
        );
        assert_eq!(
            cond(SQLITE, Kind::Number, "id", Like, "1"),
            "\"id\" LIKE '%1%' ESCAPE '!'"
        );
        assert_eq!(
            cond(MYSQL, Kind::Number, "id", Like, "1"),
            "`id` LIKE '%1%' ESCAPE '!'"
        );
    }

    #[test]
    fn in_list_handles_null_quotes_duplicates_and_empty_items() {
        use SqlFilterMode::*;
        assert_eq!(
            cond(POSTGRES, Kind::Text, "s", In, "null, 'a,b', \"it's\", a, a,, "),
            "(\"s\" IS NULL OR \"s\" IN ('a,b', 'it''s', 'a'))"
        );
        assert_eq!(
            cond(POSTGRES, Kind::Text, "s", NotIn, "NULL,a"),
            "(\"s\" IS NOT NULL AND \"s\" NOT IN ('a'))"
        );
        assert_eq!(cond(POSTGRES, Kind::Text, "s", In, "null"), "\"s\" IS NULL");
        assert_eq!(cond(POSTGRES, Kind::Text, "s", NotIn, "null"), "\"s\" IS NOT NULL");
        // A quoted null is the text "null".
        assert_eq!(cond(POSTGRES, Kind::Text, "s", In, "'null'"), "\"s\" IN ('null')");
    }

    #[test]
    fn literals_follow_the_column_type() {
        use SqlFilterMode::{Equals, GreaterThan};
        assert_eq!(cond(POSTGRES, Kind::Number, "n", Equals, "-12.5e3"), "\"n\" = -12.5e3");
        // Not a strict number: quoted, never bare.
        for bad in ["1 OR 1=1", "0x10", "1_000", "inf", "NaN", "1e", ".", "--1", "1.2.3", "٣"] {
            assert_eq!(
                cond(POSTGRES, Kind::Number, "n", Equals, bad),
                format!("\"n\" = '{bad}'"),
                "{bad}"
            );
        }
        assert_eq!(cond(POSTGRES, Kind::Text, "s", Equals, "12"), "\"s\" = '12'");
        assert_eq!(cond(POSTGRES, Kind::Unknown, "s", Equals, "12"), "\"s\" = 12");
        assert_eq!(cond(POSTGRES, Kind::Unknown, "s", Equals, "ab"), "\"s\" = 'ab'");
        assert_eq!(cond(POSTGRES, Kind::Boolean, "b", Equals, "TRUE"), "\"b\" = TRUE");
        assert_eq!(cond(SQLITE, Kind::Boolean, "b", Equals, "false"), "\"b\" = FALSE");
        assert_eq!(cond(POSTGRES, Kind::Boolean, "b", Equals, "yes"), "\"b\" = 'yes'");
        assert_eq!(
            cond(POSTGRES, Kind::Temporal, "t", GreaterThan, "2024-01-01"),
            "\"t\" > '2024-01-01'"
        );
        // Surrounding matching quotes are optional.
        assert_eq!(cond(POSTGRES, Kind::Text, "s", Equals, "'abc'"), "\"s\" = 'abc'");
        assert_eq!(cond(POSTGRES, Kind::Number, "n", Equals, "'5'"), "\"n\" = 5");
        // Blank on a non-text column never compares with ''.
        assert_eq!(
            cond(POSTGRES, Kind::Temporal, "t", SqlFilterMode::IsBlank, ""),
            "\"t\" IS NULL"
        );
        assert_eq!(cond(POSTGRES, Kind::Other, "u", SqlFilterMode::IsNotBlank, ""), "\"u\" IS NOT NULL");
    }

    #[test]
    fn disabled_and_incomplete_rules_are_skipped() {
        use SqlFilterMode::*;
        let mut disabled = rule("a", Equals, "1");
        disabled.disabled = true;
        assert_eq!(rule_condition(POSTGRES, Kind::Text, &disabled), None);
        for incomplete in [
            rule("a", Equals, "  "),
            rule("", Equals, "1"),
            rule("a", In, " , ,"),
            rule("a", Between, "1"),
            SqlFilterRule {
                end_value: "2".into(),
                ..rule("a", Between, "")
            },
        ] {
            assert!(!incomplete.is_complete(), "{incomplete:?}");
            assert_eq!(rule_condition(POSTGRES, Kind::Text, &incomplete), None);
        }
        assert!(rule("a", IsNull, "").is_complete());
        let sql = build_where(POSTGRES, &[disabled, rule("b", IsNull, "")], |_| Kind::Text).unwrap();
        assert_eq!(sql, "\"b\" IS NULL");
        assert_eq!(build_where(POSTGRES, &[], |_| Kind::Text).unwrap(), "");
    }

    #[test]
    fn conjunctions_fold_left_without_redundant_parentheses() {
        use SqlFilterConjunction::{And, Or};
        use SqlFilterMode::*;
        let with = |column: &str, mode, value: &str, conjunction| SqlFilterRule {
            conjunction,
            ..rule(column, mode, value)
        };
        let build = |rules: &[SqlFilterRule]| build_where(POSTGRES, rules, |_| Kind::Text).unwrap();
        let (a, b, c) = (
            with("a", Equals, "1", And),
            with("b", Equals, "2", And),
            with("c", Equals, "3", And),
        );
        assert_eq!(build(std::slice::from_ref(&a)), "\"a\" = '1'");
        assert_eq!(
            build(&[a.clone(), b.clone(), c.clone()]),
            "\"a\" = '1' AND \"b\" = '2' AND \"c\" = '3'"
        );
        let b_or = SqlFilterRule { conjunction: Or, ..b.clone() };
        let c_or = SqlFilterRule { conjunction: Or, ..c.clone() };
        assert_eq!(
            build(&[a.clone(), b_or.clone(), c_or.clone()]),
            "\"a\" = '1' OR \"b\" = '2' OR \"c\" = '3'"
        );
        // a AND b OR c  ==  ((a) AND (b)) OR (c)
        assert_eq!(
            build(&[a.clone(), b.clone(), c_or.clone()]),
            "(\"a\" = '1' AND \"b\" = '2') OR \"c\" = '3'"
        );
        // a OR b AND c  ==  ((a) OR (b)) AND (c)
        assert_eq!(
            build(&[a.clone(), b_or, c.clone()]),
            "(\"a\" = '1' OR \"b\" = '2') AND \"c\" = '3'"
        );
        // Skipped rules do not take part in the fold (their conjunction is dropped).
        let skipped = SqlFilterRule { disabled: true, ..b };
        assert_eq!(
            build(&[a, skipped, c_or]),
            "\"a\" = '1' OR \"c\" = '3'"
        );
        // BETWEEN is fenced when combined, bare when alone.
        let mut between = rule("n", Between, "1");
        between.end_value = "2".into();
        assert_eq!(build(&[between.clone()]), "\"n\" BETWEEN '1' AND '2'");
        assert_eq!(
            build(&[between, with("b", Equals, "2", And)]),
            "(\"n\" BETWEEN '1' AND '2') AND \"b\" = '2'"
        );
    }

    /// Nasty values yield fragments that pass the table-tab validator and in
    /// which the user text stays inside one string literal.
    #[test]
    fn nasty_values_stay_inside_a_literal_and_pass_the_fragment_validator() {
        let nasty = [
            "'",
            "''",
            "O'Reilly",
            "a' OR '1'='1",
            "x'; DROP TABLE orders; --",
            "-- comment",
            "/* c */",
            ";",
            "100%",
            "snake_case",
            "back\\slash",
            "end\\",
            "\\'",
            "!escape!",
            "\"quoted\"",
            "`tick`",
            "(",
            ")",
            "日本語 ünïcödé 🎉",
            "line1 line2",
            "tab\there",
        ];
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            for kind in [Kind::Text, Kind::Number, Kind::Unknown, Kind::Boolean, Kind::Temporal] {
                for mode in SqlFilterMode::ALL {
                    for value in nasty {
                        let mut r = rule("na\"me`x", mode, value);
                        r.end_value = value.into();
                        let Some(sql) = rule_condition(dialect, kind, &r) else {
                            continue; // e.g. a list of nothing
                        };
                        crate::sql_table::validate_fragment(dialect, "WHERE", &sql)
                            .unwrap_or_else(|e| panic!("{dialect:?} {kind:?} {mode:?} {value:?}: {e}\n{sql}"));
                        // Outside string literals the SQL contains no user text.
                        let outside = strip_string_literals(&sql);
                        for needle in ["DROP", "OR '1'", "comment", "日本語", "line1"] {
                            assert!(!outside.contains(needle), "{needle} leaked: {sql}");
                        }
                    }
                }
            }
        }
    }

    /// SQL with every `'...'` literal (with `''`) removed.
    fn strip_string_literals(sql: &str) -> String {
        let mut out = String::new();
        let mut chars = sql.chars().peekable();
        let mut in_string = false;
        while let Some(ch) = chars.next() {
            if in_string {
                if ch == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        in_string = false;
                    }
                }
            } else if ch == '\'' {
                in_string = true;
            } else {
                out.push(ch);
            }
        }
        out
    }

    #[test]
    fn string_literal_escaping_per_dialect() {
        assert_eq!(sql_string_literal(POSTGRES, "a'b\\c"), "'a''b\\c'");
        assert_eq!(sql_string_literal(SQLITE, "a'b\\c"), "'a''b\\c'");
        assert_eq!(sql_string_literal(MYSQL, "a'b\\c"), "'a''b\\\\c'");
        assert_eq!(sql_string_literal(POSTGRES, ""), "''");
    }

    #[test]
    fn nul_characters_are_rejected() {
        let rules = [rule("a", SqlFilterMode::Equals, "x\0y")];
        let error = build_where(POSTGRES, &rules, |_| Kind::Text).unwrap_err();
        assert!(error.contains("NUL"), "{error}");
    }

    #[test]
    fn numeric_literal_grammar() {
        for ok in ["0", "7", "-7", "+7", "7.5", ".5", "7.", "1e5", "1E-5", "-1.5e+10"] {
            assert!(is_numeric_literal(ok), "{ok}");
        }
        for bad in ["", "-", "+", ".", "e5", "1e", "1e+", "1 ", " 1", "1,5", "0x1", "1f"] {
            assert!(!is_numeric_literal(bad), "{bad:?}");
        }
    }

    #[test]
    fn state_manages_rules_and_moves() {
        let mut state = SqlFilterState::default();
        let a = state.add_rule("a");
        let b = state.add_rule("b");
        let c = state.add_rule("c");
        assert_eq!((a, b, c), (1, 2, 3));
        assert!(state.move_rule(c, 0));
        assert_eq!(
            state.rules.iter().map(|r| r.column.as_str()).collect::<Vec<_>>(),
            ["c", "a", "b"]
        );
        assert!(state.move_rule(c, 99), "clamped to the end");
        assert_eq!(state.rules.last().unwrap().id, c);
        assert!(!state.move_rule(c, 2), "already there");
        assert!(!state.move_rule(42, 0));
        state.remove_rule(b);
        assert_eq!(state.rules.len(), 2);
        // Ids never collide with surviving rules.
        assert_eq!(state.add_rule("d"), c + 1);
        // from_saved repairs zero / duplicate ids.
        let fixed = SqlFilterState::from_saved(
            vec![
                SqlFilterRule { id: 0, ..Default::default() },
                SqlFilterRule { id: 5, ..Default::default() },
                SqlFilterRule { id: 5, ..Default::default() },
            ],
            None,
        );
        let mut ids: Vec<u64> = fixed.rules.iter().map(|r| r.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert!(!ids.contains(&0));
    }

    #[test]
    fn rules_round_trip_through_json_and_old_files_default() {
        let mut r = rule("c", SqlFilterMode::NotBetween, "1");
        r.end_value = "2".into();
        r.conjunction = SqlFilterConjunction::Or;
        r.disabled = true;
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"not-between\"") && json.contains("\"or\""), "{json}");
        assert_eq!(serde_json::from_str::<SqlFilterRule>(&json).unwrap(), r);
        let sparse: SqlFilterRule = serde_json::from_str(r#"{"id":3,"column":"x"}"#).unwrap();
        assert_eq!(sparse.mode, SqlFilterMode::Equals);
        assert!(!sparse.disabled);
    }

    #[test]
    fn mode_labels_are_unique_and_round_trip() {
        for mode in SqlFilterMode::ALL {
            assert_eq!(SqlFilterMode::from_label(mode.label()), Some(mode));
        }
        let labels: std::collections::HashSet<_> =
            SqlFilterMode::ALL.iter().map(|mode| mode.label()).collect();
        assert_eq!(labels.len(), SqlFilterMode::ALL.len());
        assert_eq!(SqlFilterMode::from_label("nope"), None);
    }

}
