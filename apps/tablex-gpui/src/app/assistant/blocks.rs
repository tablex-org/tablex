//! Cutting a growing answer into blocks, so a long answer is several transcript rows.
//!
//! One Markdown view lays out and paints all of its text whenever it changes, and the transcript
//! virtualises rows, not the inside of a row. A 20 000 character answer in one row therefore costs
//! a layout of 20 000 characters per update. Cut into blocks, the finished blocks are rows that do
//! not change (they keep their text, their parse and their measured height, and are not built at
//! all while off screen) and only the last block, the one still being written, is updated.
//!
//! A cut is made only where Markdown starts a new block with certainty: after a blank line, at
//! the start of a line that is not indented and not a list item (so a list, a loose list and an
//! item's indented continuation stay in one piece), and never inside a fenced code block or a
//! `$$` math block. Cuts are decided on complete lines only and never move, so everything before
//! the last cut is final however the text goes on.
//!
//! Fences follow CommonMark: an opening run of at least three backticks or tildes (or two dollar
//! signs, for math; up to three spaces of indent; the info string of a backtick or dollar fence
//! has no such character, or the line is inline code or inline math), closed only by a bare run
//! of the same character at least as long. Raw HTML is not tracked: the view shows it as text, so
//! a blank line inside an HTML block may start a block of its own like any other. A table has no
//! blank line inside, so a cut at a blank line never splits one.
//!
//! Limit: a reference-style link definition (`[x]: https://...`) or a footnote definition that
//! lands in another block than the use of it does not resolve there, and shows as text.

use std::collections::HashMap;
use std::ops::Range;

/// The blocks of one growing text.
#[derive(Debug, Default)]
pub struct Blocks {
    /// Start offset of each block; the first is 0.
    starts: Vec<usize>,
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A fence line that opens a code or math block: its character and the length of its run.
pub fn opens_fence(line: &str) -> Option<(char, usize)> {
    if indent_of(line) > 3 {
        return None;
    }
    let line = line.trim_start_matches(' ');
    let fence = line
        .chars()
        .next()
        .filter(|c| matches!(c, '`' | '~' | '$'))?;
    let run = line.chars().take_while(|c| *c == fence).count();
    if run < if fence == '$' { 2 } else { 3 } {
        return None;
    }
    // "```code``` text" is an inline span and "$$x$$ text" inline math, not fences.
    if fence != '~' && line[run..].contains(fence) {
        return None;
    }
    Some((fence, run))
}

/// Whether `line` closes the fence `(fence, length)`: only that character, at least as long.
pub fn closes_fence(line: &str, (fence, length): (char, usize)) -> bool {
    if indent_of(line) > 3 {
        return false;
    }
    let line = line.trim_start_matches(' ');
    let run = line.chars().take_while(|c| *c == fence).count();
    run >= length && line[run..].trim().is_empty()
}

fn is_list_item(line: &str) -> bool {
    let rest = line.strip_prefix(['-', '*', '+']).or_else(|| {
        let digits = line.trim_start_matches(|c: char| c.is_ascii_digit());
        (digits.len() < line.len())
            .then(|| digits.strip_prefix(['.', ')']))
            .flatten()
    });
    rest.is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// A line that starts a block of its own after a blank line.
fn starts_block(line: &str) -> bool {
    !line.starts_with([' ', '\t']) && !is_list_item(line)
}

/// Where the scan of a block stands between two lines.
enum Inside {
    Text,
    Fence((char, usize)),
}

impl Blocks {
    /// Looks at the text appended since the last call (`text` is the whole text so far).
    pub fn update(&mut self, text: &str) {
        if self.starts.is_empty() {
            self.starts.push(0);
        }
        let from = *self.starts.last().unwrap_or(&0);
        let mut inside = Inside::Text;
        let mut after_blank = false;
        let mut offset = from;
        let mut cuts = Vec::new();
        for line in text[from..].split_inclusive('\n') {
            // A line without its newline may still grow: no decision on it.
            if !line.ends_with('\n') {
                break;
            }
            let body = line.trim_end_matches(['\n', '\r']);
            match inside {
                Inside::Fence(open) => {
                    if closes_fence(body, open) {
                        inside = Inside::Text;
                    }
                    after_blank = false;
                }
                Inside::Text if body.trim().is_empty() => after_blank = true,
                Inside::Text => {
                    let fence = opens_fence(body);
                    // The first line of the text from `from` is the block's own start.
                    if after_blank && offset > from && (fence.is_some() || starts_block(body)) {
                        cuts.push(offset);
                    }
                    if let Some(open) = fence {
                        inside = Inside::Fence(open);
                    }
                    after_blank = false;
                }
            }
            offset += line.len();
        }
        self.starts.extend(cuts);
    }

    pub fn len(&self) -> usize {
        self.starts.len().max(1)
    }

    /// The byte range of block `index` in a text of `text_len` bytes.
    pub fn range(&self, index: usize, text_len: usize) -> Range<usize> {
        let start = self.starts.get(index).copied().unwrap_or(0);
        let end = self.starts.get(index + 1).copied().unwrap_or(text_len);
        start..end.max(start)
    }

    /// Whether a text of `text_len` bytes is still consistent with these cuts (a shorter text
    /// than the last cut means the text was replaced).
    pub fn fits(&self, text_len: usize) -> bool {
        self.starts.last().copied().unwrap_or(0) <= text_len
    }
}

/// The blocks of every text item, by item index.
pub type BlockMap = HashMap<usize, Blocks>;

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(text: &str) -> Vec<&str> {
        let mut blocks = Blocks::default();
        blocks.update(text);
        (0..blocks.len())
            .map(|index| &text[blocks.range(index, text.len())])
            .collect()
    }

    #[test]
    fn paragraphs_headings_and_fences_are_blocks_and_nothing_is_lost() {
        let text = "## Title\n\nFirst paragraph\nstill first.\n\n```sql\nSELECT 1;\n\nSELECT 2;\n```\n\nLast.\n";
        let parts = pieces(text);
        assert_eq!(
            parts,
            [
                "## Title\n\n",
                "First paragraph\nstill first.\n\n",
                "```sql\nSELECT 1;\n\nSELECT 2;\n```\n\n",
                "Last.\n"
            ]
        );
        assert_eq!(parts.concat(), text);
    }

    #[test]
    fn a_list_and_its_indented_continuation_stay_together() {
        let text =
            "Intro\n\n1. one\n\n   continued under one\n\n2. two\n- bullet\n\nAfter the list.\n";
        let parts = pieces(text);
        // A list is not cut from the paragraph before it, nor from its own continuation.
        assert_eq!(
            parts,
            [
                "Intro\n\n1. one\n\n   continued under one\n\n2. two\n- bullet\n\n",
                "After the list.\n"
            ]
        );
    }

    #[test]
    fn an_unfinished_last_line_decides_nothing() {
        // "-" might become "- item": no cut before it yet.
        assert_eq!(pieces("Intro\n\n-").len(), 1);
        assert_eq!(pieces("Intro\n\nMore").len(), 1, "the line is not complete");
        assert_eq!(pieces("Intro\n\nMore\n").len(), 2);
    }

    #[test]
    fn cuts_never_move_as_the_text_grows() {
        let full = "A\n\nB\n\n```\nc\n\nd\n```\n\nE\n\n- f\n- g\n\nH\n";
        let mut blocks = Blocks::default();
        let mut seen: Vec<usize> = Vec::new();
        for end in (1..=full.len()).filter(|end| full.is_char_boundary(*end)) {
            blocks.update(&full[..end]);
            let keep = seen.len().min(blocks.starts.len());
            assert_eq!(&blocks.starts[..keep], &seen[..keep]);
            seen = blocks.starts.clone();
        }
        let whole = pieces(full);
        assert_eq!(whole.concat(), full);
        assert_eq!(whole.len(), 5, "{whole:?}");
        // Streamed in small pieces gives the same cuts as all at once.
        assert_eq!(seen, {
            let mut once = Blocks::default();
            once.update(full);
            once.starts
        });
    }

    #[test]
    fn a_text_that_shrank_does_not_fit_the_old_cuts() {
        let mut blocks = Blocks::default();
        blocks.update("A\n\nB\n\nC\n");
        assert!(blocks.fits(8));
        assert!(!blocks.fits(2));
    }

    #[test]
    fn a_longer_fence_is_closed_only_by_a_fence_at_least_as_long() {
        let text = "Intro\n\n````md\n```\n\nnot a close\n```\n\nstill code\n````\n\nAfter\n";
        let parts = pieces(text);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(
            parts[1].starts_with("````md") && parts[1].trim_end().ends_with("````"),
            "{parts:?}"
        );
    }

    #[test]
    fn a_tilde_fence_holds_backtick_lines_and_the_other_way_round() {
        let text = "A\n\n~~~\n```\n\n```\n~~~\n\nB\n\n```\n~~~\n\n~~~\n```\n\nC\n";
        let parts = pieces(text);
        assert_eq!(parts.len(), 5, "{parts:?}");
        assert!(parts[1].contains("~~~\n```\n\n```\n~~~"), "{parts:?}");
        assert!(parts[3].contains("```\n~~~\n\n~~~\n```"), "{parts:?}");
    }

    #[test]
    fn a_paragraph_line_that_starts_with_inline_code_is_not_a_fence() {
        let text = "Intro\n\n```inline``` is a span, not a fence.\n\nNext paragraph.\n\nLast.\n";
        assert_eq!(pieces(text).len(), 4, "{:?}", pieces(text));
    }

    #[test]
    fn raw_html_is_not_tracked_so_an_unclosed_tag_does_not_stop_the_cuts() {
        // `<pre>` and `<!--` are shown as text now; a prose line that opens one and never closes
        // it must not keep every later cut from happening.
        for opener in [
            "<pre> as a word",
            "<!-- not closed",
            "<script>",
            "<textarea>",
        ] {
            let text = format!("Intro\n\n{opener}\n\nSecond\n\nThird\n\nFourth\n");
            assert_eq!(pieces(&text).len(), 5, "{opener}: {:?}", pieces(&text));
        }
    }

    #[test]
    fn a_math_block_is_one_block_like_a_fence() {
        let text = "Intro\n\n$$\na = b\n\n+ c\n$$\n\nAfter\n";
        let parts = pieces(text);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(
            parts[1].starts_with("$$\na = b\n\n+ c\n$$"),
            "the blank line inside is not a cut: {parts:?}"
        );
        // Longer than the opening closes it; a shorter run does not.
        let longer = "$$$\nx\n$$\n\ny\n$$$$\n\nAfter\n";
        assert_eq!(pieces(longer).len(), 2, "{:?}", pieces(longer));
        // One line of math, or text after the dollars, is not a fence.
        for line in ["$$x = 1$$", "$$ a $$ b", "$ not math"] {
            let text = format!("Intro\n\n{line}\n\nSecond\n\nThird\n");
            assert_eq!(pieces(&text).len(), 4, "{line}: {:?}", pieces(&text));
        }
    }

    #[test]
    fn a_table_is_one_block_because_it_has_no_blank_line() {
        let text = "Intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n\nAfter\n";
        let parts = pieces(text);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[1].starts_with("| a | b |") && parts[1].contains("| 3 | 4 |"));
    }
}
