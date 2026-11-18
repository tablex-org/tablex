//! Highlighting of the SQL statements of a MongoDB script tab. The editor keeps the language
//! "javascript"; the statements `mongo_sql::rewrite_script` rewrites (the ones `sql_start` accepts)
//! get SQL colors as text decorations, which paint over the JavaScript ones.

use gpui::{Hsla, HighlightStyle};
use gpui_component::highlighter::SyntaxHighlighter;
use gpui_component::input::HighlightStyleResolver;
use gpui_component::Rope;
use std::ops::Range;

/// A script bigger than this is left to the JavaScript highlighting (it bounds the parse and the
/// decorations).
pub const MAX_SCRIPT_BYTES: usize = 1 << 20;

/// Byte ranges of the statements of `script` that are SQL (the rule `rewrite_script` uses).
pub fn sql_statement_ranges(script: &str) -> Vec<Range<usize>> {
    // Cheap exit for the usual script: no `select` anywhere, nothing to split.
    if script.len() > MAX_SCRIPT_BYTES
        || !script.as_bytes().windows(6).any(|word| word.eq_ignore_ascii_case(b"select"))
    {
        return Vec::new();
    }
    crate::mongo_script::statement_ranges(script)
        .into_iter()
        .filter(|range| crate::mongo_sql::sql_start(&script[range.clone()]).is_some())
        .collect()
}

/// Non-overlapping runs that cover every SQL statement of `script` completely, colored by `theme`;
/// text the SQL grammar leaves plain gets `foreground`, so no JavaScript color shows through.
pub fn sql_runs(
    script: &str,
    theme: &dyn HighlightStyleResolver,
    foreground: Hsla,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let statements = sql_statement_ranges(script);
    if statements.is_empty() {
        return Vec::new();
    }
    // The statements alone, in place (other bytes blanked, so offsets stay), each closed by a `;`
    // the grammar needs between statements. Blanking bytes keeps UTF-8 valid.
    let mut masked = vec![b' '; script.len()];
    for (at, byte) in script.bytes().enumerate() {
        if byte == b'\n' {
            masked[at] = b'\n';
        }
    }
    for range in &statements {
        masked[range.clone()].copy_from_slice(&script.as_bytes()[range.clone()]);
        if let Some(end) = masked.get_mut(range.end) {
            *end = b';';
        }
    }
    let Ok(masked) = String::from_utf8(masked) else {
        return Vec::new();
    };
    let mut highlighter = SyntaxHighlighter::new("sql");
    highlighter.update(None, &Rope::from_str(&masked), None);
    let styles = highlighter.styles(&(0..masked.len()), theme);

    let plain = HighlightStyle { color: Some(foreground), ..HighlightStyle::default() };
    let mut runs = Vec::with_capacity(styles.len());
    for statement in statements {
        let mut at = statement.start;
        // `styles` is ordered and does not overlap: jump to the first run inside the statement.
        let first = styles.partition_point(|(range, _)| range.end <= statement.start);
        for (range, mut style) in styles[first..].iter().cloned() {
            if range.start >= statement.end {
                break;
            }
            let (start, end) = (range.start.max(at), range.end.min(statement.end));
            if start >= end {
                continue;
            }
            if start > at {
                runs.push((at..start, plain));
            }
            style.color.get_or_insert(foreground);
            runs.push((start..end, style));
            at = end;
        }
        if at < statement.end {
            runs.push((at..statement.end, plain));
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::theme::Theme;

    fn color_at(runs: &[(Range<usize>, HighlightStyle)], at: usize) -> Option<Hsla> {
        runs.iter().find(|(range, _)| range.contains(&at)).and_then(|(_, style)| style.color)
    }

    fn covered(runs: &[(Range<usize>, HighlightStyle)]) -> Vec<Range<usize>> {
        let mut merged: Vec<Range<usize>> = Vec::new();
        for (range, _) in runs {
            match merged.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => merged.push(range.clone()),
            }
        }
        merged
    }

    const MIXED: &str = "db.shop.find({ status: \"open\" });\n\nSELECT status, COUNT(*) FROM \"shop\".\"orders\" WHERE total > 10;\ndb.orders.countDocuments()";

    #[test]
    fn only_sql_statements_are_found() {
        let ranges = sql_statement_ranges(MIXED);
        assert_eq!(ranges.len(), 1);
        assert!(MIXED[ranges[0].clone()].starts_with("SELECT"));
        assert!(MIXED[ranges[0].clone()].ends_with("> 10"));
        assert!(sql_statement_ranges("db.shop.find({})").is_empty());
        // `select` inside a shell statement is not an SQL statement.
        assert!(sql_statement_ranges("db.shop.find({ kind: \"select\" })").is_empty());
        assert!(sql_statement_ranges("").is_empty());
    }

    #[test]
    fn a_comment_line_before_select_is_not_part_of_the_statement() {
        let script = "-- open orders\nselect * from orders";
        assert_eq!(sql_statement_ranges(script), vec![15..script.len()]);
    }

    #[gpui::test]
    fn sql_keywords_get_a_color_other_than_identifiers(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let theme = Theme::global(cx);
            let foreground = theme.foreground;
            let runs = sql_runs(MIXED, theme.highlight_theme.as_ref(), foreground);
            let at = |word: &str| MIXED.find(word).unwrap();
            let statement = sql_statement_ranges(MIXED).remove(0);
            // Exactly the SQL statement is covered, with no gap.
            assert_eq!(covered(&runs), vec![statement.clone()]);
            assert!(runs.iter().all(|(range, style)| !range.is_empty() && style.color.is_some()));
            for pair in runs.windows(2) {
                assert!(pair[0].0.end <= pair[1].0.start, "runs overlap: {pair:?}");
            }
            let select = color_at(&runs, at("SELECT")).unwrap();
            assert_eq!(color_at(&runs, at("FROM")), Some(select));
            assert_ne!(Some(select), color_at(&runs, at("status, COUNT")));
            assert_ne!(select, foreground);
            // Nothing is decorated outside it: the shell statements stay JavaScript.
            assert_eq!(color_at(&runs, at("db.shop")), None);
            assert_eq!(color_at(&runs, at("db.orders")), None);
        });
    }

    #[gpui::test]
    fn two_sql_statements_and_non_ascii_text(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let theme = Theme::global(cx);
            let script = "select * from shop where name = 'Đặng'; db.a.find()\n\nselect 1 from orders";
            let runs = sql_runs(script, theme.highlight_theme.as_ref(), theme.foreground);
            assert_eq!(covered(&runs), sql_statement_ranges(script));
            assert_eq!(covered(&runs).len(), 2);
            for (range, _) in &runs {
                assert!(script.is_char_boundary(range.start) && script.is_char_boundary(range.end));
            }
        });
    }

    #[gpui::test]
    fn colors_follow_the_theme_mode(cx: &mut TestAppContext) {
        use gpui_component::theme::ThemeMode;
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
            let keyword = |cx: &gpui::App| {
                let theme = Theme::global(cx);
                let runs = sql_runs(MIXED, theme.highlight_theme.as_ref(), theme.foreground);
                color_at(&runs, MIXED.find("SELECT").unwrap())
            };
            crate::theme::apply(ThemeMode::Dark, cx);
            let dark = keyword(cx);
            crate::theme::apply(ThemeMode::Light, cx);
            assert_ne!(dark, keyword(cx));
        });
    }
}
