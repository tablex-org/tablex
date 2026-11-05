//! Palette matcher: tiered scoring over pre-folded strings and an
//! allocation-free top-K selection.
//!
//! Tiers (a higher tier always beats a lower one; the penalties inside a tier
//! only order its members): exact 500, prefix 400, initials 350, word prefix
//! 300, substring 200, subsequence 100. A title hit is worth +100 over a hit in
//! the scope text (connection / database / keywords), which is never matched
//! as a scattered subsequence. Several
//! whitespace-separated tokens must all match; their scores add up.
//!
//! The per-keystroke path allocates nothing proportional to the index: the
//! character bloom mask rejects most entries with one AND, the survivors are
//! scored in place, and `Ranker` reuses one scratch buffer that is trimmed
//! with `select_nth_unstable` (O(n)) before only the K winners are sorted.

use super::index::{char_bit, fold_char, Entry, Index, Segment};
use std::ops::Range;

const TIER_EXACT: i32 = 500;
const TIER_PREFIX: i32 = 400;
const TIER_INITIALS: i32 = 350;
const TIER_WORD_PREFIX: i32 = 300;
const TIER_SUBSTRING: i32 = 200;
const TIER_SUBSEQUENCE: i32 = 100;
/// Bonus of matching the title instead of the scope text: one tier step, so a
/// query that names an entry beats entries that merely live under that name.
const TITLE_BONUS: i32 = 100;
/// A title score at least this high is a prefix/exact hit that scope text
/// cannot outrank.
const TITLE_DECISIVE: i32 = TIER_PREFIX + TITLE_BONUS - MAX_PENALTY;
/// Cap of the length / gap penalty, kept below one tier step.
const MAX_PENALTY: i32 = 49;
const MAX_TOKENS: usize = 4;

struct Token {
    text: String,
    first: char,
    chars: usize,
    ascii: bool,
}

/// A parsed, folded user query.
pub(super) struct Query {
    tokens: Vec<Token>,
    mask: u64,
}

impl Query {
    pub(super) fn parse(raw: &str) -> Self {
        let mut mask = 0;
        let tokens = raw
            .split_whitespace()
            .take(MAX_TOKENS)
            .map(|word| {
                let text: String = word.chars().map(fold_char).collect();
                mask |= text.chars().fold(0, |bits, c| bits | char_bit(c));
                Token {
                    first: text.chars().next().unwrap_or(' '),
                    chars: text.chars().count(),
                    ascii: text.is_ascii(),
                    text,
                }
            })
            .collect();
        Self { tokens, mask }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

fn mark(ranges: &mut Option<&mut Vec<Range<usize>>>, range: Range<usize>) {
    if let Some(ranges) = ranges.as_deref_mut() {
        ranges.push(range);
    }
}

/// Scores `token` against one folded field; records the matched byte ranges
/// (of the folded text) into `ranges` when given. Allocation-free unless
/// `ranges` is given.
fn field_score(
    hay: &str,
    starts: u64,
    token: &Token,
    mut ranges: Option<&mut Vec<Range<usize>>>,
    allow_subsequence: bool,
) -> Option<i32> {
    let query = token.text.as_str();
    if hay.len() < query.len() {
        return None;
    }
    let excess = ((hay.len() - query.len()) as i32).min(MAX_PENALTY);
    if hay == query {
        mark(&mut ranges, 0..hay.len());
        return Some(TIER_EXACT);
    }
    if hay.starts_with(query) {
        mark(&mut ranges, 0..query.len());
        return Some(TIER_PREFIX - excess);
    }
    if token.chars >= 2 && starts.count_ones() as usize == token.chars
        && initials_match(hay, starts, query)
    {
        if ranges.is_some() {
            let mut bits = starts;
            while bits != 0 {
                let offset = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                let len = hay[offset..].chars().next().map_or(1, char::len_utf8);
                mark(&mut ranges, offset..offset + len);
            }
        }
        return Some(TIER_INITIALS - excess);
    }
    // Substring: prefer an occurrence that starts a word.
    let mut inside = None;
    let mut from = 0;
    while let Some(relative) = hay[from..].find(token.first) {
        let position = from + relative;
        if hay[position..].starts_with(query) {
            if position < 64 && starts & (1 << position) != 0 {
                mark(&mut ranges, position..position + query.len());
                return Some(TIER_WORD_PREFIX - excess);
            }
            inside.get_or_insert(position);
        }
        from = position + token.first.len_utf8();
    }
    if let Some(position) = inside {
        mark(&mut ranges, position..position + query.len());
        return Some(TIER_SUBSTRING - excess);
    }
    // Scope text (connection / keywords) is long: scattered letters would
    // match nearly anything, so only titles are matched as subsequences.
    if !allow_subsequence || token.chars < 2 {
        return None;
    }
    let (first, last) = subsequence_span(hay, token)?;
    if ranges.is_some() {
        subsequence_ranges(hay, query, &mut ranges);
    }
    let gap = ((last - first).saturating_sub(query.len()) as i32).min(MAX_PENALTY);
    Some(TIER_SUBSEQUENCE - gap)
}

/// Whether the word-start chars of `hay` spell exactly `query`.
fn initials_match(hay: &str, starts: u64, query: &str) -> bool {
    let mut wanted = query.chars();
    let mut bits = starts;
    while bits != 0 {
        let offset = bits.trailing_zeros() as usize;
        bits &= bits - 1;
        let Some(c) = hay.get(offset..).and_then(|rest| rest.chars().next()) else {
            return false;
        };
        if wanted.next() != Some(c) {
            return false;
        }
    }
    wanted.next().is_none()
}

/// Greedy left-most subsequence span: (first byte, end byte). No storage.
fn subsequence_span(hay: &str, token: &Token) -> Option<(usize, usize)> {
    let mut first = None;
    if token.ascii {
        // ASCII bytes never occur inside a multi-byte sequence.
        let mut needed = token.text.bytes();
        let mut next = needed.next()?;
        for (offset, byte) in hay.bytes().enumerate() {
            if byte == next {
                first.get_or_insert(offset);
                match needed.next() {
                    Some(following) => next = following,
                    None => return Some((first?, offset + 1)),
                }
            }
        }
    } else {
        let mut needed = token.text.chars();
        let mut next = needed.next()?;
        for (offset, c) in hay.char_indices() {
            if c == next {
                first.get_or_insert(offset);
                match needed.next() {
                    Some(following) => next = following,
                    None => return Some((first?, offset + c.len_utf8())),
                }
            }
        }
    }
    None
}

/// Highlight ranges of the same greedy subsequence (highlight path only).
fn subsequence_ranges(hay: &str, query: &str, ranges: &mut Option<&mut Vec<Range<usize>>>) {
    let mut needed = query.chars().peekable();
    for (offset, c) in hay.char_indices() {
        if needed.peek() == Some(&c) {
            needed.next();
            mark(ranges, offset..offset + c.len_utf8());
            if needed.peek().is_none() {
                return;
            }
        }
    }
}

const NO_SCORE: i32 = i32::MIN;

/// Per-scope scores of every query token, computed once per segment per
/// keystroke: thousands of objects share a handful of scopes, so the scope
/// text is matched per scope, never per entry.
fn scope_scores(segment: &Segment, query: &Query, out: &mut Vec<i32>) {
    out.clear();
    for scope in segment.scopes() {
        for token in &query.tokens {
            out.push(
                field_score(scope.folded(), scope.word_starts(), token, None, false)
                    .unwrap_or(NO_SCORE),
            );
        }
    }
}

/// Score of `entry` for the whole query, or `None` when any token misses.
fn entry_score(entry: &Entry, query: &Query, scope_scores: &[i32]) -> Option<i32> {
    if query.mask & !entry.mask() != 0 {
        return None;
    }
    let scoped = &scope_scores[entry.scope_ix() * query.tokens.len()..];
    let mut total = entry.kind.bias();
    for (token, scoped) in query.tokens.iter().zip(scoped) {
        let title = field_score(entry.folded(), entry.word_starts(), token, None, true)
            .map(|score| score + TITLE_BONUS);
        let best = match title {
            // An exact/prefix title hit cannot be beaten by scope text.
            Some(score) if score >= TITLE_DECISIVE => score,
            other => other.max((*scoped != NO_SCORE).then_some(*scoped))?,
        };
        total += best;
    }
    Some(total)
}

/// Byte ranges of `entry.title` that the title matched (for highlighting),
/// sorted and merged.
pub(super) fn highlight_ranges(entry: &Entry, query: &Query) -> Vec<Range<usize>> {
    let mut folded_ranges = Vec::new();
    for token in &query.tokens {
        field_score(entry.folded(), entry.word_starts(), token, Some(&mut folded_ranges), true);
    }
    if folded_ranges.is_empty() {
        return folded_ranges;
    }
    folded_ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(folded_ranges.len());
    for range in folded_ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    if entry.title.is_ascii() {
        return merged;
    }
    // Folded and original share char positions, not necessarily byte offsets.
    let folded = entry.folded();
    let char_of = |offset: usize| folded[..offset].chars().count();
    let byte_of = |char_index: usize| {
        entry
            .title
            .char_indices()
            .nth(char_index)
            .map_or(entry.title.len(), |(offset, _)| offset)
    };
    merged
        .into_iter()
        .map(|range| byte_of(char_of(range.start))..byte_of(char_of(range.end)))
        .collect()
}

/// One ranked entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Hit {
    pub(super) segment: u8,
    pub(super) entry: u32,
    pub(super) score: i32,
}

/// Reusable top-K selector.
#[derive(Default)]
pub(super) struct Ranker {
    /// `score << 32 | !position`: a plain descending sort is "best score,
    /// then earliest entry", and one u64 compare replaces a tuple compare.
    keys: Vec<u64>,
    scope_scores: Vec<i32>,
}

impl Ranker {
    /// Best `limit` matches, best first, ties by index order.
    pub(super) fn top(&mut self, index: &Index, query: &Query, limit: usize, out: &mut Vec<Hit>) {
        out.clear();
        if query.is_empty() || limit == 0 {
            return;
        }
        self.keys.clear();
        let mut base = 0u32;
        let mut bases = [0u32; 8];
        for (segment_ix, segment) in index.segments.iter().enumerate().take(bases.len()) {
            bases[segment_ix] = base;
            scope_scores(segment, query, &mut self.scope_scores);
            for (entry_ix, entry) in segment.entries.iter().enumerate() {
                if let Some(score) = entry_score(entry, query, &self.scope_scores) {
                    let position = base + entry_ix as u32;
                    self.keys
                        .push((score.max(0) as u64) << 32 | u64::from(u32::MAX - position));
                }
            }
            base += segment.entries.len() as u32;
        }
        if self.keys.len() > limit {
            self.keys
                .select_nth_unstable_by(limit - 1, |a, b| b.cmp(a));
            self.keys.truncate(limit);
        }
        self.keys.sort_unstable_by(|a, b| b.cmp(a));
        out.extend(self.keys.iter().map(|key| {
            let position = u32::MAX - (*key as u32);
            let segment = bases
                .iter()
                .zip(&index.segments)
                .rposition(|(start, segment)| {
                    *start <= position && !segment.entries.is_empty()
                })
                .unwrap_or(0);
            Hit {
                segment: segment as u8,
                entry: position - bases[segment],
                score: (*key >> 32) as i32,
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::{Command, Kind, SegmentBuilder};
    use super::*;
    use crate::app::tab_bar::TabIcon;
    use std::sync::Arc;

    const ICON: TabIcon = TabIcon::Plain("icons/table.svg");

    fn index_of(titles: &[(&str, &str)]) -> Index {
        let mut builder = SegmentBuilder::new();
        for (title, detail) in titles {
            let scope = builder.scope(detail, "", "c", "", "");
            builder.push(Kind::Table, ICON, scope, title, None);
        }
        Index {
            segments: vec![Arc::new(builder.finish())],
        }
    }

    fn rank(index: &Index, query: &str, limit: usize) -> Vec<String> {
        let mut hits = Vec::new();
        Ranker::default().top(index, &Query::parse(query), limit, &mut hits);
        hits.iter()
            .map(|hit| index.segments[hit.segment as usize].entries[hit.entry as usize].title.to_string())
            .collect()
    }

    #[test]
    fn tiers_order_exact_prefix_initials_word_prefix_substring_subsequence() {
        let index = index_of(&[
            ("my_users_log", "pg"),  // word prefix
            ("xxusersxx", "pg"),     // substring
            ("uxxsxxexxrxxs", "pg"), // subsequence only
            ("users", "pg"),         // exact
            ("users_archive", "pg"), // prefix
        ]);
        assert_eq!(
            rank(&index, "users", 10),
            vec!["users", "users_archive", "my_users_log", "xxusersxx", "uxxsxxexxrxxs"]
        );
    }

    #[test]
    fn initials_rank_below_prefix_and_above_word_prefix() {
        let index = index_of(&[
            ("order_items_summary", "pg"),
            ("ois_report", "pg"),
            ("daily_ois", "pg"),
        ]);
        assert_eq!(
            rank(&index, "ois", 10),
            vec!["ois_report", "order_items_summary", "daily_ois"]
        );
    }

    #[test]
    fn camel_case_boundaries_are_word_starts() {
        let index = index_of(&[("userAccounts", "pg"), ("superaccount", "pg")]);
        assert_eq!(rank(&index, "acc", 10), vec!["userAccounts", "superaccount"]);
    }

    #[test]
    fn shorter_titles_win_inside_a_tier() {
        let index = index_of(&[("orders_archive_2020", "pg"), ("orders_old", "pg"), ("orders", "pg")]);
        assert_eq!(
            rank(&index, "ord", 10),
            vec!["orders", "orders_old", "orders_archive_2020"]
        );
    }

    #[test]
    fn missing_characters_never_match_and_single_char_has_no_subsequence() {
        let index = index_of(&[("orders", "pg"), ("users", "pg")]);
        assert!(rank(&index, "ordz", 10).is_empty());
        assert!(rank(&index, "zz", 10).is_empty());
        // Typos are not fuzzy-fixed: "oders" is not a subsequence-free typo
        // match, but is still a subsequence of "orders".
        assert_eq!(rank(&index, "oders", 10), vec!["orders"]);
        assert!(rank(&index, "ordres", 10).is_empty());
    }

    #[test]
    fn scope_text_matches_with_lower_priority_and_tokens_combine() {
        let index = index_of(&[
            ("orders", "prod-pg · shop.public"),
            ("orders", "dev-pg · shop.public"),
            ("prod_report", "dev-pg · shop.public"),
        ]);
        // Title hit on "prod_report" outranks scope-only hits.
        assert_eq!(rank(&index, "prod", 10)[0], "prod_report");
        // Two tokens: both must match, title + scope.
        let hits = rank(&index, "orders dev", 10);
        assert_eq!(hits, vec!["orders"]);
        let mut raw = Vec::new();
        Ranker::default().top(&index, &Query::parse("orders dev"), 10, &mut raw);
        assert_eq!(raw[0].entry, 1);
    }

    #[test]
    fn ties_keep_index_order_and_limit_is_applied() {
        let index = index_of(&[("alpha", "x"), ("alpha2", "x"), ("alpha3", "x"), ("alpha4", "x")]);
        assert_eq!(rank(&index, "alpha", 2), vec!["alpha", "alpha2"]);
        assert_eq!(rank(&index, "alpha", 3).len(), 3);
    }

    #[test]
    fn kind_bias_breaks_ties_but_not_tiers() {
        let mut builder = SegmentBuilder::new();
        let scope = builder.scope("pg", "", "c", "", "");
        builder.push(Kind::Table, ICON, scope, "users", None);
        builder.push(Kind::Tab, ICON, scope, "users", Some(Command::ActivateTab(1)));
        builder.push(Kind::Table, ICON, scope, "users_x", None);
        let index = Index {
            segments: vec![Arc::new(builder.finish())],
        };
        let mut hits = Vec::new();
        Ranker::default().top(&index, &Query::parse("users"), 10, &mut hits);
        let kinds: Vec<_> = hits
            .iter()
            .map(|hit| index.segments[0].entries[hit.entry as usize].kind)
            .collect();
        assert_eq!(kinds, vec![Kind::Tab, Kind::Table, Kind::Table]);
    }

    #[test]
    fn highlights_cover_the_matched_title_chars() {
        let index = index_of(&[("user_accounts", "pg"), ("café_menu", "pg"), ("ordering", "pg")]);
        let entry = |i: usize| &index.segments[0].entries[i];
        assert_eq!(highlight_ranges(entry(0), &Query::parse("acc")), vec![5..8]);
        assert_eq!(highlight_ranges(entry(1), &Query::parse("men")), vec![6..9]);
        // Subsequence: each matched char separately.
        assert_eq!(
            highlight_ranges(entry(2), &Query::parse("odg")),
            vec![0..1, 2..3, 7..8]
        );
        // Scope-only matches highlight nothing in the title.
        assert!(highlight_ranges(entry(0), &Query::parse("pg")).is_empty());
    }

    #[test]
    fn non_ascii_titles_map_highlights_back_to_original_bytes() {
        // 'İ' lower-cases to two chars; fold keeps it as-is (1 char in, 1 out).
        let index = index_of(&[("İstanbulTable", "pg")]);
        let entry = &index.segments[0].entries[0];
        let ranges = highlight_ranges(entry, &Query::parse("table"));
        assert_eq!(ranges.len(), 1);
        assert_eq!(&entry.title[ranges[0].clone()], "Table");
    }

    #[test]
    fn empty_query_ranks_nothing() {
        let index = index_of(&[("a", "b")]);
        assert!(rank(&index, "   ", 5).is_empty());
    }

    /// 50k-entry ranking per keystroke; run in release with
    /// `cargo test -p tablex-gpui --release palette_bench -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark: run in --release"]
    fn palette_bench_50k_keystrokes() {
        let mut builder = SegmentBuilder::with_capacity(60_000);
        let words = [
            "user", "order", "item", "invoice", "payment", "audit", "log", "session", "product",
            "address", "customer", "shipment", "refund", "ledger", "event", "metric",
        ];
        let mut state = 0x2545F491u32;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as usize % bound
        };
        let mut scopes = Vec::new();
        for schema in 0..40 {
            scopes.push(builder.scope(
                &format!("warehouse-prod · db{}.schema_{schema}", schema % 5),
                "",
                "c",
                &format!("db{}", schema % 5),
                &format!("schema_{schema}"),
            ));
        }
        for i in 0..50_000usize {
            let title = format!(
                "{}_{}_{}{}",
                words[next(words.len())],
                words[next(words.len())],
                words[next(words.len())],
                i
            );
            let scope = scopes[i * scopes.len() / 50_000];
            builder.push(Kind::Table, ICON, scope, &title, None);
        }
        let index = Index {
            segments: vec![Arc::new(builder.finish())],
        };
        assert!(index.len() >= 50_000);
        let mut ranker = Ranker::default();
        let mut hits = Vec::new();
        for query in ["u", "us", "usr", "user", "user_ord", "pay ledger", "xq", "ois"] {
            let parsed = Query::parse(query);
            ranker.top(&index, &parsed, 64, &mut hits); // warm the scratch buffer
            // Min and median of many runs: a loaded CI/dev machine inflates
            // the mean, but the fastest run is the uncontended cost.
            let mut samples: Vec<std::time::Duration> = (0..200)
                .map(|_| {
                    let started = std::time::Instant::now();
                    ranker.top(&index, &parsed, 64, &mut hits);
                    started.elapsed()
                })
                .collect();
            samples.sort();
            let (best, median) = (samples[0], samples[samples.len() / 2]);
            println!(
                "palette_bench query {query:>10?} entries={} hits={:>3} -> min {best:?} median {median:?} per keystroke",
                index.len(),
                hits.len(),
            );
        }
    }
}
