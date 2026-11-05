pub fn default_database(uri: &str) -> Result<Option<String>, String> {
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or("Invalid MongoDB connection URI")?;
    if !scheme.eq_ignore_ascii_case("mongodb") && !scheme.eq_ignore_ascii_case("mongodb+srv") {
        return Err("Invalid MongoDB connection URI".into());
    }
    let Some((_, path)) = rest.split_once('/') else {
        return Ok(None);
    };
    let raw = path.split(['?', '#']).next().unwrap_or_default();
    if raw.contains('/') {
        return Err("Invalid MongoDB database path".into());
    }
    if raw.is_empty() {
        return Ok(None);
    }
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'%'
            && (index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit())
        {
            return Err("Invalid encoded MongoDB database name".into());
        }
    }
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| "Invalid MongoDB database name encoding")?;
    if decoded.contains(['/', '\0']) || decoded.trim() != decoded {
        return Err("Invalid MongoDB database name".into());
    }
    Ok(Some(decoded.into_owned()))
}

pub fn collection_template(collection: &str) -> String {
    if collection.is_empty() {
        return String::new();
    }
    let bytes = collection.as_bytes();
    let reference = if (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        format!(".{collection}")
    } else {
        format!(
            "[{}]",
            serde_json::to_string(collection).expect("string encodes as JSON")
        )
    };
    format!("db{reference}.find({{}})")
}

/// What Run does with a script: the fast path (one `find` / `findOne`: an editable paged grid) or
/// the MongoDB Shell plugin (everything else, including what the fast path refuses).
#[derive(Debug)]
pub enum ScriptKind {
    Read(Box<engine_mongo_script::MongoReadScript>),
    Shell,
    /// Nothing to run; the message says so.
    Empty(String),
}

pub fn classify(script: &str) -> ScriptKind {
    if script.trim().is_empty() {
        return ScriptKind::Empty("Enter a script to run".into());
    }
    match engine_mongo_script::inspect_read_script(script) {
        Ok(plan) => ScriptKind::Read(Box::new(plan)),
        Err(_) => ScriptKind::Shell,
    }
}

/// Why a script has no database to start in.
#[derive(Debug, PartialEq, Eq)]
pub enum ScriptDatabaseError {
    /// Neither the tab nor the connection URI names one.
    NoneSelected,
    /// The URI's database path is not usable (the reason `default_database` gives).
    Uri(String),
}

impl std::fmt::Display for ScriptDatabaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoneSelected => f.write_str("Select a MongoDB database before running the script"),
            Self::Uri(reason) => f.write_str(reason),
        }
    }
}

/// The database a script starts in: the tab's, else the one in the connection URI.
pub fn script_database(selected_database: &str, uri: &str) -> Result<String, ScriptDatabaseError> {
    if !selected_database.is_empty() {
        return Ok(selected_database.to_string());
    }
    default_database(uri)
        .map_err(ScriptDatabaseError::Uri)?
        .ok_or(ScriptDatabaseError::NoneSelected)
}

/// Byte ranges of the statements of a shell script. A statement ends at a
/// top-level `;` or at a blank line, never inside strings, regex-free
/// brackets or comments, so `db.a.find({\n  x: 1\n})` stays one statement.
/// Ranges are trimmed and non-empty.
pub fn statement_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut index = 0usize;
    let push = |from: usize, to: usize, ranges: &mut Vec<std::ops::Range<usize>>| {
        let slice = &text[from..to];
        let trimmed = slice.trim();
        if !trimmed.is_empty() {
            let lead = slice.len() - slice.trim_start().len();
            ranges.push(from + lead..from + lead + trimmed.len());
        }
    };
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'' | b'`') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                    index += 1;
                }
                index += 1;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' if depth <= 0 => {
                push(start, index, &mut ranges);
                start = index + 1;
                depth = 0;
            }
            b'\n' if depth <= 0 => {
                // A blank line (only whitespace until the next newline) ends a statement.
                let rest = &text[index + 1..];
                let blank = rest
                    .split('\n')
                    .next()
                    .is_some_and(|line| line.trim().is_empty() && index + 1 + line.len() < text.len());
                if blank {
                    push(start, index, &mut ranges);
                    start = index + 1;
                }
            }
            _ => {}
        }
        index += 1;
    }
    push(start, text.len(), &mut ranges);
    ranges
}

/// The statement the cursor is in (or the closest one before it).
pub fn statement_at(text: &str, cursor: usize) -> Option<std::ops::Range<usize>> {
    let ranges = statement_ranges(text);
    ranges
        .iter()
        .find(|range| range.start <= cursor && cursor <= range.end)
        .or_else(|| ranges.iter().rev().find(|range| range.end <= cursor))
        .or_else(|| ranges.first())
        .cloned()
}

/// Re-indents the object / array arguments of every `db...` call chain with
/// the shared pipeline formatter (key order preserved). Text it cannot parse
/// (functions, template strings, ...) makes the whole format fail instead of
/// guessing, and nothing is changed.
pub fn format_script(text: &str) -> Result<String, String> {
    use crate::mongo_aggregate as engine;
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for range in statement_ranges(text) {
        out.push_str(&text[last..range.start]);
        let statement = &text[range.clone()];
        out.push_str(&format_statement(statement, &engine::parse_value, &engine::format_value)?);
        last = range.end;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

fn format_statement(
    statement: &str,
    parse: &dyn Fn(&str) -> Result<bson::Bson, crate::mongo_aggregate::PipelineError>,
    format: &dyn Fn(&bson::Bson) -> String,
) -> Result<String, String> {
    let bytes = statement.as_bytes();
    let mut out = String::new();
    let mut index = 0usize;
    let mut chunk_start = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'' | b'`') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
                index += 1;
            }
            b'(' => {
                // Find the matching ')'.
                let open = index;
                let mut depth = 0i32;
                let mut cursor = index;
                while cursor < bytes.len() {
                    match bytes[cursor] {
                        quote @ (b'"' | b'\'' | b'`') => {
                            cursor += 1;
                            while cursor < bytes.len() && bytes[cursor] != quote {
                                if bytes[cursor] == b'\\' {
                                    cursor += 1;
                                }
                                cursor += 1;
                            }
                        }
                        b'(' | b'[' | b'{' => depth += 1,
                        b')' | b']' | b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    cursor += 1;
                }
                if cursor >= bytes.len() {
                    return Err("unbalanced parentheses".into());
                }
                let inner = &statement[open + 1..cursor];
                out.push_str(&statement[chunk_start..=open]);
                out.push_str(&format_arguments(inner, parse, format)?);
                out.push(')');
                chunk_start = cursor + 1;
                index = cursor + 1;
                continue;
            }
            _ => index += 1,
        }
    }
    out.push_str(&statement[chunk_start..]);
    Ok(out)
}

fn format_arguments(
    inner: &str,
    parse: &dyn Fn(&str) -> Result<bson::Bson, crate::mongo_aggregate::PipelineError>,
    format: &dyn Fn(&bson::Bson) -> String,
) -> Result<String, String> {
    if inner.trim().is_empty() {
        return Ok(String::new());
    }
    let bytes = inner.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let (mut depth, mut start, mut index) = (0i32, 0usize, 0usize);
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'"' | b'\'' | b'`') => {
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    parts.push(&inner[start..]);
    let mut formatted = Vec::with_capacity(parts.len());
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.starts_with('{') || part.starts_with('[') {
            let value = parse(part).map_err(|error| error.to_string())?;
            formatted.push(format(&value));
        } else {
            formatted.push(part.to_string());
        }
    }
    Ok(formatted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_semicolons_and_blank_lines_not_inside_literals() {
        let text = "db.a.find({ x: \"a;b\" });\ndb.b.find({\n  y: 1\n})\n\n// note;\ndb.c.find({})";
        let found: Vec<&str> = statement_ranges(text).into_iter().map(|range| &text[range]).collect();
        assert_eq!(
            found,
            ["db.a.find({ x: \"a;b\" })", "db.b.find({\n  y: 1\n})", "// note;\ndb.c.find({})"]
        );
        assert!(statement_ranges("  \n ; ;").is_empty());
    }

    #[test]
    fn current_statement_follows_the_cursor() {
        let text = "db.a.find({});\ndb.b.find({})";
        let at = |cursor| statement_at(text, cursor).map(|range| &text[range]);
        assert_eq!(at(0), Some("db.a.find({})"));
        assert_eq!(at(14), Some("db.a.find({})"), "on the semicolon");
        assert_eq!(at(20), Some("db.b.find({})"));
        assert_eq!(at(text.len()), Some("db.b.find({})"));
        assert_eq!(statement_at("", 0), None);
    }

    #[test]
    fn format_script_re_indents_arguments_keeping_order_and_untouched_text() {
        let text = "db.users.find({b:1,a:{ $gt : 2 }}).sort({ zeta:-1, alpha:1 }).limit(5);\n// keep me\ndb.logs.aggregate([{$match:{ok:true}}])";
        let formatted = format_script(text).unwrap();
        assert_eq!(
            formatted,
            "db.users.find({ b: 1, a: { $gt: 2 } }).sort({ zeta: -1, alpha: 1 }).limit(5);\n// keep me\ndb.logs.aggregate([{ $match: { ok: true } }])"
        );
        assert_eq!(format_script(&formatted).unwrap(), formatted, "idempotent");
        assert_eq!(
            engine_mongo_script::inspect_read_script(formatted.lines().next().unwrap().trim_end_matches(';')).unwrap().collection,
            "users",
            "the formatted script still runs"
        );
    }

    #[test]
    fn format_script_refuses_what_it_cannot_parse_and_changes_nothing() {
        assert!(format_script("db.a.find({ x: function() { return 1 } })").is_err());
        assert!(format_script("db.a.find({ x: })").is_err());
        assert_eq!(format_script("").unwrap(), "");
        assert_eq!(format_script("db.a.find()").unwrap(), "db.a.find()");
    }

    #[test]
    fn default_database_comes_only_from_uri_path_not_auth_source_or_admin() {
        assert_eq!(
            default_database("mongodb://a:27017,b:27018/sales%2Darchive?authSource=admin").unwrap(),
            Some("sales-archive".into())
        );
        assert_eq!(
            default_database("mongodb+srv://cluster.test/?authSource=admin").unwrap(),
            None
        );
        assert!(default_database("mongodb://host/bad%ZZ").is_err());
        assert!(default_database("mongodb://host/db/extra").is_err());
    }

    #[test]
    fn collection_template_quotes_special_names_without_executing() {
        assert_eq!(collection_template("users"), "db.users.find({})");
        assert_eq!(collection_template("a\"b"), "db[\"a\\\"b\"].find({})");
        for collection in ["a\"b", "a\\b", "café", "a-b"] {
            let script = collection_template(collection);
            assert_eq!(engine_mongo_script::inspect_read_script(&script).unwrap().collection, collection);
        }
    }

    #[test]
    fn a_script_starts_in_the_tabs_database_else_the_uri_one() {
        assert_eq!(script_database("", "mongodb://host/warehouse?authSource=admin").unwrap(), "warehouse");
        assert_eq!(script_database("selected", "mongodb://host/").unwrap(), "selected");
        assert_eq!(script_database("", "mongodb://host/?authSource=admin"), Err(ScriptDatabaseError::NoneSelected));
        assert!(matches!(script_database("", "mongodb://host/db/extra"), Err(ScriptDatabaseError::Uri(_))));
    }

    #[test]
    fn a_single_find_is_the_fast_path_and_everything_else_is_a_shell_script() {
        for read in [
            "db.events.find({ active: true })",
            "db.events.findOne({})",
            "db['odd name'].find({}).sort({ a: 1 }).limit(5)",
        ] {
            let ScriptKind::Read(plan) = classify(read) else { panic!("{read} should stay a find") };
            assert!(!plan.collection.is_empty());
        }
        for shell in [
            "db.events.deleteMany({})",
            "db.events.insertOne({ a: 1 })",
            "db.events.aggregate([{ $match: {} }])",
            "use admin",
            "show dbs",
            "SELECT * FROM events",
            "print(1)",
            "for (let i = 0; i < 3; i++) db.c.insertOne({ i })",
            "db.a.find({});\ndb.b.find({})",
            "db.events.find({ a: })",
            "db.events.find({}).forEach(printjson)",
            "var n = db.events.countDocuments({}); print(n)",
        ] {
            assert!(matches!(classify(shell), ScriptKind::Shell), "{shell}");
        }
        for empty in ["", "  \n\t "] {
            assert!(matches!(classify(empty), ScriptKind::Empty(_)), "{empty:?}");
        }
    }
}
