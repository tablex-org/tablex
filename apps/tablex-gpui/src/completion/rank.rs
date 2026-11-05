//! Matching and bounded top-N ranking. Candidates are borrowed (`&'static`
//! vocabulary, catalog names), so scanning a 10k-table schema allocates only
//! the small result vector; owned strings are built for the survivors.

use std::borrow::Cow;

use super::Kind;

/// Shorter queries only match by prefix or word start; fuzzy subsequence
/// matching on 1-2 characters is mostly noise (`su` ~ `LAST_VALUE`).
const MIN_SUBSEQUENCE: usize = 3;

/// How well a query matches a name; higher is better.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Tier {
    Subsequence = 1,
    WordPrefix = 2,
    Prefix = 3,
    Exact = 4,
}

fn fold(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

fn starts_with_folded(hay: &str, needle: &str) -> bool {
    let mut hay = hay.chars();
    needle
        .chars()
        .all(|n| hay.next().is_some_and(|h| fold(h) == fold(n)))
}

/// Byte length of the first `count` characters of `text`.
pub(super) fn prefix_bytes(text: &str, count: usize) -> usize {
    text.char_indices()
        .nth(count)
        .map_or(text.len(), |(index, _)| index)
}

fn is_word_start(previous: char, current: char) -> bool {
    !previous.is_alphanumeric() || (previous.is_lowercase() && current.is_uppercase())
}

fn is_word_start_ascii(previous: u8, current: u8) -> bool {
    !previous.is_ascii_alphanumeric()
        || (previous.is_ascii_lowercase() && current.is_ascii_uppercase())
}

/// Byte-level matcher for the common all-ASCII case: no decoding, no allocation.
fn match_tier_ascii(name: &[u8], query: &[u8]) -> Option<Tier> {
    if name.len() < query.len() {
        return None;
    }
    if name[..query.len()].eq_ignore_ascii_case(query) {
        return Some(if name.len() == query.len() {
            Tier::Exact
        } else {
            Tier::Prefix
        });
    }
    let word_prefix = (1..=name.len() - query.len()).any(|index| {
        is_word_start_ascii(name[index - 1], name[index])
            && name[index..index + query.len()].eq_ignore_ascii_case(query)
    });
    if word_prefix {
        return Some(Tier::WordPrefix);
    }
    if query.iter().skip_while(|&&byte| byte == b'$').count() >= MIN_SUBSEQUENCE {
        let mut wanted = query.iter();
        let mut next = wanted.next();
        for byte in name {
            if next.is_some_and(|q| q.eq_ignore_ascii_case(byte)) {
                next = wanted.next();
            }
        }
        if next.is_none() {
            return Some(Tier::Subsequence);
        }
    }
    None
}

/// Case-insensitive: prefix > word-prefix (`user_id` for `id`) > subsequence.
pub(super) fn match_tier(name: &str, query: &str) -> Option<Tier> {
    if query.is_empty() {
        return Some(Tier::Prefix);
    }
    if name.is_ascii() && query.is_ascii() {
        return match_tier_ascii(name.as_bytes(), query.as_bytes());
    }
    if starts_with_folded(name, query) {
        let exact = name.chars().count() == query.chars().count();
        return Some(if exact { Tier::Exact } else { Tier::Prefix });
    }
    let mut previous = None;
    for (index, current) in name.char_indices() {
        if previous.is_some_and(|previous| is_word_start(previous, current))
            && starts_with_folded(&name[index..], query)
        {
            return Some(Tier::WordPrefix);
        }
        previous = Some(current);
    }
    if query.trim_start_matches('$').chars().count() >= MIN_SUBSEQUENCE {
        let mut query = query.chars().map(fold).peekable();
        for c in name.chars().map(fold) {
            if query.peek() == Some(&c) {
                query.next();
            }
        }
        if query.peek().is_none() {
            return Some(Tier::Subsequence);
        }
    }
    None
}

/// How the accepted text is derived from a candidate.
#[derive(Clone, Debug)]
pub(super) enum Insert<'a> {
    /// A table / column / schema name: quoted on demand, qualified by
    /// `namespace` when the candidate lives outside the default one.
    Identifier,
    /// Keyword or function: follows the case the user typed, then `suffix`.
    Word { suffix: &'static str },
    /// A document key: `name: `, `"name": ` in strict JSON, or just `name`
    /// when the cursor is already inside a quoted key.
    Property,
    /// Inserted verbatim.
    Text(Cow<'a, str>),
}

#[derive(Clone, Debug)]
pub(super) struct Candidate<'a> {
    pub name: &'a str,
    pub namespace: Option<&'a str>,
    pub kind: Kind,
    pub detail: Cow<'a, str>,
    pub insert: Insert<'a>,
    /// Second string the query may match (snippet shorthand).
    pub alias: Option<&'static str>,
}

impl<'a> Candidate<'a> {
    pub(super) fn new(name: &'a str, kind: Kind, detail: impl Into<Cow<'a, str>>) -> Self {
        Self {
            name,
            namespace: None,
            kind,
            detail: detail.into(),
            insert: Insert::Identifier,
            alias: None,
        }
    }

    pub(super) fn word(name: &'a str, kind: Kind, detail: &'a str, suffix: &'static str) -> Self {
        Self {
            insert: Insert::Word { suffix },
            ..Self::new(name, kind, detail)
        }
    }

    pub(super) fn text(
        name: &'a str,
        kind: Kind,
        detail: impl Into<Cow<'a, str>>,
        insert: impl Into<Cow<'a, str>>,
    ) -> Self {
        Self {
            insert: Insert::Text(insert.into()),
            ..Self::new(name, kind, detail)
        }
    }

    pub(super) fn in_namespace(mut self, namespace: Option<&'a str>) -> Self {
        self.namespace = namespace;
        self
    }
}

pub(super) struct Ranked<'a> {
    pub score: u32,
    pub tier: Tier,
    seq: u32,
    pub candidate: Candidate<'a>,
}

/// Keeps the best `cap` candidates of an arbitrarily long stream: entries are
/// buffered and trimmed in batches, so the cost stays linear.
pub(super) struct Ranker<'a, 'q> {
    query: &'q str,
    cap: usize,
    entries: Vec<Ranked<'a>>,
    seq: u32,
    floor: Option<u32>,
}

impl<'a, 'q> Ranker<'a, 'q> {
    pub(super) fn new(query: &'q str, cap: usize) -> Self {
        Self {
            query,
            cap,
            entries: Vec::with_capacity(cap * 2),
            seq: 0,
            floor: None,
        }
    }

    /// Offers a candidate; `boost` (0..4096) orders kinds against each other
    /// within one match tier.
    pub(super) fn offer(&mut self, candidate: Candidate<'a>, boost: u16) {
        self.seq += 1;
        let tier = match_tier(candidate.name, self.query)
            .into_iter()
            .chain(
                candidate
                    .alias
                    .and_then(|alias| match_tier(alias, self.query)),
            )
            .max();
        let Some(tier) = tier else { return };
        // Shorter names first once something was typed; a bare `$` keeps table order.
        let brevity = if self.query.trim_start_matches('$').is_empty() {
            0
        } else {
            255 - candidate.name.len().min(255) as u32
        };
        let score = (tier as u32) << 24 | u32::from(boost.min(4095)) << 12 | brevity;
        if self.floor.is_some_and(|floor| score <= floor) {
            return;
        }
        self.entries.push(Ranked {
            score,
            tier,
            seq: self.seq,
            candidate,
        });
        if self.entries.len() >= self.cap * 4 {
            self.trim();
        }
    }

    fn trim(&mut self) {
        self.entries
            .sort_unstable_by(|a, b| b.score.cmp(&a.score).then(a.seq.cmp(&b.seq)));
        self.entries.truncate(self.cap);
        if self.entries.len() == self.cap {
            self.floor = self.entries.last().map(|entry| entry.score);
        }
    }

    pub(super) fn finish(mut self) -> Vec<Ranked<'a>> {
        self.trim();
        self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_order_exact_prefix_word_subsequence() {
        assert_eq!(match_tier("select", "SELECT"), Some(Tier::Exact));
        assert_eq!(match_tier("SUBSTRING", "sub"), Some(Tier::Prefix));
        assert_eq!(match_tier("user_id", "id"), Some(Tier::WordPrefix));
        assert_eq!(match_tier("createdAt", "at"), Some(Tier::WordPrefix));
        assert_eq!(
            match_tier("customer_orders", "cord"),
            Some(Tier::Subsequence)
        );
        assert_eq!(match_tier("abc", "z"), None);
        assert_eq!(match_tier("a", ""), Some(Tier::Prefix));
        // The non-ASCII path agrees with the ASCII one.
        assert_eq!(match_tier("Ünïcode_id", "id"), Some(Tier::WordPrefix));
        assert_eq!(match_tier("Ünï", "ün"), Some(Tier::Prefix));
    }

    #[test]
    fn queries_under_three_characters_never_match_by_subsequence() {
        assert_eq!(match_tier("orders", "d"), None);
        assert_eq!(match_tier("last_value", "su"), None);
        assert_eq!(match_tier("substring", "sbs"), Some(Tier::Subsequence));
        assert_eq!(match_tier("Ünïcode_value", "uv"), None);
    }

    #[test]
    fn ranker_keeps_best_in_stable_order_across_trims() {
        let names: Vec<String> = (0..1000).map(|i| format!("t{i:04}")).collect();
        let mut ranker = Ranker::new("", 5);
        for name in &names {
            ranker.offer(Candidate::new(name, Kind::Table, "table"), 100);
        }
        let top: Vec<_> = ranker
            .finish()
            .into_iter()
            .map(|r| r.candidate.name.to_owned())
            .collect();
        assert_eq!(top, ["t0000", "t0001", "t0002", "t0003", "t0004"]);
    }
}
