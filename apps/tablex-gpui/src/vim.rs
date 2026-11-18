//! Basic Vim keys for the query editors: a pure state machine. A key, the mode, the text and the
//! selection go in; the one edit, the new selection and the register change that follow come out
//! (`Effect`). `app::vim` applies them to the editor; nothing here knows GPUI.
//!
//! Supported: modes Normal / Insert / Visual (`v`) / Visual line (`V`); `h j k l`, `w b e`,
//! `0 ^ $`, `gg G`; `x dd yy p P u Ctrl-R D C`; the operators `d c y` with those motions (`dw`,
//! `cw`, `d$`, `y0`, `dgg`, `dj` ...); `i a I A o O`; in Visual the motions extend the selection and
//! `d x y c` act on it. Not supported (ignored in Normal mode, never inserted): counts, text objects,
//! `.`, search, `f`/`t`, macros, registers by name. Never inserted holds for text that is not a key
//! press too (an input method's composition, dead keys, Option chords): `app::vim` makes the editor
//! read-only outside Insert mode, so its input handler refuses them.
//!
//! Offsets are byte offsets into the editor's rope. A command reads only the cursor's line (and the
//! neighbouring one for `j`/`k`), so it costs the length of that line, not of the buffer.

use gpui_component::input::{Rope, RopeExt};
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Normal,
    Insert,
    Visual,
    VisualLine,
}

/// A key the machine reacts to; the adapter maps the platform's keystrokes onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Escape or Ctrl-[.
    Esc,
    CtrlR,
}

/// The unnamed register, shared by every editor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Register {
    pub text: String,
    /// A whole-line yank or delete: the text ends with a newline and `p` puts it on its own line.
    pub linewise: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum History {
    Undo,
    Redo,
}

/// What a key asks the editor to do, in this order: the edit, the selection, the register, the history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    /// Replace the range with the text (one undo step).
    pub edit: Option<(Range<usize>, String)>,
    /// The selection after the edit (post-edit offsets); a caret when the range is empty. The flag
    /// puts the caret at the range's start.
    pub select: Option<(Range<usize>, bool)>,
    /// A new content of the register.
    pub register: Option<Register>,
    /// The register content also goes to the system clipboard (a yank).
    pub copy: bool,
    pub history: Option<History>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

impl Op {
    fn of(ch: char) -> Option<Self> {
        match ch {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            _ => None,
        }
    }

    fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Down,
    Up,
    WordForward,
    WordBack,
    WordEnd,
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
}

impl Motion {
    fn of(ch: char) -> Option<Self> {
        Some(match ch {
            'h' => Self::Left,
            'l' => Self::Right,
            'j' => Self::Down,
            'k' => Self::Up,
            'w' => Self::WordForward,
            'b' => Self::WordBack,
            'e' => Self::WordEnd,
            '0' => Self::LineStart,
            '^' => Self::FirstNonBlank,
            '$' => Self::LineEnd,
            'G' => Self::Bottom,
            _ => return None,
        })
    }

    fn linewise(self) -> bool {
        matches!(self, Self::Down | Self::Up | Self::Top | Self::Bottom)
    }
}

/// What an operator works on.
enum Span {
    Chars(Range<usize>),
    /// Whole lines: the first line's start and the last line's end (before its newline).
    Lines(usize, usize),
}

#[derive(Clone, Debug, Default)]
pub struct Vim {
    mode: Mode,
    /// An operator waiting for its motion.
    op: Option<Op>,
    /// A `g` waiting for the second one.
    g: bool,
    /// The fixed and the moving end of a Visual selection (the char under each is selected).
    anchor: usize,
    head: usize,
    /// The selection the machine last put in the editor: another one means the user moved it.
    applied: Option<Range<usize>>,
    /// The column `j` / `k` keep (`usize::MAX` after `$`).
    want_col: Option<usize>,
}

impl Vim {
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Handles `key` with the editor's text and selection. In Insert mode only Escape is the
    /// machine's; every other key belongs to the editor.
    pub fn key(
        &mut self,
        key: Key,
        text: &Rope,
        selection: Range<usize>,
        reversed: bool,
        register: &Register,
    ) -> Effect {
        match self.mode {
            Mode::Insert => match key {
                Key::Esc => self.leave_insert(text, &selection, reversed),
                _ => Effect::default(),
            },
            Mode::Normal => self.normal(key, text, &selection, reversed, register),
            Mode::Visual | Mode::VisualLine => self.visual(key, text, selection, reversed),
        }
    }

    /// Where a Normal-mode caret that sits after the last char of its line (a click at the line's end)
    /// belongs: on that char. `None` when it is already there, or the mode is not Normal.
    pub fn normal_caret(&self, text: &Rope, selection: &Range<usize>) -> Option<usize> {
        if self.mode != Mode::Normal || !selection.is_empty() {
            return None;
        }
        let caret = clamp(text, selection.start);
        (caret != selection.start).then_some(caret)
    }

    fn leave_insert(&mut self, t: &Rope, selection: &Range<usize>, reversed: bool) -> Effect {
        self.mode = Mode::Normal;
        let caret = caret_of(selection, reversed);
        // Vim steps back onto the last typed char.
        let cursor = if caret > line_start(t, caret) { caret - width(before(t, caret)) } else { caret };
        caret_effect(cursor)
    }

    fn normal(
        &mut self,
        key: Key,
        t: &Rope,
        selection: &Range<usize>,
        reversed: bool,
        register: &Register,
    ) -> Effect {
        let op = self.op.take();
        let g = std::mem::take(&mut self.g);
        if !matches!(key, Key::Char('j' | 'k')) {
            self.want_col = None;
        }
        let c = clamp(t, caret_of(selection, reversed));
        let ch = match key {
            Key::Esc => return Effect::default(),
            Key::CtrlR if op.is_none() && !g => {
                return Effect { history: Some(History::Redo), ..Effect::default() };
            }
            Key::CtrlR => return Effect::default(),
            Key::Char(ch) => ch,
        };
        if g {
            return match (ch, op) {
                ('g', Some(op)) => self.operate(op, Motion::Top, t, c),
                ('g', None) => self.go(Motion::Top, t, c),
                _ => Effect::default(),
            };
        }
        if ch == 'g' {
            self.op = op;
            self.g = true;
            return Effect::default();
        }
        if let Some(op) = op {
            if ch == op.key() {
                let (first, last) = (line_start(t, c), line_end(t, c));
                return self.finish(op, Span::Lines(first, last), t, c);
            }
            return match Motion::of(ch) {
                Some(motion) => self.operate(op, motion, t, c),
                None => Effect::default(),
            };
        }
        if let Some(motion) = Motion::of(ch) {
            return self.go(motion, t, c);
        }
        if let Some(op) = Op::of(ch) {
            self.op = Some(op);
            return Effect::default();
        }
        let eol = line_end(t, c);
        let on_char = matches!(at(t, c), Some(x) if x != '\n');
        match ch {
            'i' => self.insert_at(c),
            'a' => self.insert_at(if on_char { c + width(at(t, c)) } else { c }),
            'I' => self.insert_at(first_non_blank(t, line_start(t, c))),
            'A' => self.insert_at(eol),
            'o' => {
                self.mode = Mode::Insert;
                Effect {
                    edit: Some((eol..eol, "\n".into())),
                    select: Some((eol + 1..eol + 1, false)),
                    ..Effect::default()
                }
            }
            'O' => {
                self.mode = Mode::Insert;
                let start = line_start(t, c);
                Effect {
                    edit: Some((start..start, "\n".into())),
                    select: Some((start..start, false)),
                    ..Effect::default()
                }
            }
            'x' if on_char => self.finish(Op::Delete, Span::Chars(c..c + width(at(t, c))), t, c),
            'D' => self.finish(Op::Delete, Span::Chars(c..eol), t, c),
            'C' => self.finish(Op::Change, Span::Chars(c..eol), t, c),
            'p' => self.put(t, c, register, false),
            'P' => self.put(t, c, register, true),
            'u' => Effect { history: Some(History::Undo), ..Effect::default() },
            'v' | 'V' => {
                self.mode = if ch == 'v' { Mode::Visual } else { Mode::VisualLine };
                self.anchor = c;
                self.head = c;
                self.select_visual(t)
            }
            // Everything else is a command this machine does not have: ignored.
            _ => Effect::default(),
        }
    }

    fn insert_at(&mut self, offset: usize) -> Effect {
        self.mode = Mode::Insert;
        Effect { select: Some((offset..offset, false)), ..Effect::default() }
    }

    /// A motion in Normal mode: the cursor moves.
    fn go(&mut self, motion: Motion, t: &Rope, c: usize) -> Effect {
        let target = self.target(motion, t, c, false);
        caret_effect(clamp(t, target))
    }

    /// Where `motion` leads from `c`, and what `j` / `k` / `$` keep for the next `j` / `k`. With
    /// `operator`, a motion may end on the line end (`dl`, `d$`).
    fn target(&mut self, motion: Motion, t: &Rope, c: usize, operator: bool) -> usize {
        self.want_col = match motion {
            Motion::Down | Motion::Up => {
                Some(self.want_col.unwrap_or_else(|| column(t, line_start(t, c), c)))
            }
            Motion::LineEnd => Some(usize::MAX),
            _ => None,
        };
        let ls = line_start(t, c);
        match motion {
            Motion::Left => {
                if c > ls {
                    c - width(before(t, c))
                } else {
                    c
                }
            }
            Motion::Right => match at(t, c) {
                Some(x) if x != '\n' => {
                    let next = c + x.len_utf8();
                    if operator || matches!(at(t, next), Some(y) if y != '\n') {
                        next
                    } else {
                        c
                    }
                }
                _ => c,
            },
            Motion::Down => {
                let eol = line_end(t, c);
                if at(t, eol) == Some('\n') {
                    offset_at_column(t, eol + 1, self.want_col.unwrap_or(0))
                } else {
                    c
                }
            }
            Motion::Up => {
                if ls == 0 {
                    c
                } else {
                    offset_at_column(t, line_start(t, ls - 1), self.want_col.unwrap_or(0))
                }
            }
            Motion::WordForward => word_forward(t, c),
            Motion::WordBack => word_back(t, c),
            Motion::WordEnd => word_end(t, c),
            Motion::LineStart => ls,
            Motion::FirstNonBlank => first_non_blank(t, ls),
            Motion::LineEnd => {
                let eol = line_end(t, c);
                if operator {
                    eol
                } else {
                    last_char(t, ls, eol)
                }
            }
            Motion::Top => first_non_blank(t, 0),
            Motion::Bottom => first_non_blank(t, line_start(t, t.len())),
        }
    }

    /// An operator and its motion (`dw`, `cw`, `y0`, `dj` ...).
    fn operate(&mut self, op: Op, motion: Motion, t: &Rope, c: usize) -> Effect {
        let target = self.target(motion, t, c, true);
        if motion.linewise() {
            let (a, b) = (c.min(target), c.max(target));
            return self.finish(op, Span::Lines(line_start(t, a), line_end(t, b)), t, c);
        }
        let mut target = target;
        if motion == Motion::WordForward {
            // `cw` on a word changes up to its end, like `ce`; no `w` runs over the line end.
            if op == Op::Change && at(t, c).is_some_and(|x| class(x) != 0) {
                target = run_end(t, c);
            }
            target = target.min(line_end(t, c));
        }
        let (a, mut b) = (c.min(target), c.max(target));
        if motion == Motion::WordEnd {
            b += width(at(t, b));
        }
        self.finish(op, Span::Chars(a..b), t, c)
    }

    /// Applies an operator to a span; `c` is the cursor it was typed at.
    fn finish(&mut self, op: Op, span: Span, t: &Rope, c: usize) -> Effect {
        self.mode = if op == Op::Change { Mode::Insert } else { Mode::Normal };
        self.applied = None;
        match span {
            Span::Chars(range) => {
                let register = Register { text: t.slice(range.clone()).to_string(), linewise: false };
                let start = range.start;
                match op {
                    Op::Yank => Effect {
                        register: Some(register),
                        copy: true,
                        ..caret_effect(clamp(t, start))
                    },
                    Op::Delete | Op::Change => {
                        // The text after the cut moves up to `start`: on the line end the caret
                        // steps back (Normal only).
                        let cursor = if op == Op::Change { start } else { clamp_after_cut(t, start, range.end) };
                        Effect {
                            edit: Some((range, String::new())),
                            select: Some((cursor..cursor, false)),
                            register: Some(register),
                            ..Effect::default()
                        }
                    }
                }
            }
            Span::Lines(first, last) => {
                let register = Register { text: format!("{}\n", t.slice(first..last)), linewise: true };
                match op {
                    Op::Yank => {
                        let cursor = if first < line_start(t, c) { first } else { c };
                        Effect { register: Some(register), copy: true, ..caret_effect(cursor) }
                    }
                    Op::Change => Effect {
                        edit: Some((first..last, String::new())),
                        select: Some((first..first, false)),
                        register: Some(register),
                        ..Effect::default()
                    },
                    Op::Delete => {
                        // The lines and one newline; the last line of the text takes the newline
                        // before it.
                        let (cut, cursor) = if at(t, last) == Some('\n') {
                            let next = last + 1;
                            let cursor = clamp(t, first_non_blank(t, next));
                            (first..next, cursor - (next - first))
                        } else if first > 0 {
                            let prev = line_start(t, first - 1);
                            (first - 1..last, clamp(t, first_non_blank(t, prev)))
                        } else {
                            (first..last, 0)
                        };
                        Effect {
                            edit: Some((cut, String::new())),
                            select: Some((cursor..cursor, false)),
                            register: Some(register),
                            ..Effect::default()
                        }
                    }
                }
            }
        }
    }

    fn put(&mut self, t: &Rope, c: usize, register: &Register, before_cursor: bool) -> Effect {
        let text = &register.text;
        if text.is_empty() {
            return Effect::default();
        }
        if register.linewise {
            let leading = text.bytes().take_while(|b| *b == b' ' || *b == b'\t').count();
            // Where the text goes and where its first line starts afterwards.
            let (offset, insert, line) = if before_cursor {
                let start = line_start(t, c);
                (start, text.clone(), start)
            } else {
                let eol = line_end(t, c);
                if at(t, eol) == Some('\n') {
                    (eol + 1, text.clone(), eol + 1)
                } else {
                    // The last line has no newline: one is added before the text.
                    (eol, format!("\n{}", text.strip_suffix('\n').unwrap_or(text)), eol + 1)
                }
            };
            return Effect {
                edit: Some((offset..offset, insert)),
                select: Some((line + leading..line + leading, false)),
                ..Effect::default()
            };
        }
        let on_char = matches!(at(t, c), Some(x) if x != '\n');
        let pos = if !before_cursor && on_char { c + width(at(t, c)) } else { c };
        // Pasted text on one line leaves the cursor on its last char, longer text at its start.
        let cursor = if text.contains('\n') {
            pos
        } else {
            pos + text.len() - text.chars().next_back().map_or(0, char::len_utf8)
        };
        Effect {
            edit: Some((pos..pos, text.clone())),
            select: Some((cursor..cursor, false)),
            ..Effect::default()
        }
    }

    // ---- Visual ------------------------------------------------------------------------------

    fn visual(&mut self, key: Key, t: &Rope, selection: Range<usize>, reversed: bool) -> Effect {
        // A selection that is not the one put in the editor was made with the mouse.
        if self.applied.as_ref() != Some(&selection) {
            if selection.is_empty() {
                self.anchor = selection.start;
                self.head = selection.start;
            } else {
                let last = selection.end - width(before(t, selection.end));
                (self.anchor, self.head) =
                    if reversed { (last, selection.start) } else { (selection.start, last) };
            }
        }
        let g = std::mem::take(&mut self.g);
        if !matches!(key, Key::Char('j' | 'k')) {
            self.want_col = None;
        }
        let ch = match key {
            Key::Esc => return self.leave_visual(t),
            Key::CtrlR => return Effect::default(),
            Key::Char(ch) => ch,
        };
        if g {
            return if ch == 'g' {
                let target = self.target(Motion::Top, t, self.head, false);
                self.head = clamp(t, target);
                self.select_visual(t)
            } else {
                self.select_visual(t)
            };
        }
        if ch == 'g' {
            self.g = true;
            return self.select_visual(t);
        }
        if let Some(motion) = Motion::of(ch) {
            let target = self.target(motion, t, self.head, false);
            self.head = clamp(t, target);
            return self.select_visual(t);
        }
        let span = |this: &Self| match this.mode {
            Mode::VisualLine => {
                Span::Lines(line_start(t, this.anchor.min(this.head)), line_end(t, this.anchor.max(this.head)))
            }
            _ => {
                let hi = this.anchor.max(this.head);
                Span::Chars(this.anchor.min(this.head)..hi + width(at(t, hi)))
            }
        };
        let cursor = self.anchor.min(self.head);
        match ch {
            'd' | 'x' => self.finish(Op::Delete, span(self), t, cursor),
            'y' => self.finish(Op::Yank, span(self), t, cursor),
            'c' | 's' => self.finish(Op::Change, span(self), t, cursor),
            'v' if self.mode == Mode::Visual => self.leave_visual(t),
            'V' if self.mode == Mode::VisualLine => self.leave_visual(t),
            'v' => {
                self.mode = Mode::Visual;
                self.select_visual(t)
            }
            'V' => {
                self.mode = Mode::VisualLine;
                self.select_visual(t)
            }
            _ => self.select_visual(t),
        }
    }

    fn leave_visual(&mut self, t: &Rope) -> Effect {
        self.mode = Mode::Normal;
        self.applied = None;
        caret_effect(clamp(t, self.head))
    }

    /// The selection the anchor and the head make: the chars under both ends, or whole lines.
    fn select_visual(&mut self, t: &Rope) -> Effect {
        let (lo, hi) = (self.anchor.min(self.head), self.anchor.max(self.head));
        let range = if self.mode == Mode::VisualLine {
            let last = line_end(t, hi);
            line_start(t, lo)..last + usize::from(at(t, last) == Some('\n'))
        } else {
            lo..hi + width(at(t, hi))
        };
        self.applied = Some(range.clone());
        Effect { select: Some((range, self.head < self.anchor)), ..Effect::default() }
    }
}

fn caret_effect(cursor: usize) -> Effect {
    Effect { select: Some((cursor..cursor, false)), ..Effect::default() }
}

fn caret_of(selection: &Range<usize>, reversed: bool) -> usize {
    if reversed { selection.start } else { selection.end }
}

// ---- text helpers (byte offsets) -----------------------------------------------------------

fn at(t: &Rope, offset: usize) -> Option<char> {
    t.char_at(offset)
}

fn before(t: &Rope, offset: usize) -> Option<char> {
    if offset == 0 {
        None
    } else {
        t.chars_at(offset).reversed().next()
    }
}

fn width(c: Option<char>) -> usize {
    c.map_or(0, char::len_utf8)
}

fn line_start(t: &Rope, offset: usize) -> usize {
    let mut start = offset;
    for c in t.chars_at(offset).reversed() {
        if c == '\n' {
            break;
        }
        start -= c.len_utf8();
    }
    start
}

fn line_end(t: &Rope, offset: usize) -> usize {
    let mut end = offset;
    for c in t.chars_at(offset) {
        if c == '\n' {
            break;
        }
        end += c.len_utf8();
    }
    end
}

/// The last char of the line `start..end`, or `end` on an empty line.
fn last_char(t: &Rope, start: usize, end: usize) -> usize {
    if end > start { end - width(before(t, end)) } else { end }
}

/// A Normal-mode cursor sits on a char, not after the last one of its line.
fn clamp(t: &Rope, offset: usize) -> usize {
    let offset = offset.min(t.len());
    let start = line_start(t, offset);
    if offset > start && matches!(at(t, offset), None | Some('\n')) {
        offset - width(before(t, offset))
    } else {
        offset
    }
}

/// The cursor after `start..end` was cut out, as `clamp` would place it in the shortened text.
fn clamp_after_cut(t: &Rope, start: usize, end: usize) -> usize {
    if start > line_start(t, start) && matches!(at(t, end), None | Some('\n')) {
        start - width(before(t, start))
    } else {
        start
    }
}

fn first_non_blank(t: &Rope, start: usize) -> usize {
    let mut offset = start;
    for c in t.chars_at(start) {
        if c != ' ' && c != '\t' {
            break;
        }
        offset += 1;
    }
    offset
}

/// The column (in chars) of `offset` on the line starting at `start`.
fn column(t: &Rope, start: usize, offset: usize) -> usize {
    let mut at_offset = start;
    let mut col = 0;
    for c in t.chars_at(start) {
        if at_offset >= offset {
            break;
        }
        at_offset += c.len_utf8();
        col += 1;
    }
    col
}

/// The offset of column `col` on the line starting at `start` (its end when the line is shorter).
fn offset_at_column(t: &Rope, start: usize, col: usize) -> usize {
    let mut offset = start;
    for (index, c) in t.chars_at(start).enumerate() {
        if index >= col || c == '\n' {
            break;
        }
        offset += c.len_utf8();
    }
    offset
}

/// 0 blank, 1 punctuation, 2 word chars.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if c.is_alphanumeric() || c == '_' {
        2
    } else {
        1
    }
}

/// The end of the run of chars of the class `c` is on (exclusive).
fn run_end(t: &Rope, c: usize) -> usize {
    let Some(first) = at(t, c) else { return c };
    let cls = class(first);
    let mut offset = c;
    for x in t.chars_at(c) {
        if class(x) != cls {
            break;
        }
        offset += x.len_utf8();
    }
    offset
}

/// `w`: the start of the next word; an empty line counts as a word.
fn word_forward(t: &Rope, c: usize) -> usize {
    let mut offset = if at(t, c).is_some_and(|x| class(x) != 0) { run_end(t, c) } else { c };
    while let Some(x) = at(t, offset) {
        if x == '\n' {
            // The next line is empty: stop there.
            if at(t, offset + 1) == Some('\n') {
                return offset + 1;
            }
        } else if class(x) != 0 {
            break;
        }
        offset += x.len_utf8();
    }
    offset
}

/// `b`: the start of this word, or of the previous one.
fn word_back(t: &Rope, c: usize) -> usize {
    let Some(prev) = before(t, c) else { return 0 };
    let mut offset = c - prev.len_utf8();
    while offset > 0 && at(t, offset).is_some_and(|x| class(x) == 0) {
        // An empty line stops the walk.
        if at(t, offset) == Some('\n') && before(t, offset) == Some('\n') {
            return offset;
        }
        offset -= width(before(t, offset));
    }
    let Some(cls) = at(t, offset).map(class) else { return offset };
    while let Some(x) = before(t, offset) {
        if class(x) != cls {
            break;
        }
        offset -= x.len_utf8();
    }
    offset
}

/// `e`: the last char of this word, or of the next one.
fn word_end(t: &Rope, c: usize) -> usize {
    let mut offset = c + width(at(t, c));
    while at(t, offset).is_some_and(|x| class(x) == 0) {
        offset += width(at(t, offset));
    }
    if at(t, offset).is_none() {
        return offset - width(before(t, offset));
    }
    let end = run_end(t, offset);
    end - width(before(t, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter in miniature: applies each `Effect` to a string.
    struct Editor {
        text: String,
        selection: Range<usize>,
        reversed: bool,
        register: Register,
        vim: Vim,
        /// Snapshots for undo / redo.
        undo: Vec<(String, Range<usize>)>,
        redo: Vec<(String, Range<usize>)>,
    }

    const ESC: char = '\u{1b}';
    const CTRL_R: char = '\u{12}';

    impl Editor {
        fn new(text: &str, cursor: usize) -> Self {
            Self {
                text: text.into(),
                selection: cursor..cursor,
                reversed: false,
                register: Register::default(),
                vim: Vim::default(),
                undo: Vec::new(),
                redo: Vec::new(),
            }
        }

        fn keys(&mut self, keys: &str) -> &mut Self {
            for ch in keys.chars() {
                if self.vim.mode() == Mode::Insert && ch != ESC {
                    // Typing: the editor's own business.
                    let (start, end) = (self.selection.start, self.selection.end);
                    self.text.replace_range(start..end, &ch.to_string());
                    let cursor = start + ch.len_utf8();
                    self.selection = cursor..cursor;
                    continue;
                }
                let key = match ch {
                    ESC => Key::Esc,
                    CTRL_R => Key::CtrlR,
                    ch => Key::Char(ch),
                };
                let rope = Rope::from(self.text.as_str());
                let effect = self.vim.key(key, &rope, self.selection.clone(), self.reversed, &self.register);
                self.apply(effect);
            }
            self
        }

        fn apply(&mut self, effect: Effect) {
            if let Some((range, text)) = effect.edit {
                self.undo.push((self.text.clone(), self.selection.clone()));
                self.redo.clear();
                self.text.replace_range(range.clone(), &text);
                let end = range.start + text.len();
                self.selection = end..end;
            }
            if let Some((range, reversed)) = effect.select {
                assert!(self.text.is_char_boundary(range.start) && self.text.is_char_boundary(range.end));
                self.selection = range;
                self.reversed = reversed;
            }
            if let Some(register) = effect.register {
                self.register = register;
            }
            match effect.history {
                Some(History::Undo) => {
                    if let Some(previous) = self.undo.pop() {
                        self.redo.push((std::mem::replace(&mut self.text, previous.0), self.selection.clone()));
                        self.selection = previous.1;
                    }
                }
                Some(History::Redo) => {
                    if let Some(next) = self.redo.pop() {
                        self.undo.push((std::mem::replace(&mut self.text, next.0), self.selection.clone()));
                        self.selection = next.1;
                    }
                }
                None => {}
            }
        }

        fn cursor(&self) -> usize {
            self.selection.start
        }

        fn mode(&self) -> Mode {
            self.vim.mode()
        }
    }

    fn run(text: &str, cursor: usize, keys: &str) -> Editor {
        let mut editor = Editor::new(text, cursor);
        editor.keys(keys);
        editor
    }

    #[track_caller]
    fn check(text: &str, cursor: usize, keys: &str, want_text: &str, want_cursor: usize) {
        let editor = run(text, cursor, keys);
        assert_eq!(editor.text, want_text, "text after {keys:?} on {text:?}");
        assert_eq!(editor.cursor(), want_cursor, "cursor after {keys:?} on {text:?}");
    }

    #[test]
    fn starts_in_normal_and_insert_modes_switch() {
        assert_eq!(Vim::default().mode(), Mode::Normal);
        let editor = run("abc", 0, "i");
        assert_eq!(editor.mode(), Mode::Insert);
        let editor = run("abc", 0, &format!("ixy{ESC}"));
        assert_eq!(editor.text, "xyabc");
        assert_eq!(editor.mode(), Mode::Normal);
        // Escape steps back onto the last typed char.
        assert_eq!(editor.cursor(), 1);
    }

    #[test]
    fn escape_at_the_line_start_stays() {
        check("abc", 0, &format!("i{ESC}"), "abc", 0);
        check("ab\ncd", 3, &format!("i{ESC}"), "ab\ncd", 3);
    }

    #[test]
    fn insert_commands() {
        check("abc def", 4, &format!("aX{ESC}"), "abc dXef", 5);
        check("abc def", 4, &format!("AX{ESC}"), "abc defX", 7);
        check("  abc", 4, &format!("IX{ESC}"), "  Xabc", 2);
        check("ab\ncd", 1, &format!("oX{ESC}"), "ab\nX\ncd", 3);
        check("ab\ncd", 4, &format!("OX{ESC}"), "ab\nX\ncd", 3);
        check("ab", 0, &format!("oX{ESC}"), "ab\nX", 3);
    }

    #[test]
    fn a_on_an_empty_line_inserts_there() {
        check("a\n\nb", 2, &format!("aX{ESC}"), "a\nX\nb", 2);
    }

    #[test]
    fn letters_are_never_inserted_in_normal_mode() {
        for keys in ["z", "q", "Z", "1", ":", "/", "f", "t", ".", "r", " ", "n", "m"] {
            let editor = run("abc", 1, keys);
            assert_eq!(editor.text, "abc", "{keys}");
            assert_eq!(editor.mode(), Mode::Normal, "{keys}");
        }
        // An operator followed by a key it does not know cancels and does not insert.
        check("abc", 0, "dz", "abc", 0);
        check("abc", 0, "dzx", "bc", 0);
    }

    #[test]
    fn h_and_l_stop_at_the_line_ends() {
        check("abc", 1, "h", "abc", 0);
        check("abc", 0, "h", "abc", 0);
        check("abc", 1, "l", "abc", 2);
        check("abc", 2, "l", "abc", 2);
        check("ab\ncd", 1, "l", "ab\ncd", 1);
        check("ab\ncd", 3, "h", "ab\ncd", 3);
        check("héllo", 0, "ll", "héllo", 3);
        check("héllo", 3, "h", "héllo", 1);
    }

    #[test]
    fn j_and_k_keep_the_column() {
        check("abcd\nxy\nabcd", 3, "j", "abcd\nxy\nabcd", 6);
        check("abcd\nxy\nabcd", 3, "jj", "abcd\nxy\nabcd", 11);
        check("abcd\nxy\nabcd", 11, "kk", "abcd\nxy\nabcd", 3);
        check("abcd", 2, "j", "abcd", 2);
        check("abcd", 2, "k", "abcd", 2);
        check("ab\n\ncd", 1, "j", "ab\n\ncd", 3);
        // A column in chars, not in bytes.
        check("éé\nabc", 2, "j", "éé\nabc", 6);
    }

    #[test]
    fn dollar_sticks_for_j() {
        check("abcd\nxy\nabcdef", 0, "$jj", "abcd\nxy\nabcdef", 13);
        check("abcd\nxy", 0, "$", "abcd\nxy", 3);
        check("\nxy", 0, "$", "\nxy", 0);
    }

    #[test]
    fn line_start_motions() {
        check("  abc d", 6, "0", "  abc d", 0);
        check("  abc d", 6, "^", "  abc d", 2);
        check("   ", 0, "^", "   ", 2);
        check("ab\n  cd", 6, "0", "ab\n  cd", 3);
    }

    #[test]
    fn gg_and_g() {
        check("  one\ntwo\nthree", 12, "gg", "  one\ntwo\nthree", 2);
        check("one\ntwo\n  three", 0, "G", "one\ntwo\n  three", 10);
        check("one", 1, "G", "one", 0);
        // A lone g waits, then another key cancels.
        check("one\ntwo", 5, "gx", "one\ntwo", 5);
        check("one\ntwo", 5, "gxgg", "one\ntwo", 0);
        check("", 0, "ggG", "", 0);
    }

    #[test]
    fn w_moves_to_the_next_word() {
        check("foo bar", 0, "w", "foo bar", 4);
        check("foo.bar", 0, "w", "foo.bar", 3);
        check("foo.bar", 0, "ww", "foo.bar", 4);
        check("foo  bar baz", 0, "ww", "foo  bar baz", 9);
        check("foo\nbar", 0, "w", "foo\nbar", 4);
        check("foo\n\nbar", 0, "w", "foo\n\nbar", 4);
        check("foo\n\nbar", 0, "ww", "foo\n\nbar", 5);
        check("foo", 0, "w", "foo", 2);
        check("a é", 0, "w", "a é", 2);
    }

    #[test]
    fn b_moves_to_the_word_start() {
        check("foo bar", 6, "b", "foo bar", 4);
        check("foo bar", 4, "b", "foo bar", 0);
        check("foo bar", 5, "b", "foo bar", 4);
        check("foo.bar", 4, "b", "foo.bar", 3);
        check("foo\nbar", 4, "b", "foo\nbar", 0);
        check("foo\n\nbar", 5, "b", "foo\n\nbar", 4);
        check("foo\n\nbar", 5, "bb", "foo\n\nbar", 0);
        check("foo", 0, "b", "foo", 0);
    }

    #[test]
    fn e_moves_to_the_word_end() {
        check("foo bar", 0, "e", "foo bar", 2);
        check("foo bar", 2, "e", "foo bar", 6);
        check("foo.bar", 0, "e", "foo.bar", 2);
        check("foo.bar", 2, "e", "foo.bar", 3);
        check("foo\nbar", 2, "e", "foo\nbar", 6);
        check("foo", 2, "e", "foo", 2);
        check("a  b", 0, "e", "a  b", 3);
    }

    #[test]
    fn x_deletes_the_char_under_the_cursor() {
        check("abc", 0, "x", "bc", 0);
        check("abc", 1, "x", "ac", 1);
        // On the last char of a line the caret steps back.
        check("abc", 2, "x", "ab", 1);
        check("héllo", 1, "x", "hllo", 1);
        check("ab\ncd", 1, "x", "a\ncd", 0);
        // Nothing to delete on an empty line or in an empty text.
        check("a\n\nb", 2, "x", "a\n\nb", 2);
        check("", 0, "x", "", 0);
    }

    #[test]
    fn x_fills_the_register() {
        let editor = run("abc", 0, "x");
        assert_eq!(editor.register, Register { text: "a".into(), linewise: false });
    }

    #[test]
    fn dd_deletes_the_line() {
        check("one\ntwo\nthree", 5, "dd", "one\nthree", 4);
        check("one\ntwo\nthree", 0, "dd", "two\nthree", 0);
        // The last line takes the newline before it.
        check("one\ntwo\nthree", 10, "dd", "one\ntwo", 4);
        check("one", 1, "dd", "", 0);
        check("", 0, "dd", "", 0);
        check("one\n  two", 0, "dd", "  two", 2);
        check("a\n\nb", 2, "dd", "a\nb", 2);
        let editor = run("one\ntwo", 0, "dd");
        assert_eq!(editor.register, Register { text: "one\n".into(), linewise: true });
    }

    #[test]
    fn yy_and_p() {
        check("one\ntwo", 0, "yyp", "one\none\ntwo", 4);
        check("one\ntwo", 0, "yyP", "one\none\ntwo", 0);
        check("one\ntwo", 4, "yyp", "one\ntwo\ntwo", 8);
        check("one\ntwo", 0, "jyyP", "one\ntwo\ntwo", 4);
        check("one\ntwo", 0, "ddp", "two\none", 4);
        check("  one\ntwo", 0, "yyp", "  one\n  one\ntwo", 8);
        let editor = run("one\ntwo", 0, "yy");
        assert_eq!(editor.register, Register { text: "one\n".into(), linewise: true });
        assert_eq!(editor.text, "one\ntwo");
    }

    #[test]
    fn put_after_the_last_line_without_a_newline() {
        check("one", 0, "yyp", "one\none", 4);
    }

    #[test]
    fn charwise_put() {
        check("abc def", 0, "dwp", "dabc ef", 4);
        check("abc def", 0, "yeP", "abcabc def", 2);
        check("abc def", 0, "yep", "aabcbc def", 3);
        check("ab", 1, "xp", "ab", 1);
        check("ab", 0, "xp", "ba", 1);
        // Multi-line text leaves the cursor at its start.
        check("a\nb c", 0, "vjy$P", "a\nba\nb c", 0);
        check("", 0, "p", "", 0);
    }

    #[test]
    fn u_and_ctrl_r() {
        check("abc", 0, "xxu", "bc", 0);
        check("abc", 0, "xxuu", "abc", 0);
        check("abc", 0, &format!("xxuu{CTRL_R}"), "bc", 0);
        check("abc", 0, &format!("xxuu{CTRL_R}{CTRL_R}"), "c", 0);
        check("abc", 0, "u", "abc", 0);
    }

    #[test]
    fn d_with_motions() {
        check("foo bar baz", 0, "dw", "bar baz", 0);
        check("foo bar baz", 4, "dw", "foo baz", 4);
        check("foo bar", 4, "dw", "foo ", 3);
        check("foo\nbar", 0, "dw", "\nbar", 0);
        check("foo bar", 4, "db", "bar", 0);
        check("foo bar", 0, "de", " bar", 0);
        check("foo bar", 0, "dl", "oo bar", 0);
        check("foo", 2, "dl", "fo", 1);
        check("foo bar", 4, "dh", "foobar", 3);
        check("foo bar", 4, "d$", "foo ", 3);
        check("foo bar", 4, "d0", "bar", 0);
        check("  foo bar", 6, "d^", "  bar", 2);
        check("foo bar", 0, "D", "", 0);
        check("foo bar", 4, "D", "foo ", 3);
        check("one\ntwo\nthree", 5, "dj", "one", 0);
        check("one\ntwo\nthree", 5, "dk", "three", 0);
        check("one\ntwo\nthree", 5, "dgg", "three", 0);
        check("one\ntwo\nthree", 5, "dG", "one", 0);
        check("one\ntwo\nthree", 5, "dgx", "one\ntwo\nthree", 5);
    }

    #[test]
    fn d_registers_what_it_cut() {
        assert_eq!(run("foo bar", 0, "dw").register, Register { text: "foo ".into(), linewise: false });
        assert_eq!(run("a\nb\nc", 0, "dj").register, Register { text: "a\nb\n".into(), linewise: true });
    }

    #[test]
    fn y_with_motions_moves_nothing_but_copies() {
        let editor = run("foo bar", 0, "yw");
        assert_eq!(editor.text, "foo bar");
        assert_eq!(editor.register.text, "foo ");
        assert_eq!(editor.cursor(), 0);
        let editor = run("foo bar", 4, "y0");
        assert_eq!(editor.register.text, "foo ");
        assert_eq!(editor.cursor(), 0);
        let editor = run("foo bar", 0, "y$");
        assert_eq!(editor.register.text, "foo bar");
        let editor = run("foo bar", 0, "ye");
        assert_eq!(editor.register.text, "foo");
        let editor = run("a\nb\nc", 2, "yk");
        assert_eq!(editor.register, Register { text: "a\nb\n".into(), linewise: true });
        assert_eq!(editor.cursor(), 0);
        let editor = run("a\nb\nc", 0, "yj");
        assert_eq!(editor.cursor(), 0);
        let editor = run("a\nb\nc", 4, "ygg");
        assert_eq!(editor.register.text, "a\nb\nc\n");
    }

    #[test]
    fn yank_asks_for_the_clipboard_and_delete_does_not() {
        let rope = Rope::from("foo bar");
        let mut vim = Vim::default();
        vim.key(Key::Char('y'), &rope, 0..0, false, &Register::default());
        let effect = vim.key(Key::Char('w'), &rope, 0..0, false, &Register::default());
        assert!(effect.copy);
        assert!(effect.edit.is_none());
        vim.key(Key::Char('d'), &rope, 0..0, false, &Register::default());
        let effect = vim.key(Key::Char('w'), &rope, 0..0, false, &Register::default());
        assert!(!effect.copy);
        assert!(effect.edit.is_some());
    }

    #[test]
    fn c_with_motions_starts_an_insert() {
        let editor = run("foo bar", 0, &format!("cwX{ESC}"));
        assert_eq!(editor.text, "X bar");
        assert_eq!(editor.mode(), Mode::Normal);
        // `cw` stops at the end of the word, not at the next one.
        check("foo bar", 0, &format!("cwX{ESC}"), "X bar", 0);
        check("foo bar", 1, &format!("cwX{ESC}"), "fX bar", 1);
        check("foo bar", 3, &format!("cwX{ESC}"), "fooXbar", 3);
        check("a b", 0, &format!("cwX{ESC}"), "X b", 0);
        check("foo bar", 4, &format!("c$X{ESC}"), "foo X", 4);
        check("foo bar", 4, &format!("CX{ESC}"), "foo X", 4);
        check("foo bar", 4, &format!("cbX{ESC}"), "Xbar", 0);
        check("foo bar", 0, &format!("ceX{ESC}"), "X bar", 0);
        check("one\ntwo", 0, &format!("ccX{ESC}"), "X\ntwo", 0);
        check("one\ntwo", 4, &format!("ccX{ESC}"), "one\nX", 4);
        check("one\ntwo\nthree", 0, &format!("cjX{ESC}"), "X\nthree", 0);
    }

    #[test]
    fn cc_leaves_an_empty_line_in_insert_mode() {
        let editor = run("one\ntwo", 0, "cc");
        assert_eq!(editor.text, "\ntwo");
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.register, Register { text: "one\n".into(), linewise: true });
    }

    #[test]
    fn an_operator_waits_for_its_motion_and_escape_cancels() {
        let editor = run("foo bar", 0, &format!("d{ESC}w"));
        assert_eq!(editor.text, "foo bar");
        assert_eq!(editor.cursor(), 4);
        let editor = run("foo bar", 0, "dg");
        assert_eq!(editor.text, "foo bar");
        check("foo bar", 0, "dd", "", 0);
        // Another operator is not a motion.
        check("foo bar", 0, "dy", "foo bar", 0);
    }

    #[test]
    fn visual_motions_extend_the_selection() {
        let editor = run("foo bar", 0, "v");
        assert_eq!(editor.mode(), Mode::Visual);
        assert_eq!(editor.selection, 0..1);
        let editor = run("foo bar", 0, "vl");
        assert_eq!(editor.selection, 0..2);
        let editor = run("foo bar", 0, "vw");
        assert_eq!(editor.selection, 0..5);
        let editor = run("foo bar", 2, "vh");
        assert_eq!(editor.selection, 1..3);
        assert!(editor.reversed);
        let editor = run("foo bar", 2, "vhl");
        assert_eq!(editor.selection, 2..3);
        let editor = run("foo bar", 2, "vhhl");
        assert_eq!(editor.selection, 1..3);
        let editor = run("foo bar", 0, "v$");
        assert_eq!(editor.selection, 0..7);
        let editor = run("foo\nbar", 0, "vj");
        assert_eq!(editor.selection, 0..5);
    }

    #[test]
    fn visual_delete_yank_change() {
        check("foo bar", 0, "vld", "o bar", 0);
        check("foo bar", 0, "vlx", "o bar", 0);
        check("foo bar", 0, "vey", "foo bar", 0);
        assert_eq!(run("foo bar", 0, "vey").register.text, "foo");
        assert_eq!(run("foo bar", 0, "vey").mode(), Mode::Normal);
        check("foo bar", 4, "vlld", "foo ", 3);
        check("foo bar", 2, "vhhd", " bar", 0);
        check("foo bar", 0, &format!("vecX{ESC}"), "X bar", 0);
        assert_eq!(run("foo bar", 0, "vlc").mode(), Mode::Insert);
        check("foo bar", 0, "vd", "oo bar", 0);
        check("ab", 1, "vd", "a", 0);
        // The selection of a line end takes the newline with it.
        check("a\nb", 0, "vjd", "", 0);
    }

    #[test]
    fn visual_escape_and_toggles() {
        let editor = run("foo bar", 0, &format!("vl{ESC}"));
        assert_eq!(editor.mode(), Mode::Normal);
        assert_eq!(editor.selection, 1..1);
        assert_eq!(run("foo bar", 0, "vv").mode(), Mode::Normal);
        assert_eq!(run("foo bar", 0, "vV").mode(), Mode::VisualLine);
        assert_eq!(run("foo bar", 0, "Vv").mode(), Mode::Visual);
        assert_eq!(run("foo bar", 0, "VV").mode(), Mode::Normal);
        // Keys that are not Visual commands are swallowed.
        let editor = run("foo bar", 0, "vz");
        assert_eq!(editor.text, "foo bar");
        assert_eq!(editor.mode(), Mode::Visual);
        assert_eq!(editor.selection, 0..1);
    }

    #[test]
    fn visual_gg_and_g() {
        let editor = run("one\ntwo\nthree", 5, "vgg");
        assert_eq!(editor.selection, 0..6);
        assert!(editor.reversed);
        let editor = run("one\ntwo\nthree", 5, "vG");
        assert_eq!(editor.selection, 5..9);
    }

    #[test]
    fn visual_line_selects_whole_lines() {
        let editor = run("one\ntwo\nthree", 5, "V");
        assert_eq!(editor.mode(), Mode::VisualLine);
        assert_eq!(editor.selection, 4..8);
        let editor = run("one\ntwo\nthree", 5, "Vj");
        assert_eq!(editor.selection, 4..13);
        let editor = run("one\ntwo\nthree", 5, "Vk");
        assert_eq!(editor.selection, 0..8);
        assert!(editor.reversed);
        check("one\ntwo\nthree", 5, "Vd", "one\nthree", 4);
        check("one\ntwo\nthree", 5, "Vjd", "one", 0);
        check("one\ntwo\nthree", 5, "Vx", "one\nthree", 4);
        check("one\ntwo\nthree", 0, "Vjjd", "", 0);
        assert_eq!(run("one\ntwo\nthree", 5, "Vy").register, Register { text: "two\n".into(), linewise: true });
        check("one\ntwo\nthree", 5, &format!("VcX{ESC}"), "one\nX\nthree", 4);
        check("one\ntwo\nthree", 5, "Vyp", "one\ntwo\ntwo\nthree", 8);
    }

    #[test]
    fn a_selection_made_with_the_mouse_takes_over_visual_mode() {
        let mut editor = Editor::new("foo bar baz", 0);
        editor.keys("v");
        // The user drags the selection to 4..7.
        editor.selection = 4..7;
        editor.reversed = false;
        editor.keys("d");
        assert_eq!(editor.text, "foo  baz");
    }

    #[test]
    fn unicode_text() {
        check("héllo wörld", 0, "wx", "héllo örld", 7);
        check("日本語 テスト", 0, "dw", "テスト", 0);
        check("日本語", 0, "$x", "日本", 3);
        check("日本語", 0, "lyyP", "日本語\n日本語", 0);
        check("😀 a", 0, "dw", "a", 0);
        check("a😀b", 0, "lx", "ab", 1);
        check("a😀b", 0, "$bb", "a😀b", 0);
    }

    #[test]
    fn empty_text() {
        for keys in ["h", "l", "j", "k", "w", "b", "e", "0", "^", "$", "gg", "G", "x", "dd", "yy", "p", "P", "D", "u", "dw", "v", "V"] {
            let editor = run("", 0, keys);
            assert_eq!(editor.text, "", "{keys}");
            assert_eq!(editor.cursor(), 0, "{keys}");
        }
        check("", 0, &format!("iab{ESC}"), "ab", 1);
        check("", 0, &format!("oab{ESC}"), "\nab", 2);
        check("", 0, &format!("Oab{ESC}"), "ab\n", 1);
    }

    #[test]
    fn a_cursor_after_the_last_char_is_taken_back_onto_it() {
        // A click puts the caret at the line end; Normal commands act on the last char.
        check("abc", 3, "x", "ab", 1);
        check("abc", 3, "h", "abc", 1);
        check("abc def", 7, "dw", "abc de", 5);
    }

    #[test]
    fn a_normal_caret_after_the_last_char_belongs_on_it() {
        let rope = |text: &str| Rope::from(text);
        let vim = Vim::default();
        assert_eq!(vim.normal_caret(&rope("abc"), &(3..3)), Some(2));
        assert_eq!(vim.normal_caret(&rope("abc\ndef"), &(3..3)), Some(2));
        assert_eq!(vim.normal_caret(&rope("abc"), &(2..2)), None, "already on the last char");
        assert_eq!(vim.normal_caret(&rope("abc\n"), &(4..4)), None, "an empty line has no char");
        assert_eq!(vim.normal_caret(&rope("abc"), &(1..3)), None, "a selection is the user's");
        let mut insert = Vim::default();
        insert.key(Key::Char('i'), &rope("abc"), 0..0, false, &Register::default());
        assert_eq!(insert.normal_caret(&rope("abc"), &(3..3)), None, "Insert may sit at the end");
    }

    #[test]
    fn a_long_text_is_edited_in_place() {
        let text = "line\n".repeat(20_000);
        let editor = run(&text, 0, "jjjdd");
        assert_eq!(editor.text.len(), text.len() - 5);
    }
}
