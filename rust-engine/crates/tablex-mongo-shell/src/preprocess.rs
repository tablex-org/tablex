//! Line commands (`use`, `show`, `it`): rewritten to JavaScript calls before
//! the script runs. They count only as whole lines at the top level of the
//! script: never inside a string, template literal, comment, regex literal or
//! bracket. Replacements stay on their own line, so line numbers in error
//! messages still match what the user typed.

use std::sync::OnceLock;

/// Result of [`rewrite_line_commands`].
#[derive(Debug, PartialEq, Eq)]
pub struct Rewritten {
    pub code: String,
    /// How many line commands were replaced.
    pub commands: usize,
}

struct Patterns {
    use_command: regex::Regex,
    show: regex::Regex,
    iterate: regex::Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        use_command: regex::Regex::new(
            r#"^\s*use\s+(?:"([^"]+)"|'([^']+)'|([^\s;'"()]+))\s*;?\s*(?://.*)?$"#,
        )
        .expect("static regex"),
        show: regex::Regex::new(r"^\s*show\s+([A-Za-z]+)\s*;?\s*(?://.*)?$").expect("static regex"),
        iterate: regex::Regex::new(r"^\s*it\s*;?\s*(?://.*)?$").expect("static regex"),
    })
}

/// Scanner state carried from one line to the next.
#[derive(Default)]
struct Scan {
    depth: usize,
    in_block_comment: bool,
    /// Bracket depth at each open `${` of a template literal.
    template_exprs: Vec<usize>,
    in_template: bool,
    /// The last significant character (to tell a regex literal from a division).
    previous: Option<char>,
}

impl Scan {
    fn at_top_level(&self) -> bool {
        self.depth == 0 && !self.in_block_comment && !self.in_template && self.template_exprs.is_empty()
    }

    fn line(&mut self, text: &str) {
        let chars: Vec<char> = text.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if self.in_block_comment {
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    self.in_block_comment = false;
                    i += 1;
                }
                i += 1;
                continue;
            }
            if self.in_template {
                match c {
                    '\\' => i += 1,
                    '`' => {
                        self.in_template = false;
                        self.previous = Some('`');
                    }
                    '$' if chars.get(i + 1) == Some(&'{') => {
                        self.template_exprs.push(self.depth);
                        self.depth += 1;
                        self.in_template = false;
                        self.previous = Some('{');
                        i += 1;
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            match c {
                '\'' | '"' => {
                    i += 1;
                    while i < chars.len() && chars[i] != c {
                        if chars[i] == '\\' {
                            i += 1;
                        }
                        i += 1;
                    }
                    self.previous = Some(c);
                }
                '`' => {
                    self.in_template = true;
                }
                '/' if chars.get(i + 1) == Some(&'/') => return,
                '/' if chars.get(i + 1) == Some(&'*') => {
                    self.in_block_comment = true;
                    i += 1;
                }
                '/' if self.previous.is_none_or(|p| "(,=:[!&|?{};+-*%<>~^".contains(p)) => {
                    // A regex literal: skip to the closing slash (not inside a class).
                    i += 1;
                    let mut in_class = false;
                    while i < chars.len() {
                        match chars[i] {
                            '\\' => i += 1,
                            '[' => in_class = true,
                            ']' => in_class = false,
                            '/' if !in_class => break,
                            _ => {}
                        }
                        i += 1;
                    }
                    self.previous = Some('/');
                }
                '(' | '[' | '{' => {
                    self.depth += 1;
                    self.previous = Some(c);
                }
                ')' | ']' | '}' => {
                    self.depth = self.depth.saturating_sub(1);
                    if c == '}' && self.template_exprs.last() == Some(&self.depth) {
                        self.template_exprs.pop();
                        self.in_template = true;
                    }
                    self.previous = Some(c);
                }
                c if c.is_whitespace() => {}
                c => self.previous = Some(c),
            }
            i += 1;
        }
    }
}

/// Rewrites the line commands of `script` (see the module docs). `show` and
/// `it` as the last statement hand their result to the host; elsewhere they
/// print.
pub fn rewrite_line_commands(script: &str) -> Rewritten {
    let patterns = patterns();
    let lines: Vec<&str> = script.split('\n').collect();
    let mut scan = Scan::default();
    let mut out = String::with_capacity(script.len() + 32);
    let mut commands = 0;
    for (index, raw_line) in lines.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let (line, carriage) = match raw_line.strip_suffix('\r') {
            Some(stripped) => (stripped, "\r"),
            None => (*raw_line, ""),
        };
        let is_last = lines[index + 1..].iter().all(|rest| rest.trim().is_empty());
        let replacement = scan.at_top_level().then(|| command_call(patterns, line, is_last)).flatten();
        match replacement {
            Some(call) => {
                commands += 1;
                out.push_str(&call);
                out.push_str(carriage);
                scan.previous = Some(';');
            }
            None => {
                out.push_str(raw_line);
                scan.line(line);
            }
        }
    }
    Rewritten {
        code: out,
        commands,
    }
}

fn command_call(patterns: &Patterns, line: &str, is_last: bool) -> Option<String> {
    if let Some(found) = patterns.use_command.captures(line) {
        let name = found.iter().skip(1).flatten().next()?.as_str();
        return Some(format!("__shell.use({});", json_string(name)));
    }
    if let Some(found) = patterns.show.captures(line) {
        return Some(format!("__shell.show({}, {is_last});", json_string(&found[1])));
    }
    patterns
        .iterate
        .is_match(line)
        .then(|| format!("__shell.it({is_last});"))
}

fn json_string(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(script: &str) -> String {
        rewrite_line_commands(script).code
    }

    #[test]
    fn use_show_and_it_become_calls_on_their_own_line() {
        assert_eq!(code("use app"), r#"__shell.use("app");"#);
        assert_eq!(code("  use  my-db ;"), r#"__shell.use("my-db");"#);
        assert_eq!(code("use 'odd name'"), r#"__shell.use("odd name");"#);
        assert_eq!(code("show collections"), "__shell.show(\"collections\", true);");
        assert_eq!(code("it"), "__shell.it(true);");
        assert_eq!(code("it;"), "__shell.it(true);");
    }

    #[test]
    fn commands_in_the_middle_keep_line_numbers_and_know_if_they_are_last() {
        let rewritten = rewrite_line_commands("var a = 1;\nuse app\nshow dbs\ndb.c.find()\nshow collections\n\n");
        assert_eq!(rewritten.commands, 3);
        let lines: Vec<&str> = rewritten.code.split('\n').collect();
        assert_eq!(lines.len(), 7);
        assert_eq!(lines[0], "var a = 1;");
        assert_eq!(lines[1], r#"__shell.use("app");"#);
        assert_eq!(lines[2], "__shell.show(\"dbs\", false);");
        assert_eq!(lines[3], "db.c.find()");
        assert_eq!(lines[4], "__shell.show(\"collections\", true);");
    }

    #[test]
    fn strings_comments_templates_regexes_and_brackets_are_left_alone() {
        for script in [
            "var s = 'a\\' b';\n",
            "var t = `line one\nuse app\nshow dbs\nit`;",
            "/* start\nuse app\nshow dbs\n*/\nvar x = 1;",
            "// use app\nvar x = 1;",
            "db.c.find({\nuse: 1,\nshow: 2\n})",
            "var s = \"multi\" +\n  \"use app\";",
            "var r = /'/;\nvar x = 1;",
            "var a = [\n it\n];",
            "var t = `a ${ x ? `use` : 'y' } b`;",
            "show = 1;\nuse(2);\nit(3);\nuseful\nshowy dbs",
            "use app extra words",
            "show a b",
        ] {
            assert_eq!(rewrite_line_commands(script).commands, 0, "{script:?}");
            assert_eq!(code(script), script);
        }
    }

    #[test]
    fn a_command_after_a_string_with_a_quote_in_a_comment_still_counts() {
        let rewritten = rewrite_line_commands("// it's a comment with a quote\nuse app");
        assert_eq!(rewritten.commands, 1);
    }

    #[test]
    fn windows_line_endings_survive() {
        assert_eq!(code("use app\r\nvar x = 1;\r\n"), "__shell.use(\"app\");\r\nvar x = 1;\r\n");
    }
}
