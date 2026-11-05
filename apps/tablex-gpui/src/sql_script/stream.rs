//! The statement splitter, over text held whole ([`super::statement_ranges`]) or arriving from a file
//! ([`ScriptReader`]): one state machine over the driver kit's lexer, so a statement ends where the
//! server would read it end whichever way the text comes in.
//!
//! [`Splitter`] works on a byte buffer the caller owns and asks for more with [`Step::NeedMore`]; it
//! keeps no copy of the text. [`ScriptReader`] feeds it from a reader a megabyte at a time, hands out
//! one statement at a time and drops what it has handed out, so a script of any size costs the memory of
//! its largest statement (capped, with an error that says so). Work is linear in the input: the lexer
//! resumes where its last token ended, and a token that has not ended (a string of hundreds of
//! megabytes) is read again only after the buffer has doubled.
//!
//! Memory: the buffer holds the statement being read and what follows it in the chunk, and doubles while a statement goes
//! on, never past [`MAX_STATEMENT`] (+1 byte). Growing it a last time briefly holds the old and the new buffer (at the cap:
//! about 1.5 times the statement). The statement is then copied again by whoever runs it: see the executor of the run.
//!
//! Besides `;` it reads what the client tools of two servers read in a script and the server does not:
//!
//! * MySQL / MariaDB `DELIMITER xx` lines ([`Syntax::delimiter_command`]): the statement ends at `xx`
//!   (a procedure body keeps its `;`) until `DELIMITER ;`;
//! * PostgreSQL's psql lines ([`Syntax::psql_commands`]): a line starting with `\` (`\connect`, `\set`)
//!   is a [`Piece::Command`], and the data lines after `COPY … FROM stdin;` up to `\.` are dropped
//!   ([`Piece::CopyFromStdin`] stands for the whole block).
//!
//! SQLite: a `CREATE TRIGGER` runs from its `CREATE` to the `END` statement that closes its
//! `BEGIN ... ; ... ; END` body, which holds top-level `;` of its own.

use std::io::Read;
use std::ops::Range;

use engine_driver_kit::sql::{Pull, SqlLexOptions, SqlToken, SqlTokenKind, TokenStream};

/// What the splitter reads besides `;`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Syntax {
    /// `DELIMITER xx` lines (MySQL, MariaDB).
    pub delimiter_command: bool,
    /// psql backslash lines and the data of `COPY … FROM stdin` (PostgreSQL).
    pub psql_commands: bool,
}

/// One thing a script holds that is not blank: ranges are into the buffer given to [`Splitter::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// A statement for the server, trimmed, without its terminator.
    Statement(Range<usize>),
    /// A `COPY … FROM stdin` statement. Its data lines were left out; the server could not be given them.
    CopyFromStdin(Range<usize>),
    /// A psql backslash line, from the `\` to the end of the line.
    Command(Range<usize>),
}

impl Piece {
    pub fn range(&self) -> Range<usize> {
        match self {
            Self::Statement(range) | Self::CopyFromStdin(range) | Self::Command(range) => range.clone(),
        }
    }
}

/// The answer of [`Splitter::step`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Piece(Piece),
    /// The buffer ends inside something that might go on: append input and step again.
    NeedMore,
    End,
}

/// The first three tokens of the statement being read.
#[derive(Debug, Default)]
struct Head {
    tokens: [Option<(SqlTokenKind, Range<usize>)>; 3],
    seen: usize,
    /// `COPY`: the words `FROM stdin` are looked for.
    copy: bool,
    /// Parentheses open: `FROM stdin` counts only at the top level (`COPY (SELECT … FROM stdin) TO STDOUT` is not a block).
    depth: u32,
    after_from: bool,
    from_stdin: bool,
}

#[derive(Debug)]
pub struct Splitter {
    syntax: Syntax,
    /// SQLite: `CREATE TRIGGER` bodies hold `;`.
    trigger_bodies: bool,
    stream: TokenStream,
    /// A `DELIMITER` that is not `;`.
    delimiter: Option<Vec<u8>>,
    /// Where the lexer goes on.
    at: usize,
    /// Where the statement being read starts (after the previous terminator).
    start: usize,
    has_token: bool,
    head: Head,
    /// Inside a trigger: where it starts and where its last statement so far ends.
    joining: Option<(usize, usize)>,
    /// The data of a `COPY … FROM stdin` is being dropped.
    skipping_copy: bool,
    finished: bool,
}

impl Splitter {
    pub fn new(lex: SqlLexOptions, syntax: Syntax) -> Self {
        Self {
            syntax,
            trigger_bodies: lex.bracket_identifiers,
            stream: TokenStream::new(lex),
            delimiter: None,
            at: 0,
            start: 0,
            has_token: false,
            head: Head::default(),
            joining: None,
            skipping_copy: false,
            finished: false,
        }
    }

    /// The first byte of `buffer` the splitter still needs; everything before it may be dropped (then
    /// [`Self::rebase`]).
    pub fn keep_from(&self) -> usize {
        self.joining.map_or(self.start, |(begin, _)| begin.min(self.start))
    }

    /// Where the lexer goes on: bytes after it are what a larger buffer would be read again from.
    pub fn resume(&self) -> usize {
        self.at
    }

    /// The first `dropped` bytes of the buffer were removed.
    pub fn rebase(&mut self, dropped: usize) {
        self.at -= dropped;
        self.start -= dropped;
        if let Some((begin, end)) = &mut self.joining {
            *begin -= dropped;
            *end -= dropped;
        }
        for (_, range) in self.head.tokens.iter_mut().flatten() {
            *range = range.start - dropped..range.end - dropped;
        }
    }

    /// The next piece of `buffer`. `last`: the buffer ends the script. After a piece, its range stays
    /// valid until the buffer is changed.
    pub fn step(&mut self, buffer: &[u8], last: bool) -> Step {
        if self.skipping_copy && self.skip_copy_data(buffer, last) {
            return Step::NeedMore;
        }
        loop {
            // Where to come back to when the buffer ends inside what this token starts (a command line,
            // a delimiter).
            let saved = (self.at, self.stream.clone());
            let token = match self.stream.pull(buffer, &mut self.at, last) {
                Pull::NeedMore => return Step::NeedMore,
                Pull::End => return self.end(buffer),
                Pull::Token(token) => token,
            };
            if !self.has_token {
                match self.command(buffer, &token, last) {
                    Command::None => {}
                    Command::NeedMore => {
                        (self.at, self.stream) = saved;
                        return Step::NeedMore;
                    }
                    Command::Handled => continue,
                    Command::Piece(piece) => return Step::Piece(piece),
                }
            }
            let terminator = match self.terminator(buffer, &token, last) {
                Ok(found) => found,
                Err(NeedMore) => {
                    (self.at, self.stream) = saved;
                    return Step::NeedMore;
                }
            };
            match terminator {
                Some((from, to)) => {
                    if from > token.range.start {
                        self.note(buffer, &token);
                    }
                    self.at = to;
                    if let Some(piece) = self.complete(buffer, from, to) {
                        return Step::Piece(piece);
                    }
                }
                None => self.note(buffer, &token),
            }
        }
    }

    fn end(&mut self, buffer: &[u8]) -> Step {
        if !self.finished {
            self.finished = true;
            if let Some(piece) = self.complete(buffer, buffer.len(), buffer.len()) {
                return Step::Piece(piece);
            }
        }
        // A trigger without its closing `END`: the rest is the trigger, the server reports the syntax error.
        match self.joining.take() {
            Some((begin, end)) => Step::Piece(Piece::Statement(begin..end)),
            None => Step::End,
        }
    }

    /// `token` belongs to the statement being read.
    fn note(&mut self, buffer: &[u8], token: &SqlToken) {
        self.has_token = true;
        let head = &mut self.head;
        if head.seen < head.tokens.len() {
            head.tokens[head.seen] = Some((token.kind, token.range.clone()));
            head.seen += 1;
            head.copy = self.syntax.psql_commands && head.tokens[0].as_ref().is_some_and(|(kind, range)| *kind == SqlTokenKind::Word && word_is(buffer, range, b"copy"));
        }
        if head.copy {
            match token.kind {
                SqlTokenKind::Open => {
                    head.depth += 1;
                    head.after_from = false;
                }
                SqlTokenKind::Close => {
                    head.depth = head.depth.saturating_sub(1);
                    head.after_from = false;
                }
                SqlTokenKind::Word if head.depth == 0 => {
                    head.from_stdin |= head.after_from && word_is(buffer, &token.range, b"stdin");
                    head.after_from = word_is(buffer, &token.range, b"from");
                }
                _ => head.after_from = false,
            }
        }
    }

    /// Where the statement ends in or at `token`: the `;` token, or the custom delimiter found in it.
    fn terminator(&self, buffer: &[u8], token: &SqlToken, last: bool) -> Result<Option<(usize, usize)>, NeedMore> {
        let Some(delimiter) = &self.delimiter else {
            return Ok((token.kind == SqlTokenKind::Semicolon).then_some((token.range.start, token.range.end)));
        };
        if token.kind == SqlTokenKind::Quoted {
            return Ok(None);
        }
        for from in token.range.clone() {
            if buffer[from] != delimiter[0] {
                continue;
            }
            let to = from + delimiter.len();
            if to > buffer.len() {
                if last {
                    continue;
                }
                return Err(NeedMore);
            }
            if buffer[from..to] == delimiter[..] {
                return Ok(Some((from, to)));
            }
        }
        Ok(None)
    }

    /// The statement ended at `from..to`: its piece, if it is one.
    fn complete(&mut self, buffer: &[u8], from: usize, to: usize) -> Option<Piece> {
        let had_token = std::mem::replace(&mut self.has_token, false);
        let head = std::mem::take(&mut self.head);
        let range = trimmed(buffer, self.start..from);
        self.start = to;
        if !had_token || range.is_empty() {
            return None;
        }
        if self.trigger_bodies {
            if let Some((begin, _)) = self.joining {
                if head_word(buffer, &head, 0, b"end") {
                    self.joining = None;
                    return Some(Piece::Statement(begin..range.end));
                }
                self.joining = Some((begin, range.end));
                return None;
            }
            if creates_trigger(buffer, &head) {
                self.joining = Some((range.start, range.end));
                return None;
            }
        }
        if head.from_stdin {
            self.skipping_copy = true;
            return Some(Piece::CopyFromStdin(range));
        }
        Some(Piece::Statement(range))
    }

    /// The first token of a statement may be a command of the client tools, not SQL.
    fn command(&mut self, buffer: &[u8], token: &SqlToken, last: bool) -> Command {
        let text = &buffer[token.range.clone()];
        if self.syntax.delimiter_command && token.kind == SqlTokenKind::Word && text.eq_ignore_ascii_case(b"delimiter") {
            let Some(end) = line_end(buffer, token.range.end, last) else { return Command::NeedMore };
            let argument = trimmed(buffer, token.range.end..end);
            self.delimiter = delimiter_of(&buffer[argument]);
            self.at = (end + 1).min(buffer.len());
            self.start = self.at;
            return Command::Handled;
        }
        if self.syntax.psql_commands && token.kind == SqlTokenKind::Other && text == b"\\" {
            let Some(end) = line_end(buffer, token.range.start, last) else { return Command::NeedMore };
            let line = trimmed(buffer, token.range.start..end);
            self.at = (end + 1).min(buffer.len());
            self.start = self.at;
            return Command::Piece(Piece::Command(line));
        }
        Command::None
    }

    /// Drops the data lines of a `COPY … FROM stdin` up to the line `\.`. `true`: the buffer ended first.
    fn skip_copy_data(&mut self, buffer: &[u8], last: bool) -> bool {
        loop {
            let Some(end) = buffer[self.at..].iter().position(|&byte| byte == b'\n').map(|offset| self.at + offset) else {
                if last {
                    (self.at, self.start, self.skipping_copy) = (buffer.len(), buffer.len(), false);
                    return false;
                }
                // The partial line stays; whole lines before it are gone.
                self.start = self.at;
                return true;
            };
            let line = &buffer[self.at..end];
            let terminator = line.strip_suffix(b"\r").unwrap_or(line) == b"\\.";
            self.at = end + 1;
            self.start = self.at;
            if terminator {
                self.skipping_copy = false;
                return false;
            }
        }
    }
}

struct NeedMore;

enum Command {
    None,
    NeedMore,
    /// Read and acted on; no piece.
    Handled,
    Piece(Piece),
}

fn word_is(buffer: &[u8], range: &Range<usize>, word: &[u8]) -> bool {
    buffer[range.clone()].eq_ignore_ascii_case(word)
}

fn head_word(buffer: &[u8], head: &Head, index: usize, word: &[u8]) -> bool {
    matches!(&head.tokens[index], Some((SqlTokenKind::Word, range)) if word_is(buffer, range, word))
}

/// `CREATE [TEMP|TEMPORARY] TRIGGER`.
fn creates_trigger(buffer: &[u8], head: &Head) -> bool {
    head_word(buffer, head, 0, b"create")
        && (head_word(buffer, head, 1, b"trigger")
            || ((head_word(buffer, head, 1, b"temp") || head_word(buffer, head, 1, b"temporary")) && head_word(buffer, head, 2, b"trigger")))
}

/// The end of the line that holds `from` (the index of its `\n`, or the end of the text when that is all there is).
fn line_end(buffer: &[u8], from: usize, last: bool) -> Option<usize> {
    match buffer[from..].iter().position(|&byte| byte == b'\n') {
        Some(offset) => Some(from + offset),
        None => last.then_some(buffer.len()),
    }
}

/// The text of `DELIMITER xx`: its first word, quotes dropped; `None` for `;` (and for no word at all).
fn delimiter_of(argument: &[u8]) -> Option<Vec<u8>> {
    let word = match argument.first() {
        Some(&quote @ (b'\'' | b'"' | b'`')) => {
            let inner = &argument[1..];
            &inner[..inner.iter().position(|&byte| byte == quote).unwrap_or(inner.len())]
        }
        _ => &argument[..argument.iter().position(u8::is_ascii_whitespace).unwrap_or(argument.len())],
    };
    (!word.is_empty() && word != b";").then(|| word.to_vec())
}

/// `range` without the whitespace at both ends, as `str::trim` reads it (Unicode whitespace too). Bytes that are not UTF-8 are
/// not whitespace.
fn trimmed(buffer: &[u8], range: Range<usize>) -> Range<usize> {
    let slice = &buffer[range.clone()];
    let mut start = 0;
    while let Some((character, width)) = char_at(&slice[start..]) {
        if !character.is_whitespace() {
            break;
        }
        start += width;
    }
    let mut end = slice.len();
    while end > start {
        match char_before(&slice[start..end]) {
            Some((character, width)) if character.is_whitespace() => end -= width,
            _ => break,
        }
    }
    range.start + start..range.start + end
}

/// The character at the start of `bytes` and its width.
fn char_at(bytes: &[u8]) -> Option<(char, usize)> {
    let first = *bytes.first()?;
    if first < 0x80 {
        return Some((char::from(first), 1));
    }
    let width = match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => return None,
    };
    std::str::from_utf8(bytes.get(..width)?).ok()?.chars().next().map(|character| (character, width))
}

/// The character that ends `bytes` and its width.
fn char_before(bytes: &[u8]) -> Option<(char, usize)> {
    let last = *bytes.last()?;
    if last < 0x80 {
        return Some((char::from(last), 1));
    }
    let width = (1..=bytes.len().min(4)).find(|width| !matches!(bytes[bytes.len() - width], 0x80..=0xBF))?;
    char_at(&bytes[bytes.len() - width..]).filter(|(_, found)| *found == width)
}

// ---------------------------------------------------------------- a script from a reader

/// Bytes asked of the reader at a time.
pub const CHUNK: usize = 1024 * 1024;

/// The most bytes one statement may hold.
pub const MAX_STATEMENT: usize = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum ScriptError {
    Read(std::io::Error),
    /// A statement (starting on `line`) is longer than the limit: it cannot be told where it ends within the memory the run may use.
    StatementTooLarge { line: u64, limit: usize },
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read(error) => write!(f, "The file could not be read: {error}"),
            Self::StatementTooLarge { line, limit } => write!(
                f,
                "The statement that starts on line {line} is longer than {} MiB (or is never closed). A statement is held whole while it is read, so the run stops here.",
                limit / (1024 * 1024)
            ),
        }
    }
}

impl std::error::Error for ScriptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read(error) => Some(error),
            Self::StatementTooLarge { .. } => None,
        }
    }
}

/// What [`ScriptReader::next`] hands out: the piece, its text (valid until the next call) and the line it starts on.
#[derive(Debug)]
pub struct Item<'a> {
    pub piece: Piece,
    pub text: &'a [u8],
    pub line: u64,
}

pub struct ScriptReader<R> {
    reader: R,
    splitter: Splitter,
    buffer: Vec<u8>,
    eof: bool,
    chunk: usize,
    limit: usize,
    /// `line` is the line number of `buffer[counted]`.
    line: u64,
    counted: usize,
    /// How many times the buffer was refilled.
    fills: u64,
}

impl<R: Read> ScriptReader<R> {
    pub fn new(reader: R, lex: SqlLexOptions, syntax: Syntax) -> Self {
        Self::with_limits(reader, lex, syntax, CHUNK, MAX_STATEMENT)
    }

    pub fn with_limits(reader: R, lex: SqlLexOptions, syntax: Syntax, chunk: usize, limit: usize) -> Self {
        Self { reader, splitter: Splitter::new(lex, syntax), buffer: Vec::new(), eof: false, chunk: chunk.max(1), limit, line: 1, counted: 0, fills: 0 }
    }

    /// The next piece, or `None` at the end of the script.
    pub fn next(&mut self) -> Result<Option<Item<'_>>, ScriptError> {
        let piece = loop {
            match self.splitter.step(&self.buffer, self.eof) {
                Step::Piece(piece) => break piece,
                Step::End => return Ok(None),
                Step::NeedMore => self.fill()?,
            }
        };
        let range = piece.range();
        self.count_lines(range.start);
        Ok(Some(Item { piece, text: &self.buffer[range], line: self.line }))
    }

    /// How many times the buffer was refilled.
    #[cfg(test)]
    pub fn fills(&self) -> u64 {
        self.fills
    }

    /// Bytes held right now (a statement and the lookahead).
    #[cfg(test)]
    pub fn held(&self) -> usize {
        self.buffer.len()
    }

    fn count_lines(&mut self, upto: usize) {
        if upto > self.counted {
            self.line += self.buffer[self.counted..upto].iter().filter(|&&byte| byte == b'\n').count() as u64;
            self.counted = upto;
        }
    }

    /// Drops what the splitter is done with and reads on.
    fn fill(&mut self) -> Result<(), ScriptError> {
        let keep = self.splitter.keep_from();
        self.count_lines(keep);
        if keep > 0 {
            self.buffer.drain(..keep);
            self.splitter.rebase(keep);
            self.counted = self.counted.saturating_sub(keep);
        }
        // A token that has not ended is read again from its start with the next piece: reading as much
        // as it holds keeps that linear.
        let unfinished = self.buffer.len() - self.splitter.resume();
        let room = (self.limit + 1).saturating_sub(self.buffer.len());
        self.fills += 1;
        if room == 0 {
            // The statement starts at its first token: the blank lines before it are not its lines.
            let blank = self.buffer.iter().take_while(|byte| byte.is_ascii_whitespace()).filter(|&&byte| byte == b'\n').count() as u64;
            return Err(ScriptError::StatementTooLarge { line: self.line + blank, limit: self.limit });
        }
        // The buffer doubles while a statement goes on (so the copies a growing buffer makes add up to the statement's size, not to
        // its size times the number of chunks), and never past the cap.
        let want = self.chunk.max(unfinished).max(self.buffer.len()).min(room);
        self.buffer.reserve_exact(want);
        let read = (&mut self.reader).take(want as u64).read_to_end(&mut self.buffer).map_err(ScriptError::Read)?;
        self.eof = read < want;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::encoding::Encoding;

    fn syntax_for(lex: &SqlLexOptions) -> Syntax {
        Syntax { delimiter_command: lex.backtick_identifiers && lex.hash_comments, psql_commands: lex.dollar_quotes && lex.nested_block_comments }
    }

    /// Every piece of `text` read through a reader in chunks of `chunk` bytes: `(kind, text, line)`.
    fn pieces(text: &[u8], lex: SqlLexOptions, chunk: usize) -> Vec<(char, String, u64)> {
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(text.to_vec()), lex, syntax_for(&lex), chunk, MAX_STATEMENT);
        let mut found = Vec::new();
        while let Some(item) = reader.next().unwrap() {
            let kind = match item.piece {
                Piece::Statement(_) => 's',
                Piece::CopyFromStdin(_) => 'c',
                Piece::Command(_) => 'm',
            };
            found.push((kind, String::from_utf8_lossy(item.text).into_owned(), item.line));
        }
        found
    }

    fn texts(found: &[(char, String, u64)]) -> Vec<&str> {
        found.iter().map(|(_, text, _)| text.as_str()).collect()
    }

    const PG: fn() -> SqlLexOptions = SqlLexOptions::postgres;
    const MY: fn() -> SqlLexOptions = SqlLexOptions::mysql;

    /// A script with a string, a dollar quote, a comment, a nested comment and a quoted name, each holding `;`.
    const TRICKY_PG: &str = "select 'a;b', \"c;d\"; /* x; /* y; */ z; */ select $f$ e;f $f$;\n-- g;h\nselect 'it''s;ok' ;create function f() returns int as $$ begin return 1; end $$ language plpgsql;select 1";

    #[test]
    fn any_chunk_size_gives_the_same_statements() {
        let whole = pieces(TRICKY_PG.as_bytes(), PG(), 1 << 20);
        assert_eq!(
            texts(&whole),
            [
                "select 'a;b', \"c;d\"",
                "/* x; /* y; */ z; */ select $f$ e;f $f$",
                "-- g;h\nselect 'it''s;ok'",
                "create function f() returns int as $$ begin return 1; end $$ language plpgsql",
                "select 1"
            ]
        );
        for chunk in 1..=48 {
            assert_eq!(pieces(TRICKY_PG.as_bytes(), PG(), chunk), whole, "chunks of {chunk}");
        }
    }

    #[test]
    fn the_whole_text_splitter_and_the_streaming_one_cut_the_same_ranges() {
        let samples = [
            (TRICKY_PG, PG()),
            ("select 'a\\';b'; select `c;d`; # e;\nselect 1 -- f;\n;select 2", MY()),
            ("create table t(a); CREATE TEMP TRIGGER tr AFTER INSERT ON t BEGIN update t set a = case when 1 then 2 end; delete from t; END; select 1; create trigger u begin select 1;", SqlLexOptions::sqlite()),
            ("select 1 /* open; select 2", PG()),
            ("select '\u{e9}\u{1f600};' ; select \u{4e16}\u{754c}", PG()),
        ];
        for (text, lex) in samples {
            let expected: Vec<String> = super::super::statement_ranges(text, &lex).into_iter().map(|range| text[range].to_owned()).collect();
            for chunk in [1, 2, 5, 9, 64, 1 << 20] {
                let streamed: Vec<String> = pieces(text.as_bytes(), lex, chunk).into_iter().map(|(_, text, _)| text).collect();
                assert_eq!(streamed, expected, "{text:?} in chunks of {chunk}");
            }
        }
    }

    #[test]
    fn a_mysql_delimiter_keeps_the_semicolons_of_a_procedure_and_a_chunk_edge_inside_it_changes_nothing() {
        let script = "DELIMITER //\r\nCREATE PROCEDURE p()\r\nBEGIN\r\n  SELECT 'a;//b';\r\n  -- c // d\r\n  SELECT 2;\r\nEND//\r\nDELIMITER ;\r\nSELECT 3;\r\ndelimiter $$\nselect 4$$ select 5; select 6 $$\nDELIMITER ;\nselect 7";
        let expected = [
            "CREATE PROCEDURE p()\r\nBEGIN\r\n  SELECT 'a;//b';\r\n  -- c // d\r\n  SELECT 2;\r\nEND",
            "SELECT 3",
            "select 4",
            "select 5; select 6",
            "select 7",
        ];
        for chunk in (1..=40).chain([1000]) {
            let found = pieces(script.as_bytes(), MY(), chunk);
            assert_eq!(texts(&found), expected, "chunks of {chunk}");
        }
        // The lines are the file's own: the procedure starts on line 2, `select 4` on line 11.
        let found = pieces(script.as_bytes(), MY(), 7);
        assert_eq!(found.iter().map(|(_, _, line)| *line).collect::<Vec<_>>(), [2, 9, 11, 11, 13]);
    }

    #[test]
    fn a_delimiter_inside_a_word_or_spanning_tokens_ends_the_statement() {
        let found = pieces(b"delimiter $$\nselect 1 from t$$select 2$$\ndelimiter ;;\nselect 3;;select 4", MY(), 3);
        assert_eq!(texts(&found), ["select 1 from t", "select 2", "select 3", "select 4"]);
        // PostgreSQL has no DELIMITER line: it is a statement for the server to refuse.
        assert_eq!(texts(&pieces(b"delimiter //\nselect 1;", PG(), 4)), ["delimiter //\nselect 1"]);
    }

    #[test]
    fn psql_lines_are_commands_and_copy_data_is_left_out() {
        let script = "\\connect other\nCREATE TABLE t(a int);\nCOPY t (a) FROM stdin;\n1\n2;\n\\.\nSELECT 'x';\n\\set ON_ERROR_STOP on\r\ninsert into t values (1)\n\\echo done";
        let expected = ["\\connect other", "CREATE TABLE t(a int)", "COPY t (a) FROM stdin", "SELECT 'x'", "\\set ON_ERROR_STOP on", "insert into t values (1)\n\\echo done"];
        for chunk in (1..=30).chain([1000]) {
            let found = pieces(script.as_bytes(), PG(), chunk);
            assert_eq!(texts(&found), expected, "chunks of {chunk}");
            assert_eq!(found.iter().map(|(kind, ..)| *kind).collect::<String>(), "mscsms", "chunks of {chunk}");
        }
        // The data block has no `\.`: it runs to the end of the script and nothing of it is a statement.
        assert_eq!(texts(&pieces(b"COPY t FROM STDIN;\n1\n2\n", PG(), 3)), ["COPY t FROM STDIN"]);
        // `copy` of a table to a file, a column named stdin and an ordinary statement are not blocks.
        assert_eq!(texts(&pieces(b"copy t to stdout; select stdin from from_t; select 2", PG(), 5)), ["copy t to stdout", "select stdin from from_t", "select 2"]);
    }

    #[test]
    fn a_copy_block_is_a_top_level_copy_from_stdin_only() {
        for (script, expected) in [
            ("COPY (SELECT stdin FROM from_t) TO STDOUT; select 2", vec!["COPY (SELECT stdin FROM from_t) TO STDOUT", "select 2"]),
            ("COPY (SELECT * FROM stdin) TO STDOUT;\nselect 2", vec!["COPY (SELECT * FROM stdin) TO STDOUT", "select 2"]),
            ("COPY t (a, b) FROM stdin WITH (FORMAT csv);\n1,2\n\\.\nselect 2", vec!["COPY t (a, b) FROM stdin WITH (FORMAT csv)", "select 2"]),
        ] {
            for chunk in [1, 3, 1000] {
                assert_eq!(texts(&pieces(script.as_bytes(), PG(), chunk)), expected, "{script:?} in chunks of {chunk}");
            }
        }
    }

    #[test]
    fn statements_are_trimmed_of_unicode_whitespace_as_str_trim_does() {
        let script = "\u{a0}select 1\u{3000};\u{2003}\n select 2 \u{85}\u{2028};select '\u{a0}x\u{a0}'\u{a0}";
        let expected: Vec<&str> = ["\u{a0}select 1\u{3000}", "\u{2003}\n select 2 \u{85}\u{2028}", "select '\u{a0}x\u{a0}'\u{a0}"].iter().map(|text| text.trim()).collect();
        assert_eq!(expected, ["select 1", "select 2", "select '\u{a0}x\u{a0}'"]);
        let found: Vec<String> = super::super::statement_ranges(script, &PG()).into_iter().map(|range| script[range].to_owned()).collect();
        assert_eq!(found, expected);
        for chunk in [1, 2, 3, 1000] {
            assert_eq!(texts(&pieces(script.as_bytes(), PG(), chunk)), expected, "chunks of {chunk}");
        }
        // Bytes that are not UTF-8 are not whitespace, and a cut character is left alone.
        assert_eq!(trimmed(b" a\xff ", 0..4), 1..3);
        assert_eq!(trimmed(b"\xe3\x80", 0..2), 0..2);
    }

    #[test]
    fn a_statement_that_goes_on_grows_the_buffer_by_doubling() {
        let script = format!("insert into t values ({});", "'xxxxxxxx', ".repeat(8000));
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(script.clone().into_bytes()), PG(), Syntax::default(), 64, MAX_STATEMENT);
        assert_eq!(reader.next().unwrap().unwrap().text.len(), script.len() - 1);
        assert!(reader.fills() <= 24, "{} refills for {} bytes read 64 at a time", reader.fills(), script.len());
        // A cap in the middle of the doubling is respected: the buffer never holds more than the cap and one byte.
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(script.into_bytes()), PG(), Syntax::default(), 64, 50_000);
        assert!(matches!(reader.next(), Err(ScriptError::StatementTooLarge { .. })));
        assert!(reader.held() <= 50_001, "{}", reader.held());
    }

    #[test]
    fn a_sqlite_trigger_is_one_statement_across_chunks() {
        let script = "CREATE TRIGGER tr AFTER INSERT ON t BEGIN\n update t set a = 1;\n delete from t;\nEND;\nselect 1";
        for chunk in [1, 4, 17, 1000] {
            let found = pieces(script.as_bytes(), SqlLexOptions::sqlite(), chunk);
            assert_eq!(texts(&found), ["CREATE TRIGGER tr AFTER INSERT ON t BEGIN\n update t set a = 1;\n delete from t;\nEND", "select 1"], "{chunk}");
        }
    }

    #[test]
    fn lines_count_from_one_across_chunks_and_the_buffer_is_dropped_behind() {
        let mut script = String::new();
        for number in 1..=300 {
            script.push_str(&format!("-- row {number}\ninsert into t values ({number});\n\n"));
        }
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(script.into_bytes()), PG(), Syntax::default(), 64, MAX_STATEMENT);
        let mut count = 0;
        let mut most_held = 0;
        while let Some(item) = reader.next().unwrap() {
            count += 1;
            assert_eq!(item.line, 3 * count - 2, "the comment before the statement is its first line");
            most_held = most_held.max(reader.held());
        }
        assert_eq!(count, 300);
        assert!(most_held < 400, "only the statement being read and a chunk are held, not the {} bytes of the script: {most_held}", 300 * 40);
    }

    #[test]
    fn a_statement_over_the_limit_stops_the_read_with_the_line_it_starts_on() {
        let script = format!("select 1;\n\nselect '{}';\nselect 3", "x".repeat(5000));
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(script.into_bytes()), PG(), Syntax::default(), 128, 1000);
        assert_eq!(String::from_utf8_lossy(reader.next().unwrap().unwrap().text), "select 1");
        let error = reader.next().unwrap_err();
        assert!(matches!(error, ScriptError::StatementTooLarge { line: 3, limit: 1000 }), "{error}");
        assert!(error.to_string().contains("line 3"), "{error}");
        // A statement at the limit is fine.
        let script = format!("select '{}'", "y".repeat(980));
        let mut reader = ScriptReader::with_limits(std::io::Cursor::new(script.into_bytes()), PG(), Syntax::default(), 128, 1000);
        assert_eq!(reader.next().unwrap().unwrap().text.len(), 989);
    }

    #[test]
    fn the_encodings_of_a_file_reach_the_splitter_as_utf8() {
        let text = "select 'caf\u{e9};', \u{1f600};\r\nselect 2 -- \u{4e16}\r\n;select 3";
        let expected = ["select 'caf\u{e9};', \u{1f600}", "select 2 -- \u{4e16}", "select 3"];
        let utf16 = |big_endian: bool, mark: bool| {
            let mut units: Vec<u16> = text.encode_utf16().collect();
            if mark {
                units.insert(0, 0xFEFF);
            }
            units.iter().flat_map(|unit| if big_endian { unit.to_be_bytes() } else { unit.to_le_bytes() }).collect::<Vec<u8>>()
        };
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend(text.as_bytes());
        let latin1: Vec<u8> = "select 'caf\u{e9};'; select 2".chars().map(|c| c as u8).collect();
        for (encoding, bytes, expected) in [
            (Encoding::Auto, with_bom.clone(), expected.to_vec()),
            (Encoding::Auto, utf16(false, true), expected.to_vec()),
            (Encoding::Auto, utf16(true, true), expected.to_vec()),
            (Encoding::Utf16Le, utf16(false, false), expected.to_vec()),
            (Encoding::Latin1, latin1, vec!["select 'caf\u{e9};'", "select 2"]),
        ] {
            let decoded = encoding.decode(std::io::Cursor::new(bytes)).unwrap();
            let mut reader = ScriptReader::with_limits(decoded, PG(), Syntax::default(), 5, MAX_STATEMENT);
            let mut found = Vec::new();
            while let Some(item) = reader.next().unwrap() {
                found.push(String::from_utf8(item.text.to_vec()).unwrap());
            }
            assert_eq!(found, expected, "{encoding:?}");
        }
    }

    #[test]
    #[ignore = "writes and reads a 300 MB file; run in release"]
    fn one_statement_of_300_megabytes_is_read_in_linear_time() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("one.sql");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
        // One INSERT: millions of short tuples, then a tuple that holds one giant string.
        let (mut size, mut tuples) = (0usize, 0u64);
        file.write_all(b"INSERT INTO t VALUES ").unwrap();
        while size < 150 * 1024 * 1024 {
            let tuple = format!("({tuples}, 'name {tuples}', 'a;b''c'), ");
            file.write_all(tuple.as_bytes()).unwrap();
            size += tuple.len();
            tuples += 1;
        }
        file.write_all(b"(0, '', ''), (1, '").unwrap();
        let block = vec![b'z'; 1024 * 1024];
        for _ in 0..150 {
            file.write_all(&block).unwrap();
        }
        file.write_all(b"', '');\nselect 1;\n").unwrap();
        drop(file);
        let length = std::fs::metadata(&path).unwrap().len();
        let run = |limit: usize| {
            let started = std::time::Instant::now();
            let mut reader = ScriptReader::with_limits(std::fs::File::open(&path).unwrap(), PG(), Syntax::default(), CHUNK, limit);
            let mut pieces = Vec::new();
            let mut most_held = 0;
            loop {
                match reader.next() {
                    Ok(Some(item)) => pieces.push(item.text.len()),
                    Ok(None) => break,
                    Err(error) => return (Err(error), started.elapsed(), most_held),
                }
                most_held = most_held.max(reader.held());
            }
            (Ok(pieces), started.elapsed(), most_held)
        };
        let (pieces, elapsed, held) = run(1 << 30);
        let pieces = pieces.unwrap();
        eprintln!("{length} bytes, {} pieces {pieces:?}: {elapsed:?} ({:.0} MB/s), most held {} MB", pieces.len(), length as f64 / 1e6 / elapsed.as_secs_f64(), held / 1_000_000);
        assert_eq!(pieces.len(), 2, "the INSERT of 300 MB, then the select");
        assert!(elapsed.as_secs_f64() < 10.0 + length as f64 / 50e6, "{elapsed:?}");
        // The default cap (256 MiB) refuses it, and so does any cap below its size: the error names the line.
        let (refused, ..) = run(MAX_STATEMENT);
        assert!(matches!(refused, Err(ScriptError::StatementTooLarge { line: 1, .. })), "{refused:?}");
    }
}
