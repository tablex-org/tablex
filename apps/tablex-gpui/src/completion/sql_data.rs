//! Static SQL vocabulary: keywords, functions and snippets, tagged with the
//! dialects that accept them. Everything is `&'static` data filtered by a bit
//! mask at query time, so a keystroke never allocates the catalog.

use super::{sql_data_clickhouse, SqlFlavor};

/// Dialect bits for [`Keyword::dialects`] / [`Function::dialects`].
pub(super) const PG: u8 = 1;
pub(super) const SQLITE: u8 = 2;
pub(super) const MYSQL: u8 = 4;
pub(super) const MARIADB: u8 = 8;
pub(super) const CH: u8 = 32;
/// The four classic dialects; `ANY` adds ClickHouse for the shared core.
const CLASSIC: u8 = PG | SQLITE | MYSQL | MARIADB;
pub(super) const ANY: u8 = CLASSIC | CH;
const MY: u8 = MYSQL | MARIADB;
/// Function takes no arguments: inserted as `NAME()`.
pub(super) const NOARG: u8 = 16;
const DIALECT_BITS: u8 = ANY;

impl SqlFlavor {
    pub(super) const fn bit(self) -> u8 {
        match self {
            Self::Postgres => PG,
            Self::Sqlite => SQLITE,
            Self::Mysql => MYSQL,
            Self::Mariadb => MARIADB,
            Self::Clickhouse => CH,
        }
    }
}

/// Where a keyword is useful, so a context only offers what can follow.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Place {
    /// First word of a statement.
    Statement,
    /// Continues a query after a table reference (`JOIN`, `WHERE`, ...).
    Clause,
    /// Inside an expression (`AND`, `IN`, `NULL`, ...).
    Expr,
    /// Data definition / data types.
    Ddl,
}

pub(super) struct Keyword {
    pub text: &'static str,
    pub desc: &'static str,
    pub dialects: u8,
    pub place: Place,
}

pub(super) const fn kw(text: &'static str, desc: &'static str, dialects: u8, place: Place) -> Keyword {
    Keyword {
        text,
        desc,
        dialects,
        place,
    }
}

use Place::{Clause, Ddl, Expr, Statement};

pub(super) static KEYWORDS: &[Keyword] = &[
    kw("SELECT", "Query rows", ANY, Statement),
    kw("INSERT INTO", "Insert rows into a table", ANY, Statement),
    kw("UPDATE", "Modify existing rows", ANY, Statement),
    kw("DELETE FROM", "Delete rows from a table", ANY, Statement),
    kw("WITH", "Define common table expressions", ANY, Statement),
    kw("CREATE TABLE", "Create a table", ANY, Statement),
    kw("CREATE INDEX", "Create an index", ANY, Statement),
    kw("CREATE VIEW", "Create a view", ANY, Statement),
    kw("ALTER TABLE", "Change a table definition", ANY, Statement),
    kw("DROP TABLE", "Remove a table", ANY, Statement),
    kw("DROP INDEX", "Remove an index", ANY, Statement),
    kw("DROP VIEW", "Remove a view", ANY, Statement),
    kw(
        "TRUNCATE",
        "Remove all rows from a table",
        PG | MY | CH,
        Statement,
    ),
    kw("EXPLAIN", "Show the query plan", ANY, Statement),
    kw(
        "EXPLAIN ANALYZE",
        "Run the query and show the plan",
        PG | MY,
        Statement,
    ),
    kw("BEGIN", "Start a transaction", CLASSIC, Statement),
    kw("COMMIT", "Commit the transaction", CLASSIC, Statement),
    kw("ROLLBACK", "Roll back the transaction", CLASSIC, Statement),
    kw("SHOW", "Show server information", PG | MY | CH, Statement),
    kw("USE", "Switch the active database", MY | CH, Statement),
    kw("DESCRIBE", "Describe a table", MY | CH, Statement),
    kw(
        "PRAGMA",
        "Query or change SQLite settings",
        SQLITE,
        Statement,
    ),
    kw(
        "VACUUM",
        "Rebuild the database file",
        PG | SQLITE,
        Statement,
    ),
    kw("ANALYZE", "Collect planner statistics", CLASSIC, Statement),
    kw("COPY", "Copy data to or from a file", PG, Statement),
    kw("GRANT", "Grant privileges", PG | MY | CH, Statement),
    kw("REVOKE", "Revoke privileges", PG | MY | CH, Statement),
    kw(
        "REPLACE INTO",
        "Insert or replace rows",
        SQLITE | MY,
        Statement,
    ),
    kw(
        "ATTACH DATABASE",
        "Attach another database file",
        SQLITE,
        Statement,
    ),
    kw("FROM", "Source tables", ANY, Clause),
    kw("WHERE", "Filter rows", ANY, Clause),
    kw("JOIN", "Join another table", ANY, Clause),
    kw(
        "INNER JOIN",
        "Rows with matches in both tables",
        ANY,
        Clause,
    ),
    kw("LEFT JOIN", "All rows from the left table", ANY, Clause),
    kw(
        "RIGHT JOIN",
        "All rows from the right table",
        PG | MY | CH,
        Clause,
    ),
    kw("FULL JOIN", "All rows from both tables", PG | CH, Clause),
    kw("CROSS JOIN", "Cartesian product", ANY, Clause),
    kw("ON", "Join condition", ANY, Clause),
    kw("USING", "Join on equally named columns", ANY, Clause),
    kw("GROUP BY", "Group rows", ANY, Clause),
    kw("HAVING", "Filter groups", ANY, Clause),
    kw("ORDER BY", "Sort rows", ANY, Clause),
    kw("LIMIT", "Limit the number of rows", ANY, Clause),
    kw("OFFSET", "Skip rows", ANY, Clause),
    kw("FETCH FIRST", "Limit rows (SQL standard)", PG, Clause),
    kw("UNION", "Combine results without duplicates", ANY, Clause),
    kw(
        "UNION ALL",
        "Combine results keeping duplicates",
        ANY,
        Clause,
    ),
    kw(
        "INTERSECT",
        "Rows present in both results",
        PG | SQLITE | CH,
        Clause,
    ),
    kw(
        "EXCEPT",
        "Rows missing from the second result",
        PG | SQLITE | CH,
        Clause,
    ),
    kw("VALUES", "Literal rows", ANY, Clause),
    kw("SET", "Assign column values", ANY, Clause),
    kw(
        "RETURNING",
        "Return the affected rows",
        PG | SQLITE | MARIADB,
        Clause,
    ),
    kw(
        "ON CONFLICT",
        "Handle unique violations",
        PG | SQLITE,
        Clause,
    ),
    kw(
        "ON DUPLICATE KEY UPDATE",
        "Update on duplicate key",
        MY,
        Clause,
    ),
    kw("FOR UPDATE", "Lock the selected rows", PG | MY, Clause),
    kw("AS", "Alias", ANY, Clause),
    kw("DISTINCT", "Remove duplicate rows", ANY, Expr),
    kw("DISTINCT ON", "First row of each group", PG, Expr),
    kw("ALL", "Keep duplicate rows", ANY, Expr),
    kw("AND", "Logical AND", ANY, Expr),
    kw("OR", "Logical OR", ANY, Expr),
    kw("NOT", "Logical negation", ANY, Expr),
    kw("IN", "Value is in a list", ANY, Expr),
    kw("NOT IN", "Value is not in a list", ANY, Expr),
    kw("BETWEEN", "Value is within a range", ANY, Expr),
    kw("LIKE", "Pattern match", ANY, Expr),
    kw("ILIKE", "Case-insensitive pattern match", PG | CH, Expr),
    kw("GLOB", "Unix glob pattern match", SQLITE, Expr),
    kw("REGEXP", "Regular expression match", SQLITE | MY, Expr),
    kw("SIMILAR TO", "SQL regular expression match", PG, Expr),
    kw("IS NULL", "Value is NULL", ANY, Expr),
    kw("IS NOT NULL", "Value is not NULL", ANY, Expr),
    kw("NULL", "Missing value", ANY, Expr),
    kw("TRUE", "Boolean true", ANY, Expr),
    kw("FALSE", "Boolean false", ANY, Expr),
    kw("EXISTS", "Subquery returns rows", ANY, Expr),
    kw("NOT EXISTS", "Subquery returns no rows", ANY, Expr),
    kw("ANY", "Compare with any value of a set", PG | MY, Expr),
    kw("CASE", "Conditional expression", ANY, Expr),
    kw("WHEN", "Case branch", ANY, Expr),
    kw("THEN", "Case result", ANY, Expr),
    kw("ELSE", "Default case result", ANY, Expr),
    kw("END", "Close a CASE expression", ANY, Expr),
    kw("ASC", "Ascending order", ANY, Expr),
    kw("DESC", "Descending order", ANY, Expr),
    kw("NULLS FIRST", "Sort NULLs first", PG | SQLITE | CH, Expr),
    kw("NULLS LAST", "Sort NULLs last", PG | SQLITE | CH, Expr),
    kw("OVER", "Window specification", ANY, Expr),
    kw("PARTITION BY", "Window partition", ANY, Expr),
    kw("INTERVAL", "Time interval literal", PG | MY | CH, Expr),
    kw("DEFAULT", "Column default value", ANY, Expr),
    kw("TABLE", "Table", ANY, Ddl),
    kw("INDEX", "Index", ANY, Ddl),
    kw("VIEW", "View", ANY, Ddl),
    kw("IF NOT EXISTS", "Skip when the object exists", ANY, Ddl),
    kw("IF EXISTS", "Skip when the object is missing", ANY, Ddl),
    kw("PRIMARY KEY", "Primary key constraint", ANY, Ddl),
    kw("FOREIGN KEY", "Foreign key constraint", CLASSIC, Ddl),
    kw("REFERENCES", "Referenced table", CLASSIC, Ddl),
    kw("UNIQUE", "Unique constraint", CLASSIC, Ddl),
    kw("CHECK", "Check constraint", ANY, Ddl),
    kw("CONSTRAINT", "Name a constraint", ANY, Ddl),
    kw("AUTO_INCREMENT", "Generated integer key", MY, Ddl),
    kw("AUTOINCREMENT", "Generated integer key", SQLITE, Ddl),
    kw("ADD COLUMN", "Add a column", ANY, Ddl),
    kw("DROP COLUMN", "Drop a column", ANY, Ddl),
    kw("RENAME TO", "Rename the object", CLASSIC, Ddl),
    kw("CASCADE", "Cascade to dependents", PG | MY, Ddl),
    kw("WITHOUT ROWID", "Table without a rowid", SQLITE, Ddl),
    kw("ENGINE", "Storage engine", MY | CH, Ddl),
    kw("INTEGER", "Integer type", ANY, Ddl),
    kw("BIGINT", "64-bit integer type", ANY, Ddl),
    kw("SMALLINT", "16-bit integer type", ANY, Ddl),
    kw("SERIAL", "Auto-incrementing integer", PG, Ddl),
    kw("BIGSERIAL", "Auto-incrementing bigint", PG, Ddl),
    kw("NUMERIC", "Exact numeric type", ANY, Ddl),
    kw("DECIMAL", "Exact numeric type", ANY, Ddl),
    kw("REAL", "Floating point type", ANY, Ddl),
    kw("DOUBLE PRECISION", "Double precision type", PG, Ddl),
    kw("BOOLEAN", "Boolean type", PG | MY | CH, Ddl),
    kw("TEXT", "Variable-length text", ANY, Ddl),
    kw("VARCHAR", "Variable-length string", ANY, Ddl),
    kw("CHAR", "Fixed-length string", ANY, Ddl),
    kw("BLOB", "Binary data", SQLITE | MY, Ddl),
    kw("BYTEA", "Binary data", PG, Ddl),
    kw("DATE", "Calendar date", ANY, Ddl),
    kw("TIME", "Time of day", PG | MY, Ddl),
    kw("TIMESTAMP", "Date and time", ANY, Ddl),
    kw("TIMESTAMPTZ", "Date and time with zone", PG, Ddl),
    kw("DATETIME", "Date and time", SQLITE | MY | CH, Ddl),
    kw("JSON", "JSON document", PG | MY | CH, Ddl),
    kw("JSONB", "Binary JSON document", PG, Ddl),
    kw("UUID", "Universally unique identifier", PG | CH, Ddl),
];

pub(super) struct Function {
    pub name: &'static str,
    pub desc: &'static str,
    pub flags: u8,
}

impl Function {
    pub(super) const fn available_in(&self, flavor: SqlFlavor) -> bool {
        self.flags & flavor.bit() != 0
    }

    pub(super) const fn no_args(&self) -> bool {
        self.flags & NOARG != 0
    }
}

pub(super) const fn func(name: &'static str, desc: &'static str, flags: u8) -> Function {
    Function { name, desc, flags }
}

/// Ordered by popularity: equal-ranked matches keep this order.
pub(super) static FUNCTIONS: &[Function] = &[
    func("SUM", "Sum of a numeric column", ANY),
    func("COUNT", "Number of rows", ANY),
    func("AVG", "Average of a numeric column", ANY),
    func("MIN", "Minimum value", ANY),
    func("MAX", "Maximum value", ANY),
    func("COALESCE", "First non-NULL argument", ANY),
    func("NULLIF", "NULL when both values are equal", ANY),
    func("IFNULL", "Alternate value when NULL", SQLITE | MY | CH),
    func("IF", "Value chosen by a condition", MY | CH),
    func("IIF", "Value chosen by a condition", SQLITE),
    func("CAST", "Convert a value to a type", ANY),
    func("SIGN", "Sign of a number", PG | MY | CH),
    func("SQRT", "Square root", PG | MY | CH),
    func("STDDEV", "Standard deviation", PG | MY),
    func("STDDEV_POP", "Population standard deviation", PG | MY | CH),
    func("STDDEV_SAMP", "Sample standard deviation", PG | MY | CH),
    func("VARIANCE", "Variance", PG | MY),
    func("STRPOS", "Position of a substring", PG),
    func("SUBSTR", "Extract a substring", ANY),
    func("SUBSTRING", "Extract a substring", PG | MY | SQLITE),
    func("SPLIT_PART", "Field of a delimited string", PG),
    func("STRING_AGG", "Concatenate values with a separator", PG),
    func("GROUP_CONCAT", "Concatenate values of a group", SQLITE | MY),
    func("ARRAY_AGG", "Collect values into an array", PG),
    func("JSON_AGG", "Aggregate values as a JSON array", PG),
    func("JSONB_AGG", "Aggregate values as a JSONB array", PG),
    func("JSON_ARRAYAGG", "Aggregate values as a JSON array", MY),
    func("JSON_OBJECTAGG", "Aggregate pairs as a JSON object", MY),
    func("BOOL_AND", "True when all values are true", PG),
    func("BOOL_OR", "True when any value is true", PG),
    func("TOTAL", "Sum as a float, 0.0 for no rows", SQLITE),
    func("ROW_NUMBER", "Row number within the window", ANY | NOARG),
    func("RANK", "Rank with gaps", ANY | NOARG),
    func("DENSE_RANK", "Rank without gaps", ANY | NOARG),
    func("NTILE", "Bucket number", CLASSIC),
    func("LAG", "Value from a previous row", CLASSIC),
    func("LEAD", "Value from a following row", CLASSIC),
    func("FIRST_VALUE", "First value of the window", ANY),
    func("LAST_VALUE", "Last value of the window", ANY),
    func("NTH_VALUE", "Nth value of the window", ANY),
    func("ABS", "Absolute value", ANY),
    func("ROUND", "Round to a precision", ANY),
    func("CEIL", "Round up", PG | MY | CH),
    func("CEILING", "Round up", PG | MY | CH),
    func("FLOOR", "Round down", PG | MY | CH),
    func("TRUNC", "Truncate toward zero", PG),
    func("TRUNCATE", "Truncate to decimals", MY),
    func("POWER", "Raise to a power", PG | MY | CH),
    func("POW", "Raise to a power", MY | CH),
    func("MOD", "Remainder of a division", PG | MY | CH),
    func("EXP", "Exponential", PG | MY | CH),
    func("LN", "Natural logarithm", PG | MY | CH),
    func("LOG", "Logarithm", PG | MY | CH),
    func("LOG10", "Base 10 logarithm", PG | MY | CH),
    func("PI", "Value of pi", PG | MY | CH | NOARG),
    func("RANDOM", "Random value", PG | SQLITE | NOARG),
    func("RAND", "Random float", MY | CH | NOARG),
    func("GREATEST", "Largest argument", PG | MY | CH),
    func("LEAST", "Smallest argument", PG | MY | CH),
    func("LENGTH", "Length of a string", ANY),
    func("CHAR_LENGTH", "Number of characters", PG | MY | CH),
    func("LOWER", "Convert to lower case", ANY),
    func("UPPER", "Convert to upper case", ANY),
    func("TRIM", "Remove surrounding characters", ANY),
    func("LTRIM", "Remove leading characters", ANY),
    func("RTRIM", "Remove trailing characters", ANY),
    func("REPLACE", "Replace occurrences of a substring", ANY),
    func("CONCAT", "Concatenate strings", PG | MY | CH),
    func("CONCAT_WS", "Concatenate with a separator", PG | MY | CH),
    func("LEFT", "Leftmost characters", PG | MY | CH),
    func("RIGHT", "Rightmost characters", PG | MY | CH),
    func("LPAD", "Left-pad a string", PG | MY | CH),
    func("RPAD", "Right-pad a string", PG | MY | CH),
    func("REPEAT", "Repeat a string", PG | MY | CH),
    func("REVERSE", "Reverse a string", PG | MY | CH),
    func("POSITION", "Position of a substring", PG | MY | CH),
    func("INSTR", "Position of a substring", SQLITE | MY),
    func("LOCATE", "Position of a substring", MY | CH),
    func("INITCAP", "Capitalize each word", PG),
    func("TRANSLATE", "Replace characters", PG),
    func("STARTS_WITH", "String starts with a prefix", PG),
    func("FORMAT", "Format a value", PG | MY),
    func("PRINTF", "Formatted string", SQLITE),
    func("TO_CHAR", "Format a value as text", PG),
    func("REGEXP_REPLACE", "Replace by regular expression", PG | MY | CH),
    func("REGEXP_MATCHES", "Regular expression matches", PG),
    func("REGEXP_LIKE", "Regular expression test", MY),
    func("SUBSTRING_INDEX", "Substring before a delimiter count", MY),
    func("FIND_IN_SET", "Index in a comma-separated list", MY),
    func("ASCII", "Code of the first character", PG | MY | CH),
    func("CHR", "Character from a code", PG),
    func("HEX", "Hexadecimal representation", SQLITE | MY),
    func("MD5", "MD5 hash", PG | MY | CH),
    func("SHA1", "SHA-1 hash", MY | CH),
    func("SHA2", "SHA-2 hash", MY),
    func("QUOTE", "Quote a value as a SQL literal", SQLITE | MY),
    func("NOW", "Current date and time", PG | MY | CH | NOARG),
    func("CURRENT_DATE", "Current date", CLASSIC),
    func("CURRENT_TIME", "Current time", CLASSIC),
    func("CURRENT_TIMESTAMP", "Current date and time", CLASSIC),
    func("DATE_TRUNC", "Truncate a timestamp", PG),
    func("DATE_PART", "Extract a part of a date", PG),
    func("EXTRACT", "Extract a part of a date", PG | MY | CH),
    func("AGE", "Interval between timestamps", PG),
    func("TO_DATE", "Parse a date", PG),
    func("TO_TIMESTAMP", "Parse a timestamp", PG),
    func("GENERATE_SERIES", "Series of values", PG),
    func("DATE", "Date part of a value", SQLITE | MY),
    func("TIME", "Time part of a value", SQLITE | MY),
    func("DATETIME", "Date and time of a value", SQLITE),
    func("STRFTIME", "Format a date and time", SQLITE),
    func("JULIANDAY", "Julian day number", SQLITE),
    func("UNIXEPOCH", "Unix timestamp", SQLITE),
    func("DATE_FORMAT", "Format a date", MY | CH),
    func("STR_TO_DATE", "Parse a date", MY),
    func("DATEDIFF", "Days between two dates", MY | CH),
    func("DATE_ADD", "Add an interval to a date", MY | CH),
    func("DATE_SUB", "Subtract an interval from a date", MY | CH),
    func("TIMESTAMPDIFF", "Difference between datetimes", MY),
    func("CURDATE", "Current date", MY | NOARG),
    func("CURTIME", "Current time", MY | NOARG),
    func("UNIX_TIMESTAMP", "Unix timestamp", MY | CH),
    func("FROM_UNIXTIME", "Date from a Unix timestamp", MY | CH),
    func("YEAR", "Year of a date", MY | CH),
    func("MONTH", "Month of a date", MY | CH),
    func("DAY", "Day of a date", MY | CH),
    func("HOUR", "Hour of a time", MY | CH),
    func("MINUTE", "Minute of a time", MY | CH),
    func("SECOND", "Second of a time", MY | CH),
    func("LAST_DAY", "Last day of the month", MY),
    func("JSON_EXTRACT", "Extract a value from JSON", SQLITE | MY),
    func("JSON_OBJECT", "Build a JSON object", SQLITE | MY),
    func("JSON_ARRAY", "Build a JSON array", SQLITE | MY),
    func("JSON_VALID", "Check JSON validity", SQLITE | MY),
    func("JSON_TYPE", "Type of a JSON value", SQLITE),
    func("JSON_UNQUOTE", "Unquote a JSON string", MY),
    func("JSON_BUILD_OBJECT", "Build a JSON object", PG),
    func("JSONB_BUILD_OBJECT", "Build a JSONB object", PG),
    func("JSON_ARRAY_ELEMENTS", "Expand a JSON array", PG),
    func("JSONB_SET", "Replace a value in JSONB", PG),
    func("JSON_EXTRACT_PATH_TEXT", "Extract a JSON path as text", PG),
    func("TO_JSON", "Convert a value to JSON", PG),
    func("TO_JSONB", "Convert a value to JSONB", PG),
    func("ARRAY_LENGTH", "Length of an array dimension", PG),
    func("UNNEST", "Expand an array to rows", PG),
    func("ARRAY_TO_STRING", "Join array elements", PG),
    func("STRING_TO_ARRAY", "Split a string into an array", PG),
    func("ARRAY_APPEND", "Append to an array", PG),
    func("ARRAY_REMOVE", "Remove from an array", PG),
    func("GEN_RANDOM_UUID", "Random UUID", PG | NOARG),
    func("UUID", "Generate a UUID", MY | NOARG),
    func("RANDOMBLOB", "Random bytes", SQLITE),
    func(
        "LAST_INSERT_ROWID",
        "Rowid of the last insert",
        SQLITE | NOARG,
    ),
    func("LAST_INSERT_ID", "Id of the last insert", MY | NOARG),
    func(
        "CHANGES",
        "Rows changed by the last statement",
        SQLITE | NOARG,
    ),
    func(
        "ROW_COUNT",
        "Rows affected by the last statement",
        MY | NOARG,
    ),
    func("TYPEOF", "Storage type of a value", SQLITE),
    func("PG_TYPEOF", "Type of a value", PG),
    func("PG_SIZE_PRETTY", "Human-readable size", PG),
    func("PG_TOTAL_RELATION_SIZE", "Total size of a relation", PG),
    func("CURRENT_DATABASE", "Current database", PG | NOARG),
    func("CURRENT_SCHEMA", "Current schema", PG | NOARG),
    func("CURRENT_USER", "Current user", PG | MY),
    func("DATABASE", "Current database", MY | CH | NOARG),
    func("VERSION", "Server version", PG | MY | CH | NOARG),
    func("SQLITE_VERSION", "SQLite version", SQLITE | NOARG),
];

pub(super) struct Snippet {
    /// Matched like a label; the shorthand also matches (`sel` -> `select *`).
    pub label: &'static str,
    pub trigger: &'static str,
    /// One-line preview shown as the menu detail.
    pub summary: &'static str,
    pub body: &'static str,
}

pub(super) static SNIPPETS: &[Snippet] = &[
    Snippet {
        label: "select *",
        trigger: "sel",
        summary: "SELECT * FROM table LIMIT 100;",
        body: "SELECT *\nFROM table\nLIMIT 100;",
    },
    Snippet {
        label: "insert into",
        trigger: "ins",
        summary: "INSERT INTO table (columns) VALUES (values);",
        body: "INSERT INTO table (columns)\nVALUES (values);",
    },
    Snippet {
        label: "update set",
        trigger: "upd",
        summary: "UPDATE table SET column = value WHERE condition;",
        body: "UPDATE table\nSET column = value\nWHERE condition;",
    },
    Snippet {
        label: "common table expression",
        trigger: "cte",
        summary: "WITH name AS (SELECT ...) SELECT * FROM name;",
        body: "WITH name AS (\n  SELECT columns\n  FROM table\n)\nSELECT *\nFROM name;",
    },
    Snippet {
        label: "join",
        trigger: "join",
        summary: "JOIN table ON left_column = right_column",
        body: "JOIN table ON left_column = right_column",
    },
    Snippet {
        label: "case when",
        trigger: "case",
        summary: "CASE WHEN condition THEN value ELSE default END",
        body: "CASE\n  WHEN condition THEN value\n  ELSE default\nEND",
    },
    Snippet {
        label: "create table",
        trigger: "ct",
        summary: "CREATE TABLE table (column type);",
        body: "CREATE TABLE table (\n  column type\n);",
    },
    Snippet {
        label: "exists",
        trigger: "ex",
        summary: "EXISTS (SELECT 1 FROM table WHERE condition)",
        body: "EXISTS (\n  SELECT 1\n  FROM table\n  WHERE condition\n)",
    },
    Snippet {
        label: "not exists",
        trigger: "nex",
        summary: "NOT EXISTS (SELECT 1 FROM table WHERE condition)",
        body: "NOT EXISTS (\n  SELECT 1\n  FROM table\n  WHERE condition\n)",
    },
    Snippet {
        label: "alter table add column",
        trigger: "at",
        summary: "ALTER TABLE table ADD COLUMN column type;",
        body: "ALTER TABLE table\nADD COLUMN column type;",
    },
    Snippet {
        label: "create index",
        trigger: "ci",
        summary: "CREATE INDEX idx_name ON table (column);",
        body: "CREATE INDEX idx_name\nON table (column);",
    },
];

/// Words that must be quoted when they are used as table / column names.
pub(super) fn is_reserved_identifier(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "all",
        "and",
        "any",
        "as",
        "asc",
        "between",
        "both",
        "case",
        "cast",
        "check",
        "collate",
        "column",
        "constraint",
        "create",
        "default",
        "desc",
        "distinct",
        "do",
        "else",
        "end",
        "except",
        "false",
        "fetch",
        "for",
        "foreign",
        "from",
        "grant",
        "group",
        "having",
        "in",
        "intersect",
        "into",
        "key",
        "leading",
        "limit",
        "not",
        "null",
        "offset",
        "on",
        "only",
        "or",
        "order",
        "primary",
        "references",
        "select",
        "some",
        "table",
        "then",
        "to",
        "trailing",
        "true",
        "union",
        "unique",
        "user",
        "using",
        "when",
        "where",
        "window",
        "with",
    ];
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

/// Words that can never be a table alias.
pub(super) fn is_reserved(word: &str) -> bool {
    const RESERVED: &[&str] = &[
        "WHERE",
        "GROUP",
        "ORDER",
        "HAVING",
        "LIMIT",
        "OFFSET",
        "JOIN",
        "INNER",
        "LEFT",
        "RIGHT",
        "FULL",
        "CROSS",
        "NATURAL",
        "OUTER",
        "ON",
        "USING",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "SET",
        "VALUES",
        "SELECT",
        "RETURNING",
        "WINDOW",
        "FETCH",
        "FOR",
        "WITH",
        "FROM",
        "AS",
        "AND",
        "OR",
        "NOT",
        "LATERAL",
        "ONLY",
        // ClickHouse table modifiers that follow a table reference.
        "FINAL",
        "PREWHERE",
        "SAMPLE",
        "SETTINGS",
        "FORMAT",
        "ARRAY",
    ];
    RESERVED
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(word))
}

/// Functions `flavor` accepts, in popularity order.
pub(super) fn functions_for(flavor: SqlFlavor) -> impl Iterator<Item = &'static Function> {
    let extra: &[Function] = match flavor {
        SqlFlavor::Clickhouse => sql_data_clickhouse::FUNCTIONS,
        _ => &[],
    };
    FUNCTIONS
        .iter()
        .chain(extra)
        .filter(move |function| function.available_in(flavor))
}

/// Keywords whose dialect bit matches `flavor`.
pub(super) fn keywords_for(
    flavor: SqlFlavor,
    place: Place,
) -> impl Iterator<Item = &'static Keyword> {
    let bit = flavor.bit() & DIALECT_BITS;
    let extra: &[Keyword] = match flavor {
        SqlFlavor::Clickhouse => sql_data_clickhouse::KEYWORDS,
        _ => &[],
    };
    KEYWORDS
        .iter()
        .chain(extra)
        .filter(move |keyword| keyword.place == place && keyword.dialects & bit != 0)
}
