//! Vim keys for the query editors: a pure state machine. A key, the mode, the text and the
//! selection go in; the one edit, the new selection and the register change that follow come out
//! (`Effect`). `app::vim` applies them to the editor; nothing here knows GPUI.
//!
//! Supported: modes Normal / Insert / Visual (`v`) / Visual line (`V`).
//! - Counts everywhere a count makes sense: `3j`, `5dd`, `2w`, `d3w`, `3x`, `10G`, `3p`, `3ifoo`.
//! - Motions: `h j k l`, `w b e` and `W B E`, `0 ^ $`, `gg G` (a count is the line), `f F t T` with
//!   `;` `,`, `%` (matching bracket), `Ctrl-D` / `Ctrl-U` (half a page), `n N * #`, and `/` `?` with
//!   a prompt (`prompt()`: Enter runs it, Escape or Backspace on an empty one cancels it). Search is
//!   literal text (no regex); `*` and `#` match the word under the cursor as a whole word.
//! - Operators `d c y > <` with any motion (`dw`, `cw`, `dt,`, `cf)`, `d/foo`, `dgg`, `dj`) and
//!   doubled for lines (`dd`, `3yy`, `>>`); the text objects `iw aw iW aW`, `i" a" i' a' i\` a\``,
//!   `i( a( ib ab`, `i[ a[`, `i{ a{ iB aB`, `ip ap` after an operator and in Visual mode.
//! - Edits: `x X s S D C Y p P r~ J u Ctrl-R`, `.` (repeats the last change, with a new count if
//!   one is typed; an Insert session is repeated with the text that was typed), `i a I A o O`.
//! - `zz zt zb` (asks the editor to scroll). In Visual: motions and objects extend the selection;
//!   `d x y c s > < ~ J` act on it.
//!
//! Not supported (ignored in Normal mode, never inserted): regular expressions in search, macros,
//! registers by name, marks, `gv`, `Ctrl-V` block selection, `.` after a Visual-mode command or an
//! operator with `/`, counts for `u`. Never inserted holds for text that is not a key press too
//! (an input method's composition, dead keys, Option chords): `app::vim` makes the editor read-only
//! outside Insert mode, so its input handler refuses them.
//!
//! Offsets are byte offsets into the editor's rope. A command reads only the lines it touches (a
//! search reads the rope from the cursor on, chunk by chunk, until it matches), so it costs the
//! length of those lines, not of the buffer.

use gpui_component::input::{Rope, RopeExt};
use std::collections::VecDeque;
use std::ops::Range;

/// Largest count a command takes (`99999999dd` is `dd` to the end).
const MAX_COUNT: usize = 10_000_000;
/// The longest Insert session `.` remembers.
const MAX_INSERT_REPEAT: usize = 64 * 1024;

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
    /// Enter and Backspace matter to the search prompt only.
    Enter,
    Backspace,
    CtrlR,
    CtrlD,
    CtrlU,
}

/// What the editor around the machine knows and the machine needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Env {
    /// Lines of half a page (`Ctrl-D`, `Ctrl-U`).
    pub half_page: usize,
    pub ignore_case: bool,
    pub smart_case: bool,
    /// Columns of one `>` / `<` step.
    pub indent: usize,
}

impl Default for Env {
    fn default() -> Self {
        Self { half_page: 10, ignore_case: true, smart_case: true, indent: 2 }
    }
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

/// How a command wants the view moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// The cursor line to the middle / the top / the bottom of the view.
    Center,
    Top,
    Bottom,
    /// By this many lines (positive: the text moves up), as `Ctrl-D` / `Ctrl-U` do.
    Lines(isize),
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
    /// The register content is a yank (what "On yank" copies to the system clipboard).
    pub copy: bool,
    pub history: Option<History>,
    pub scroll: Option<Scroll>,
    /// The text a yank took, for the highlight (the text is unchanged: pre- and post-edit agree).
    pub highlight: Option<Range<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
    Indent,
    Outdent,
}

impl Op {
    fn of(ch: char) -> Option<Self> {
        match ch {
            'd' => Some(Self::Delete),
            'c' => Some(Self::Change),
            'y' => Some(Self::Yank),
            '>' => Some(Self::Indent),
            '<' => Some(Self::Outdent),
            _ => None,
        }
    }

    fn key(self) -> char {
        match self {
            Self::Delete => 'd',
            Self::Change => 'c',
            Self::Yank => 'y',
            Self::Indent => '>',
            Self::Outdent => '<',
        }
    }
}

/// `f` `F` `t` `T` with its char.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Find {
    forward: bool,
    till: bool,
    ch: char,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    Left,
    Right,
    Down,
    Up,
    /// `w` / `W` (the flag: WORDs, split on blanks only).
    Word(bool),
    WordBack(bool),
    WordEnd(bool),
    LineStart,
    FirstNonBlank,
    LineEnd,
    Top,
    Bottom,
    Find(Find),
    /// `;` (false) and `,` (true: the other way).
    FindRepeat(bool),
    Match,
    /// `n` (false) and `N` (true).
    Search(bool),
    /// `*` (true) and `#` (false).
    Star(bool),
    /// `Ctrl-D` (true) and `Ctrl-U`.
    Page(bool),
}

impl Motion {
    fn of(ch: char) -> Option<Self> {
        Some(match ch {
            'h' => Self::Left,
            'l' => Self::Right,
            'j' => Self::Down,
            'k' => Self::Up,
            'w' => Self::Word(false),
            'W' => Self::Word(true),
            'b' => Self::WordBack(false),
            'B' => Self::WordBack(true),
            'e' => Self::WordEnd(false),
            'E' => Self::WordEnd(true),
            '0' => Self::LineStart,
            '^' => Self::FirstNonBlank,
            '$' => Self::LineEnd,
            'G' => Self::Bottom,
            '%' => Self::Match,
            ';' => Self::FindRepeat(false),
            ',' => Self::FindRepeat(true),
            'n' => Self::Search(false),
            'N' => Self::Search(true),
            '*' => Self::Star(true),
            '#' => Self::Star(false),
            _ => return None,
        })
    }
}

/// Where a motion leads and what an operator takes of it.
struct Located {
    pos: usize,
    /// The char at `pos` belongs to the text an operator takes (`e`, `f`, `%`).
    inclusive: bool,
    /// Whole lines: `j`, `k`, `G`, `gg`.
    linewise: bool,
    /// Lines the view moves with it (`Ctrl-D`).
    scrolled: isize,
}

impl Located {
    fn to(pos: usize) -> Self {
        Self { pos, inclusive: false, linewise: false, scrolled: 0 }
    }

    fn through(pos: usize) -> Self {
        Self { inclusive: true, ..Self::to(pos) }
    }

    fn lines(pos: usize) -> Self {
        Self { linewise: true, ..Self::to(pos) }
    }
}

/// A text object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Obj {
    Word(bool),
    Quote(char),
    Bracket(char, char),
    Paragraph,
}

impl Obj {
    fn of(ch: char) -> Option<Self> {
        Some(match ch {
            'w' => Self::Word(false),
            'W' => Self::Word(true),
            '"' | '\'' | '`' => Self::Quote(ch),
            '(' | ')' | 'b' => Self::Bracket('(', ')'),
            '[' | ']' => Self::Bracket('[', ']'),
            '{' | '}' | 'B' => Self::Bracket('{', '}'),
            'p' => Self::Paragraph,
            _ => return None,
        })
    }
}

/// What a text object found.
enum Found {
    Chars(Range<usize>),
    /// Whole lines: the first line's start and the last line's end (before its newline).
    Lines(usize, usize),
}

/// What an operator works on.
enum Span {
    Chars(Range<usize>),
    /// Whole lines: the first line's start and the last line's end (before its newline).
    Lines(usize, usize),
}

/// A search: the text, the way `/` or `?` gave, and how it matches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Search {
    pattern: String,
    forward: bool,
    whole_word: bool,
    /// Smart case applies (not to `*` and `#`).
    smart: bool,
}

/// The `/` or `?` prompt being typed.
#[derive(Clone, Debug)]
struct Prompt {
    forward: bool,
    text: String,
}

/// The last change, for `.`.
#[derive(Clone, Debug)]
struct Repeat {
    /// The keys of the command without its count digits.
    keys: Vec<Key>,
    /// Its count (0: none typed).
    count: usize,
    /// What was typed in the Insert session it started.
    insert: Option<String>,
}

/// The Insert session going on.
#[derive(Clone, Debug, Default)]
struct Insert {
    /// Where the typing began.
    origin: usize,
    /// `3ifoo` types three times; `o` / `O` once per line.
    count: usize,
    lines: bool,
    /// The command that began it is what `.` repeats.
    repeat: bool,
}

/// What `parse` made of a key.
enum Parsed {
    /// More keys are due (a count, a prefix, an operator).
    Wait,
    Cancel,
    Motion(Motion),
    Object(Obj, bool),
    Replace(char),
    Scroll(Scroll),
    Cmd(char),
}

#[derive(Clone, Debug, Default)]
pub struct Vim {
    mode: Mode,
    // The command being typed.
    count: Option<usize>,
    op: Option<Op>,
    op_count: Option<usize>,
    /// A `g` / `z` waiting for the second key.
    g: bool,
    z: bool,
    /// An `i` (true) / `a` waiting for the object's key.
    obj: Option<bool>,
    /// An `f` / `t` waiting for its char: (forward, till).
    find: Option<(bool, bool)>,
    /// An `r` waiting for its char.
    replace: bool,
    prompt: Option<Prompt>,
    /// The keys of that command, count digits left out, for `.`.
    keys: Vec<Key>,
    /// The count of the command that just ran (0: none).
    cmd_count: usize,
    /// The command cannot be repeated (it used the search prompt).
    no_repeat: bool,
    replaying: bool,
    /// The fixed and the moving end of a Visual selection (the char under each is selected).
    anchor: usize,
    head: usize,
    /// The selection the machine last put in the editor: another one means the user moved it.
    applied: Option<Range<usize>>,
    /// The column `j` / `k` keep (`usize::MAX` after `$`).
    want_col: Option<usize>,
    last_find: Option<Find>,
    last_search: Option<Search>,
    last_change: Option<Repeat>,
    insert: Insert,
}

impl Vim {
    /// A machine that starts in `mode` (Normal or Insert).
    pub fn with_mode(mode: Mode) -> Self {
        Self { mode, ..Self::default() }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The search prompt being typed: its first char (`/` or `?`) and the text so far.
    pub fn prompt(&self) -> Option<(char, &str)> {
        self.prompt.as_ref().map(|prompt| (if prompt.forward { '/' } else { '?' }, prompt.text.as_str()))
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
        env: &Env,
    ) -> Effect {
        match self.mode {
            Mode::Insert => match key {
                Key::Esc => self.leave_insert(text, &selection, reversed),
                _ => Effect::default(),
            },
            Mode::Normal | Mode::Visual | Mode::VisualLine => {
                if self.mode != Mode::Normal {
                    self.sync_visual(text, &selection, reversed);
                }
                if self.prompt.is_some() {
                    return self.prompt_key(key, text, &selection, reversed, env);
                }
                if !matches!(key, Key::Char('j' | 'k' | '0'..='9') | Key::CtrlD | Key::CtrlU) {
                    self.want_col = None;
                }
                if self.mode == Mode::Normal {
                    self.normal(key, text, &selection, reversed, register, env)
                } else {
                    self.visual(key, text, env)
                }
            }
        }
    }

    /// The editor moved the caret in Insert mode (an arrow, Home, End, a click) to `caret`. As in
    /// Vim that ends the insert and starts another one there: the count of `3ifoo` is dropped and
    /// `.` repeats an `i` of what is typed from here on, never the text from before the move.
    pub fn insert_moved(&mut self, caret: usize) {
        if self.mode != Mode::Insert {
            return;
        }
        self.insert = Insert { origin: caret, count: 1, lines: false, repeat: self.insert.repeat };
        if self.insert.repeat {
            if let Some(last) = &mut self.last_change {
                *last = Repeat { keys: vec![Key::Char('i')], count: 0, insert: None };
            }
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

    // ---- the command being typed --------------------------------------------------------------

    /// The count of the command: the one before the operator times the one after it.
    fn count(&self) -> Option<usize> {
        match (self.count, self.op_count) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(1).saturating_mul(b.unwrap_or(1)).min(MAX_COUNT)),
        }
    }

    /// Ends the command's typing: its operator and count, which the caller runs.
    fn take_command(&mut self) -> (Option<Op>, Option<usize>) {
        let n = self.count();
        self.cmd_count = n.unwrap_or(0);
        self.count = None;
        self.op_count = None;
        (self.op.take(), n)
    }

    /// Forgets everything of the command typed so far.
    fn cancel(&mut self) {
        self.take_command();
        self.g = false;
        self.z = false;
        self.obj = None;
        self.find = None;
        self.replace = false;
        self.prompt = None;
        self.keys.clear();
        self.no_repeat = false;
    }

    /// Reads a key of a command: counts and prefixes are kept in `self` (`Wait`); what is left is a
    /// motion, an object, a char argument or a command key.
    fn parse(&mut self, ch: char, visual: bool) -> Parsed {
        let counting = if self.op.is_some() { self.op_count.is_some() } else { self.count.is_some() };
        let pending = self.find.is_some() || self.replace || self.obj.is_some() || self.g || self.z;
        let count_digit = !pending && ch.is_ascii_digit() && (ch != '0' || counting);
        if !count_digit && !self.replaying {
            self.keys.push(Key::Char(ch));
        }
        if let Some((forward, till)) = self.find.take() {
            return Parsed::Motion(Motion::Find(Find { forward, till, ch }));
        }
        if std::mem::take(&mut self.replace) {
            return Parsed::Replace(ch);
        }
        if let Some(inner) = self.obj.take() {
            return Obj::of(ch).map_or(Parsed::Cancel, |obj| Parsed::Object(obj, inner));
        }
        if std::mem::take(&mut self.g) {
            return if ch == 'g' { Parsed::Motion(Motion::Top) } else { Parsed::Cancel };
        }
        if std::mem::take(&mut self.z) {
            return match ch {
                'z' => Parsed::Scroll(Scroll::Center),
                't' => Parsed::Scroll(Scroll::Top),
                'b' => Parsed::Scroll(Scroll::Bottom),
                _ => Parsed::Cancel,
            };
        }
        if count_digit {
            let digit = ch as usize - '0' as usize;
            let slot = if self.op.is_some() { &mut self.op_count } else { &mut self.count };
            *slot = Some(slot.unwrap_or(0).saturating_mul(10).saturating_add(digit).min(MAX_COUNT));
            return Parsed::Wait;
        }
        match ch {
            'g' => {
                self.g = true;
                Parsed::Wait
            }
            'z' if self.op.is_none() => {
                self.z = true;
                Parsed::Wait
            }
            'f' | 'F' | 't' | 'T' => {
                self.find = Some((ch.is_ascii_lowercase(), ch.eq_ignore_ascii_case(&'t')));
                Parsed::Wait
            }
            'r' if !visual && self.op.is_none() => {
                self.replace = true;
                Parsed::Wait
            }
            'i' | 'a' if visual || self.op.is_some() => {
                self.obj = Some(ch == 'i');
                Parsed::Wait
            }
            _ => Motion::of(ch).map_or(Parsed::Cmd(ch), Parsed::Motion),
        }
    }

    /// A finished command: remembers it for `.` when it changed the text, and starts the next one.
    fn done(&mut self, effect: Effect) -> Effect {
        let keys = std::mem::take(&mut self.keys);
        let changed = effect.edit.is_some() || self.mode == Mode::Insert;
        if !self.replaying && !self.no_repeat && !keys.is_empty() && changed {
            self.last_change = Some(Repeat { keys, count: self.cmd_count, insert: None });
            self.insert.repeat = self.mode == Mode::Insert;
        }
        self.no_repeat = false;
        effect
    }

    fn open_prompt(&mut self, forward: bool) {
        self.prompt = Some(Prompt { forward, text: String::new() });
        self.keys.clear();
        self.no_repeat = true;
    }

    /// A key while the search prompt is open.
    fn prompt_key(
        &mut self,
        key: Key,
        t: &Rope,
        selection: &Range<usize>,
        reversed: bool,
        env: &Env,
    ) -> Effect {
        let Some(prompt) = self.prompt.as_mut() else {
            return Effect::default();
        };
        match key {
            Key::Char(ch) if !ch.is_control() => prompt.text.push(ch),
            Key::Backspace => {
                if prompt.text.pop().is_none() {
                    self.cancel();
                }
            }
            Key::Esc => self.cancel(),
            Key::Enter => {
                let prompt = self.prompt.take().unwrap_or(Prompt { forward: true, text: String::new() });
                if !prompt.text.is_empty() {
                    self.last_search = Some(Search {
                        pattern: prompt.text,
                        forward: prompt.forward,
                        whole_word: false,
                        smart: true,
                    });
                } else if let Some(last) = &mut self.last_search {
                    // An empty prompt repeats the last pattern, the way this one points.
                    last.forward = prompt.forward;
                }
                if self.last_search.is_none() {
                    self.cancel();
                    return Effect::default();
                }
                if self.mode == Mode::Normal {
                    let c = clamp(t, caret_of(selection, reversed));
                    return self.motion_cmd(Motion::Search(false), t, c, env);
                }
                return self.visual_motion(Motion::Search(false), t, env);
            }
            _ => {}
        }
        if self.mode == Mode::Normal { Effect::default() } else { self.select_visual(t) }
    }

    // ---- Normal ------------------------------------------------------------------------------

    fn leave_insert(&mut self, t: &Rope, selection: &Range<usize>, reversed: bool) -> Effect {
        self.mode = Mode::Normal;
        let caret = caret_of(selection, reversed);
        let session = std::mem::take(&mut self.insert);
        let typed = (session.origin <= caret
            && caret - session.origin <= MAX_INSERT_REPEAT
            && t.is_char_boundary(session.origin))
            .then(|| t.slice(session.origin..caret).to_string())
            .filter(|typed| !typed.is_empty());
        if session.repeat {
            if let Some(last) = &mut self.last_change {
                last.insert = typed.clone();
            }
        }
        // `3ifoo` types it again, `3ofoo` on new lines.
        if let Some(typed) = typed.filter(|_| session.count > 1) {
            let unit = if session.lines { format!("\n{typed}") } else { typed };
            let more = unit.repeat(session.count - 1);
            let end = caret + more.len();
            let step = more.chars().next_back().filter(|c| *c != '\n').map_or(0, char::len_utf8);
            return Effect {
                edit: Some((caret..caret, more)),
                select: Some((end - step..end - step, false)),
                ..Effect::default()
            };
        }
        // Vim steps back onto the last typed char.
        let cursor = if caret > line_start(t, caret) { caret - width(before(t, caret)) } else { caret };
        caret_effect(cursor)
    }

    fn begin_insert(&mut self, origin: usize, count: usize, lines: bool) {
        self.mode = Mode::Insert;
        self.insert = Insert { origin, count: count.max(1), lines, repeat: false };
    }

    fn normal(
        &mut self,
        key: Key,
        t: &Rope,
        selection: &Range<usize>,
        reversed: bool,
        register: &Register,
        env: &Env,
    ) -> Effect {
        let c = clamp(t, caret_of(selection, reversed));
        let ch = match key {
            Key::Esc | Key::Enter | Key::Backspace => {
                self.cancel();
                return Effect::default();
            }
            Key::CtrlR => {
                if self.op.is_some() || self.count.is_some() || self.g || self.z || self.find.is_some() || self.replace {
                    self.cancel();
                    return Effect::default();
                }
                return Effect { history: Some(History::Redo), ..Effect::default() };
            }
            Key::CtrlD | Key::CtrlU => {
                if self.op.is_some() || self.g || self.z || self.find.is_some() || self.replace || self.obj.is_some() {
                    self.cancel();
                    return Effect::default();
                }
                let effect = self.motion_cmd(Motion::Page(key == Key::CtrlD), t, c, env);
                return self.done(effect);
            }
            Key::Char(ch) => ch,
        };
        let effect = match self.parse(ch, false) {
            Parsed::Wait => return Effect::default(),
            Parsed::Cancel => {
                self.cancel();
                return Effect::default();
            }
            Parsed::Motion(motion) => self.motion_cmd(motion, t, c, env),
            Parsed::Scroll(scroll) => {
                self.take_command();
                Effect { scroll: Some(scroll), ..Effect::default() }
            }
            Parsed::Replace(ch) => {
                let (_, n) = self.take_command();
                if ch.is_control() { Effect::default() } else { replace_chars(ch, n.unwrap_or(1), t, c) }
            }
            Parsed::Object(obj, inner) => {
                let (op, n) = self.take_command();
                let found = op.and_then(|op| obj_range(obj, inner, n.unwrap_or(1), t, c).map(|found| (op, found)));
                match found {
                    Some((op, Found::Chars(range))) => self.finish(op, Span::Chars(range), t, c, env),
                    Some((op, Found::Lines(first, last))) => self.finish(op, Span::Lines(first, last), t, c, env),
                    None => Effect::default(),
                }
            }
            Parsed::Cmd(ch) => return self.normal_cmd(ch, t, c, register, selection, reversed, env),
        };
        self.done(effect)
    }

    /// A command key of Normal mode that is not a motion.
    #[allow(clippy::too_many_arguments)]
    fn normal_cmd(
        &mut self,
        ch: char,
        t: &Rope,
        c: usize,
        register: &Register,
        selection: &Range<usize>,
        reversed: bool,
        env: &Env,
    ) -> Effect {
        if let Some(op) = self.op {
            if ch == op.key() {
                let (_, n) = self.take_command();
                let effect = self.lines_op(op, n.unwrap_or(1), t, c, env);
                return self.done(effect);
            }
            if ch == '/' || ch == '?' {
                self.open_prompt(ch == '/');
                return Effect::default();
            }
            self.cancel();
            return Effect::default();
        }
        if let Some(op) = Op::of(ch) {
            self.op = Some(op);
            return Effect::default();
        }
        let effect = match ch {
            '/' | '?' => {
                self.open_prompt(ch == '/');
                return Effect::default();
            }
            '.' => {
                let (_, n) = self.take_command();
                self.keys.clear();
                return self.dot(n, t, selection, reversed, register, env);
            }
            'x' | 'X' | 's' | 'D' | 'C' => {
                let (op, motion) = match ch {
                    'x' => (Op::Delete, Motion::Right),
                    'X' => (Op::Delete, Motion::Left),
                    's' => (Op::Change, Motion::Right),
                    'D' => (Op::Delete, Motion::LineEnd),
                    _ => (Op::Change, Motion::LineEnd),
                };
                self.op = Some(op);
                self.motion_cmd(motion, t, c, env)
            }
            'S' | 'Y' => {
                let (_, n) = self.take_command();
                self.lines_op(if ch == 'S' { Op::Change } else { Op::Yank }, n.unwrap_or(1), t, c, env)
            }
            'p' | 'P' => {
                let (_, n) = self.take_command();
                self.put(t, c, register, ch == 'P', n.unwrap_or(1))
            }
            'J' => {
                let (_, n) = self.take_command();
                self.join(t, c, n.unwrap_or(2))
            }
            '~' => {
                let (_, n) = self.take_command();
                toggle_chars(n.unwrap_or(1), t, c)
            }
            'u' => {
                self.take_command();
                Effect { history: Some(History::Undo), ..Effect::default() }
            }
            'i' | 'a' | 'I' | 'A' | 'o' | 'O' => {
                let (_, n) = self.take_command();
                self.insert_cmd(ch, n.unwrap_or(1), t, c)
            }
            'v' | 'V' => {
                self.take_command();
                self.mode = if ch == 'v' { Mode::Visual } else { Mode::VisualLine };
                self.anchor = c;
                self.head = c;
                self.select_visual(t)
            }
            // Everything else is a command this machine does not have: ignored.
            _ => {
                self.cancel();
                return Effect::default();
            }
        };
        self.done(effect)
    }

    fn insert_cmd(&mut self, ch: char, count: usize, t: &Rope, c: usize) -> Effect {
        let eol = line_end(t, c);
        let on_char = matches!(at(t, c), Some(x) if x != '\n');
        match ch {
            'i' => self.insert_at(c, count),
            'a' => self.insert_at(if on_char { c + width(at(t, c)) } else { c }, count),
            'I' => self.insert_at(first_non_blank(t, line_start(t, c)), count),
            'A' => self.insert_at(eol, count),
            'o' => {
                self.begin_insert(eol + 1, count, true);
                Effect { edit: Some((eol..eol, "\n".into())), select: Some((eol + 1..eol + 1, false)), ..Effect::default() }
            }
            _ => {
                let start = line_start(t, c);
                self.begin_insert(start, count, true);
                Effect { edit: Some((start..start, "\n".into())), select: Some((start..start, false)), ..Effect::default() }
            }
        }
    }

    fn insert_at(&mut self, offset: usize, count: usize) -> Effect {
        self.begin_insert(offset, count, false);
        Effect { select: Some((offset..offset, false)), ..Effect::default() }
    }

    /// `.`: the last change again, as the keys that made it; `n` replaces its count.
    fn dot(
        &mut self,
        n: Option<usize>,
        t: &Rope,
        selection: &Range<usize>,
        reversed: bool,
        register: &Register,
        env: &Env,
    ) -> Effect {
        let Some(mut last) = self.last_change.clone() else {
            return Effect::default();
        };
        if let Some(n) = n {
            last.count = n;
        }
        self.replaying = true;
        let digits = if last.count > 0 { last.count.to_string() } else { String::new() };
        let mut effect = Effect::default();
        for key in digits.chars().map(Key::Char).chain(last.keys.iter().copied()) {
            effect = self.normal(key, t, selection, reversed, register, env);
        }
        self.replaying = false;
        self.last_change = Some(last.clone());
        if self.mode != Mode::Insert {
            return effect;
        }
        // The command began an Insert session: put the text that was typed in it and leave.
        let session = std::mem::take(&mut self.insert);
        self.mode = Mode::Normal;
        let typed = last.insert.unwrap_or_default();
        let mut text = typed.clone();
        if session.count > 1 && !typed.is_empty() {
            let unit = if session.lines { format!("\n{typed}") } else { typed };
            text.push_str(&unit.repeat(session.count - 1));
        }
        let caret = effect.select.as_ref().map_or_else(|| caret_of(selection, reversed), |(range, _)| range.start);
        let (edit, end, prev) = match effect.edit.take() {
            None => {
                let prev = text.chars().next_back().or_else(|| before(t, caret));
                let end = caret + text.len();
                ((!text.is_empty()).then_some((caret..caret, text)), end, prev)
            }
            Some((range, new)) => {
                // The caret sits inside or at the end of the text the edit put in.
                let mut k = caret.saturating_sub(range.start).min(new.len());
                while !new.is_char_boundary(k) {
                    k -= 1;
                }
                let prev = text.chars().next_back().or_else(|| new[..k].chars().next_back()).or_else(|| before(t, range.start));
                let end = range.start + k + text.len();
                let merged = format!("{}{}{}", &new[..k], text, &new[k..]);
                (Some((range, merged)), end, prev)
            }
        };
        let step = prev.filter(|c| *c != '\n').map_or(0, char::len_utf8);
        effect.edit = edit;
        effect.select = Some((end - step..end - step, false));
        effect
    }

    /// A motion in Normal mode: the cursor moves, or an operator takes the text up to it.
    fn motion_cmd(&mut self, motion: Motion, t: &Rope, c: usize, env: &Env) -> Effect {
        let (op, n) = self.take_command();
        let Some(found) = self.locate(motion, n, t, c, op, env) else {
            self.keys.clear();
            return Effect::default();
        };
        let Some(op) = op else {
            let scroll = (found.scrolled != 0).then_some(Scroll::Lines(found.scrolled));
            return Effect { scroll, ..caret_effect(clamp(t, found.pos)) };
        };
        if found.linewise {
            let (a, b) = (c.min(found.pos), c.max(found.pos));
            return self.finish(op, Span::Lines(line_start(t, a), line_end(t, b)), t, c, env);
        }
        let (a, mut b) = (c.min(found.pos), c.max(found.pos));
        if found.inclusive {
            b += width(at(t, b));
        } else if b > a && before(t, b) == Some('\n') {
            // An exclusive motion that ends in the first column stops at the end of the line before.
            b -= 1;
        }
        self.finish(op, Span::Chars(a..b), t, c, env)
    }

    /// Where `motion` leads from `c`; `None` when it fails (no such char, no match, no line to go to).
    fn locate(
        &mut self,
        motion: Motion,
        n: Option<usize>,
        t: &Rope,
        c: usize,
        op: Option<Op>,
        env: &Env,
    ) -> Option<Located> {
        let count = n.unwrap_or(1);
        self.want_col = match motion {
            Motion::Down | Motion::Up | Motion::Page(_) => {
                Some(self.want_col.unwrap_or_else(|| column(t, line_start(t, c), c)))
            }
            Motion::LineEnd => Some(usize::MAX),
            _ => None,
        };
        let ls = line_start(t, c);
        Some(match motion {
            Motion::Left => {
                let mut pos = c;
                for _ in 0..count {
                    if pos == ls {
                        break;
                    }
                    pos -= width(before(t, pos));
                }
                Located::to(pos)
            }
            Motion::Right => {
                let mut pos = c;
                for _ in 0..count {
                    match at(t, pos) {
                        Some(x) if x != '\n' => {
                            let next = pos + x.len_utf8();
                            // An operator may reach the line end (`dl`, `d$`); a cursor stops on the last char.
                            if op.is_some() || matches!(at(t, next), Some(y) if y != '\n') {
                                pos = next;
                            } else {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                Located::to(pos)
            }
            Motion::Down | Motion::Up | Motion::Page(_) => {
                let down = !matches!(motion, Motion::Up | Motion::Page(false));
                let lines = if matches!(motion, Motion::Page(_)) { n.unwrap_or(env.half_page).max(1) } else { count };
                let (mut line, mut moved) = (ls, 0isize);
                for _ in 0..lines {
                    if down {
                        let eol = line_end(t, line);
                        if at(t, eol) != Some('\n') {
                            break;
                        }
                        line = eol + 1;
                    } else {
                        if line == 0 {
                            break;
                        }
                        line = line_start(t, line - 1);
                    }
                    moved += 1;
                }
                if moved == 0 && op.is_some() {
                    return None;
                }
                let pos = if moved == 0 { c } else { offset_at_column(t, line, self.want_col.unwrap_or(0)) };
                let scrolled = if matches!(motion, Motion::Page(_)) { if down { moved } else { -moved } } else { 0 };
                Located { scrolled, ..Located::lines(pos) }
            }
            Motion::Word(big) => {
                if op == Some(Op::Change) && at(t, c).is_some_and(|x| class_of(x, big) != 0) {
                    // `cw` on a word changes up to the word's end, like `ce`.
                    let end = run_end(t, c, big);
                    let mut pos = end - width(before(t, end));
                    for _ in 1..count {
                        let next = word_end(t, pos, big);
                        if next == pos {
                            break;
                        }
                        pos = next;
                    }
                    Located::through(pos)
                } else {
                    let (mut pos, mut prev) = (c, c);
                    for _ in 0..count {
                        let next = word_forward(t, pos, big);
                        if next == pos {
                            break;
                        }
                        (prev, pos) = (pos, next);
                    }
                    // The last word on a line ends the text an operator takes: no `w` runs over the line end.
                    if op.is_some() && pos > prev && line_start(t, pos) != line_start(t, prev) {
                        pos = line_end(t, prev);
                    }
                    Located::to(pos)
                }
            }
            Motion::WordBack(big) => {
                let mut pos = c;
                for _ in 0..count {
                    let next = word_back(t, pos, big);
                    if next == pos {
                        break;
                    }
                    pos = next;
                }
                Located::to(pos)
            }
            Motion::WordEnd(big) => {
                let mut pos = c;
                for _ in 0..count {
                    let next = word_end(t, pos, big);
                    if next == pos {
                        break;
                    }
                    pos = next;
                }
                Located::through(pos)
            }
            Motion::LineStart => Located::to(ls),
            Motion::FirstNonBlank => Located::to(first_non_blank(t, ls)),
            Motion::LineEnd => {
                let mut eol = line_end(t, c);
                for _ in 1..count {
                    if at(t, eol) != Some('\n') {
                        break;
                    }
                    eol = line_end(t, eol + 1);
                }
                // An operator takes up to the line end; a cursor stands on the last char.
                Located::to(if op.is_some() { eol } else { last_char(t, line_start(t, eol), eol) })
            }
            Motion::Top | Motion::Bottom => {
                let last_row = t.offset_to_point(t.len()).row;
                let row = match (motion, n) {
                    (_, Some(n)) => (n - 1).min(last_row),
                    (Motion::Top, None) => 0,
                    _ => last_row,
                };
                Located::lines(first_non_blank(t, t.line_start_offset(row)))
            }
            Motion::Find(find) => {
                self.last_find = Some(find);
                Located { inclusive: find.forward, ..Located::to(find_char(t, c, find, count, false)?) }
            }
            Motion::FindRepeat(reverse) => {
                let mut find = self.last_find?;
                find.forward ^= reverse;
                // After `t` the next `;` goes on to the next match, not back onto the char before this one.
                Located { inclusive: find.forward, ..Located::to(find_char(t, c, find, count, find.till)?) }
            }
            Motion::Match => Located::through(match_bracket(t, c)?),
            Motion::Search(reverse) => {
                let search = self.last_search.clone()?;
                let forward = search.forward != reverse;
                let mut pos = c;
                for _ in 0..count {
                    pos = search_text(t, &search, pos, forward, env)?;
                }
                Located::to(pos)
            }
            Motion::Star(forward) => {
                let (start, end) = word_under(t, c)?;
                let search = Search { pattern: t.slice(start..end).to_string(), forward, whole_word: true, smart: false };
                let mut pos = start;
                for _ in 0..count {
                    pos = search_text(t, &search, pos, forward, env)?;
                }
                self.last_search = Some(search);
                Located::to(pos)
            }
        })
    }

    /// An operator on `n` lines from the cursor's (`dd`, `3yy`, `>>`, `S`).
    fn lines_op(&mut self, op: Op, n: usize, t: &Rope, c: usize, env: &Env) -> Effect {
        let first = line_start(t, c);
        let mut last = first;
        for _ in 1..n {
            let eol = line_end(t, last);
            if at(t, eol) != Some('\n') {
                break;
            }
            last = eol + 1;
        }
        self.finish(op, Span::Lines(first, line_end(t, last)), t, c, env)
    }

    /// Applies an operator to a span; `c` is the cursor it was typed at.
    fn finish(&mut self, op: Op, span: Span, t: &Rope, c: usize, env: &Env) -> Effect {
        self.mode = if op == Op::Change { Mode::Insert } else { Mode::Normal };
        self.applied = None;
        if matches!(op, Op::Indent | Op::Outdent) {
            let (first, last) = match span {
                Span::Lines(first, last) => (first, last),
                Span::Chars(range) => {
                    let end = if range.end > range.start { range.end - width(before(t, range.end)) } else { range.end };
                    (line_start(t, range.start), line_end(t, end.max(range.start)))
                }
            };
            return self.shift(op == Op::Indent, first, last, 1, t, env);
        }
        match span {
            Span::Chars(range) => {
                if range.is_empty() {
                    return match op {
                        Op::Change => self.insert_at(range.start, 1),
                        _ => Effect::default(),
                    };
                }
                let register = Register { text: t.slice(range.clone()).to_string(), linewise: false };
                let start = range.start;
                match op {
                    Op::Yank => Effect {
                        register: Some(register),
                        copy: true,
                        highlight: Some(range),
                        ..caret_effect(clamp(t, start))
                    },
                    _ => {
                        // The text after the cut moves up to `start`: on the line end the caret
                        // steps back (Normal only).
                        let cursor = if op == Op::Change { start } else { clamp_after_cut(t, start, range.end) };
                        if op == Op::Change {
                            self.begin_insert(start, 1, false);
                        }
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
                        Effect {
                            register: Some(register),
                            copy: true,
                            highlight: Some(first..last),
                            ..caret_effect(cursor)
                        }
                    }
                    Op::Change => {
                        self.begin_insert(first, 1, false);
                        Effect {
                            edit: Some((first..last, String::new())),
                            select: Some((first..first, false)),
                            register: Some(register),
                            ..Effect::default()
                        }
                    }
                    _ => {
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

    /// `>` / `<` on the lines `first..last` (`times` steps): one edit over those lines.
    fn shift(&mut self, right: bool, first: usize, last: usize, times: usize, t: &Rope, env: &Env) -> Effect {
        self.mode = Mode::Normal;
        self.applied = None;
        let step = env.indent.max(1);
        let amount = step.saturating_mul(times.max(1));
        let mut out = String::new();
        let mut cursor = 0;
        let mut start = first;
        loop {
            let eol = line_end(t, start);
            let body_start = first_non_blank(t, start);
            let blanks = t.slice(start..body_start).to_string();
            let body = t.slice(body_start..eol).to_string();
            let lead = if right {
                // An empty line is not indented.
                if blanks.is_empty() && body.is_empty() { blanks } else { format!("{blanks}{}", " ".repeat(amount)) }
            } else {
                let mut left = amount;
                let mut kept = blanks.as_str();
                while left > 0 {
                    match kept.chars().next() {
                        Some(' ') => left -= 1,
                        Some('\t') => left = left.saturating_sub(step),
                        _ => break,
                    }
                    kept = &kept[1..];
                }
                kept.to_string()
            };
            if start == first {
                cursor = lead.len();
            }
            out.push_str(&lead);
            out.push_str(&body);
            if eol >= last {
                break;
            }
            out.push('\n');
            start = eol + 1;
        }
        let at_first = first + cursor;
        if t.slice(first..last) == out.as_str() {
            return caret_effect(clamp(t, at_first));
        }
        Effect { edit: Some((first..last, out)), select: Some((at_first..at_first, false)), ..Effect::default() }
    }

    /// `J`: joins `n` lines (at least two) from the line of `c`.
    fn join(&mut self, t: &Rope, c: usize, n: usize) -> Effect {
        let first_eol = line_end(t, c);
        let mut out = String::new();
        let mut end = first_eol;
        let mut separator = 0;
        let mut last = if first_eol > line_start(t, c) { before(t, first_eol) } else { None };
        let mut joined = false;
        for _ in 1..n.max(2) {
            if at(t, end) != Some('\n') {
                break;
            }
            let next = end + 1;
            let body_start = first_non_blank(t, next);
            let next_eol = line_end(t, next);
            let body = t.slice(body_start..next_eol).to_string();
            // One space, unless the text before ends in a blank or is empty, or the next line is empty or starts with `)`.
            let space = !(body.is_empty() || body.starts_with(')') || matches!(last, None | Some(' ' | '\t')));
            separator = out.len();
            if space {
                out.push(' ');
            }
            out.push_str(&body);
            last = body.chars().next_back().or(last);
            end = next_eol;
            joined = true;
        }
        if !joined {
            return Effect::default();
        }
        // On the space that was put in, or where the lines met; on the last char if nothing was added there.
        let cursor = if out.len() > separator {
            first_eol + separator
        } else {
            first_eol + separator - out[..separator].chars().next_back().or_else(|| before(t, first_eol)).map_or(0, char::len_utf8)
        };
        Effect { edit: Some((first_eol..end, out)), select: Some((cursor..cursor, false)), ..Effect::default() }
    }

    fn put(&mut self, t: &Rope, c: usize, register: &Register, before_cursor: bool, count: usize) -> Effect {
        if register.text.is_empty() {
            return Effect::default();
        }
        let text = &register.text.repeat(count.max(1));
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

    /// A selection that is not the one put in the editor was made with the mouse: it is the new one.
    fn sync_visual(&mut self, t: &Rope, selection: &Range<usize>, reversed: bool) {
        if self.applied.as_ref() == Some(selection) {
            return;
        }
        if selection.is_empty() {
            self.anchor = selection.start;
            self.head = selection.start;
        } else {
            let last = selection.end - width(before(t, selection.end));
            (self.anchor, self.head) = if reversed { (last, selection.start) } else { (selection.start, last) };
        }
    }

    fn visual(&mut self, key: Key, t: &Rope, env: &Env) -> Effect {
        let ch = match key {
            Key::Esc => {
                self.cancel();
                return self.leave_visual(t);
            }
            Key::CtrlD | Key::CtrlU => {
                self.g = false;
                self.z = false;
                self.obj = None;
                self.find = None;
                return self.visual_motion(Motion::Page(key == Key::CtrlD), t, env);
            }
            Key::CtrlR | Key::Enter | Key::Backspace => {
                self.cancel();
                return self.select_visual(t);
            }
            Key::Char(ch) => ch,
        };
        match self.parse(ch, true) {
            Parsed::Wait => self.select_visual(t),
            Parsed::Cancel | Parsed::Replace(_) => {
                self.cancel();
                self.select_visual(t)
            }
            Parsed::Motion(motion) => self.visual_motion(motion, t, env),
            Parsed::Scroll(scroll) => {
                self.take_command();
                Effect { scroll: Some(scroll), ..self.select_visual(t) }
            }
            Parsed::Object(obj, inner) => {
                let (_, n) = self.take_command();
                self.keys.clear();
                match obj_range(obj, inner, n.unwrap_or(1), t, self.head) {
                    Some(Found::Chars(range)) if !range.is_empty() => {
                        self.anchor = range.start;
                        self.head = range.end - width(before(t, range.end));
                    }
                    Some(Found::Lines(first, last)) => {
                        self.mode = Mode::VisualLine;
                        self.anchor = first;
                        self.head = line_start(t, last);
                    }
                    _ => {}
                }
                self.select_visual(t)
            }
            Parsed::Cmd(ch) => self.visual_cmd(ch, t, env),
        }
    }

    fn visual_motion(&mut self, motion: Motion, t: &Rope, env: &Env) -> Effect {
        let (_, n) = self.take_command();
        self.keys.clear();
        let head = self.head;
        if let Some(found) = self.locate(motion, n, t, head, None, env) {
            self.head = clamp(t, found.pos);
            let scroll = (found.scrolled != 0).then_some(Scroll::Lines(found.scrolled));
            return Effect { scroll, ..self.select_visual(t) };
        }
        self.select_visual(t)
    }

    fn visual_span(&self, t: &Rope) -> Span {
        let (lo, hi) = (self.anchor.min(self.head), self.anchor.max(self.head));
        match self.mode {
            Mode::VisualLine => Span::Lines(line_start(t, lo), line_end(t, hi)),
            _ => Span::Chars(lo..hi + width(at(t, hi))),
        }
    }

    fn visual_cmd(&mut self, ch: char, t: &Rope, env: &Env) -> Effect {
        if ch == '/' || ch == '?' {
            self.open_prompt(ch == '/');
            return self.select_visual(t);
        }
        let (_, n) = self.take_command();
        self.keys.clear();
        let cursor = self.anchor.min(self.head);
        let (lo, hi) = (self.anchor.min(self.head), self.anchor.max(self.head));
        match ch {
            'd' | 'x' => self.finish(Op::Delete, self.visual_span(t), t, cursor, env),
            'y' => self.finish(Op::Yank, self.visual_span(t), t, cursor, env),
            'c' | 's' => self.finish(Op::Change, self.visual_span(t), t, cursor, env),
            '>' | '<' => {
                let (first, last) = (line_start(t, lo), line_end(t, hi));
                self.shift(ch == '>', first, last, n.unwrap_or(1), t, env)
            }
            '~' => {
                let range = match self.visual_span(t) {
                    Span::Chars(range) => range,
                    Span::Lines(first, last) => first..last,
                };
                self.mode = Mode::Normal;
                self.applied = None;
                let old = t.slice(range.clone()).to_string();
                let new = toggle_case(&old);
                if new == old {
                    return caret_effect(range.start);
                }
                Effect {
                    edit: Some((range.clone(), new)),
                    select: Some((range.start..range.start, false)),
                    ..Effect::default()
                }
            }
            'J' => {
                let rows = t.offset_to_point(hi).row - t.offset_to_point(lo).row + 1;
                self.mode = Mode::Normal;
                self.applied = None;
                self.join(t, lo, rows.max(2))
            }
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

// ---- edits that need no state ---------------------------------------------------------------

/// `r`: `n` chars from `c` become `ch`; fails when the line has fewer.
fn replace_chars(ch: char, n: usize, t: &Rope, c: usize) -> Effect {
    let (mut end, mut have) = (c, 0);
    for x in t.chars_at(c) {
        if x == '\n' || have == n {
            break;
        }
        end += x.len_utf8();
        have += 1;
    }
    if have < n || have == 0 {
        return Effect::default();
    }
    let new = ch.to_string().repeat(n);
    let cursor = c + new.len() - ch.len_utf8();
    Effect { edit: Some((c..end, new)), select: Some((cursor..cursor, false)), ..Effect::default() }
}

fn toggle_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_lowercase() {
            out.extend(ch.to_uppercase());
        } else if ch.is_uppercase() {
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

/// `~`: the case of `n` chars from `c` (to the line end) flips and the cursor moves past them.
fn toggle_chars(n: usize, t: &Rope, c: usize) -> Effect {
    let (mut end, mut taken) = (c, 0);
    for x in t.chars_at(c) {
        if x == '\n' || taken == n {
            break;
        }
        end += x.len_utf8();
        taken += 1;
    }
    if end == c {
        return Effect::default();
    }
    let old = t.slice(c..end).to_string();
    let new = toggle_case(&old);
    let cursor = if matches!(at(t, end), None | Some('\n')) {
        c + new.len() - new.chars().next_back().map_or(0, char::len_utf8)
    } else {
        c + new.len()
    };
    if new == old {
        return caret_effect(cursor);
    }
    Effect { edit: Some((c..end, new)), select: Some((cursor..cursor, false)), ..Effect::default() }
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

fn is_blank(c: Option<char>) -> bool {
    matches!(c, Some(' ' | '\t'))
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
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
    } else if is_word_char(c) {
        2
    } else {
        1
    }
}

/// `class`, or with `big` (WORDs) only blank and not blank.
fn class_of(c: char, big: bool) -> u8 {
    if big { u8::from(!c.is_whitespace()) } else { class(c) }
}

/// The end of the run of chars of the class `c` is on (exclusive).
fn run_end(t: &Rope, c: usize, big: bool) -> usize {
    let Some(first) = at(t, c) else { return c };
    let cls = class_of(first, big);
    let mut offset = c;
    for x in t.chars_at(c) {
        if class_of(x, big) != cls {
            break;
        }
        offset += x.len_utf8();
    }
    offset
}

/// The run of the same class that `c` is in, on its line.
fn run_bounds(t: &Rope, c: usize, big: bool) -> (usize, usize) {
    let Some(first) = at(t, c).filter(|x| *x != '\n') else {
        return (c, c);
    };
    let cls = class_of(first, big);
    let mut start = c;
    for x in t.chars_at(c).reversed() {
        if x == '\n' || class_of(x, big) != cls {
            break;
        }
        start -= x.len_utf8();
    }
    let mut end = c;
    for x in t.chars_at(c) {
        if x == '\n' || class_of(x, big) != cls {
            break;
        }
        end += x.len_utf8();
    }
    (start, end)
}

/// `w`: the start of the next word; an empty line counts as a word.
fn word_forward(t: &Rope, c: usize, big: bool) -> usize {
    let mut offset = if at(t, c).is_some_and(|x| class_of(x, big) != 0) { run_end(t, c, big) } else { c };
    while let Some(x) = at(t, offset) {
        if x == '\n' {
            // The next line is empty: stop there.
            if at(t, offset + 1) == Some('\n') {
                return offset + 1;
            }
        } else if class_of(x, big) != 0 {
            break;
        }
        offset += x.len_utf8();
    }
    offset
}

/// `b`: the start of this word, or of the previous one.
fn word_back(t: &Rope, c: usize, big: bool) -> usize {
    let Some(prev) = before(t, c) else { return 0 };
    let mut offset = c - prev.len_utf8();
    while offset > 0 && at(t, offset).is_some_and(|x| class_of(x, big) == 0) {
        // An empty line stops the walk.
        if at(t, offset) == Some('\n') && before(t, offset) == Some('\n') {
            return offset;
        }
        offset -= width(before(t, offset));
    }
    let Some(cls) = at(t, offset).map(|x| class_of(x, big)) else { return offset };
    while let Some(x) = before(t, offset) {
        if class_of(x, big) != cls {
            break;
        }
        offset -= x.len_utf8();
    }
    offset
}

/// `e`: the last char of this word, or of the next one.
fn word_end(t: &Rope, c: usize, big: bool) -> usize {
    let mut offset = c + width(at(t, c));
    while at(t, offset).is_some_and(|x| class_of(x, big) == 0) {
        offset += width(at(t, offset));
    }
    if at(t, offset).is_none() {
        return offset - width(before(t, offset));
    }
    let end = run_end(t, offset, big);
    end - width(before(t, end))
}

/// `f` `F` `t` `T`: the offset the `n`th `find.ch` on the line leads to; `skip` ignores a match
/// right next to the cursor (`;` after `t`).
fn find_char(t: &Rope, c: usize, find: Find, n: usize, skip: bool) -> Option<usize> {
    let mut seen = 0;
    if find.forward {
        let mut offset = c + width(at(t, c));
        if skip && at(t, offset) == Some(find.ch) {
            offset += find.ch.len_utf8();
        }
        for x in t.chars_at(offset) {
            if x == '\n' {
                break;
            }
            if x == find.ch {
                seen += 1;
                if seen == n {
                    return Some(if find.till { offset - width(before(t, offset)) } else { offset });
                }
            }
            offset += x.len_utf8();
        }
    } else {
        let mut offset = c;
        for x in t.chars_at(c).reversed() {
            if x == '\n' {
                break;
            }
            offset -= x.len_utf8();
            if skip && offset + x.len_utf8() == c && x == find.ch {
                continue;
            }
            if x == find.ch {
                seen += 1;
                if seen == n {
                    return Some(if find.till { offset + x.len_utf8() } else { offset });
                }
            }
        }
    }
    None
}

/// `%`: the bracket matching the first one at or after `c` on its line.
fn match_bracket(t: &Rope, c: usize) -> Option<usize> {
    let mut offset = c;
    let mut found = None;
    for x in t.chars_at(c) {
        if x == '\n' {
            break;
        }
        if "()[]{}".contains(x) {
            found = Some((offset, x));
            break;
        }
        offset += x.len_utf8();
    }
    let (at_bracket, bracket) = found?;
    let (open, close) = match bracket {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        _ => ('{', '}'),
    };
    if bracket == open {
        matching_close(t, at_bracket, open, close)
    } else {
        enclosing_open(t, at_bracket, open, close, true)
    }
}

/// The `close` that matches the `open` at `open_at`.
fn matching_close(t: &Rope, open_at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    let mut offset = open_at + open.len_utf8();
    for x in t.chars_at(offset) {
        if x == open {
            depth += 1;
        } else if x == close {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
        offset += x.len_utf8();
    }
    None
}

/// The `open` that encloses `c`; with `here`, one at `c` counts (a `close` at `c` is the end of the pair).
fn enclosing_open(t: &Rope, c: usize, open: char, close: char, here: bool) -> Option<usize> {
    if here && at(t, c) == Some(open) {
        return Some(c);
    }
    let mut depth = 0usize;
    let mut offset = c;
    for x in t.chars_at(c).reversed() {
        offset -= x.len_utf8();
        if x == close {
            depth += 1;
        } else if x == open {
            if depth == 0 {
                return Some(offset);
            }
            depth -= 1;
        }
    }
    None
}

// ---- text objects ---------------------------------------------------------------------------

fn obj_range(obj: Obj, inner: bool, n: usize, t: &Rope, c: usize) -> Option<Found> {
    match obj {
        Obj::Word(big) => word_object(t, c, inner, big, n).map(Found::Chars),
        Obj::Quote(quote) => quote_object(t, c, quote, inner).map(Found::Chars),
        Obj::Bracket(open, close) => bracket_object(t, c, open, close, inner, n),
        Obj::Paragraph => paragraph_object(t, c, inner, n).map(|(first, last)| Found::Lines(first, last)),
    }
}

/// `iw` / `aw` (and WORDs): the word under the cursor, with the blanks after it (or before it, when
/// nothing follows) for `a`; `n` words.
fn word_object(t: &Rope, c: usize, inner: bool, big: bool, n: usize) -> Option<Range<usize>> {
    let (ls, eol) = (line_start(t, c), line_end(t, c));
    if ls == eol {
        return None;
    }
    let (mut start, mut end) = run_bounds(t, c, big);
    if inner {
        for _ in 1..n {
            if end >= eol {
                break;
            }
            end = run_bounds(t, end, big).1;
        }
        return Some(start..end);
    }
    if is_blank(at(t, c)) {
        // Blanks, then the word after them.
        if end < eol {
            end = run_bounds(t, end, big).1;
        }
    } else if end < eol && is_blank(at(t, end)) {
        end = run_bounds(t, end, big).1;
    } else {
        while start > ls && is_blank(before(t, start)) {
            start -= 1;
        }
    }
    for _ in 1..n {
        if end >= eol {
            break;
        }
        end = run_bounds(t, end, big).1;
        if end < eol && is_blank(at(t, end)) {
            end = run_bounds(t, end, big).1;
        }
    }
    Some(start..end)
}

/// `i"` / `a"` and the other quotes: the pair on the cursor's line around (or after) the cursor.
fn quote_object(t: &Rope, c: usize, quote: char, inner: bool) -> Option<Range<usize>> {
    let ls = line_start(t, c);
    let mut quotes = Vec::new();
    let (mut offset, mut escaped) = (ls, false);
    for x in t.chars_at(ls) {
        if x == '\n' {
            break;
        }
        if escaped {
            escaped = false;
        } else if x == '\\' {
            escaped = true;
        } else if x == quote {
            quotes.push(offset);
        }
        offset += x.len_utf8();
    }
    let (open, close) = match quotes.iter().position(|&q| q == c) {
        // On a quote: it opens when an even number of quotes is before it.
        Some(i) if i % 2 == 0 => (quotes[i], *quotes.get(i + 1)?),
        Some(i) => (quotes[i - 1], quotes[i]),
        None => {
            let before_cursor = quotes.iter().filter(|&&q| q < c).count();
            if before_cursor % 2 == 1 {
                (quotes[before_cursor - 1], *quotes.get(before_cursor)?)
            } else {
                (*quotes.get(before_cursor)?, *quotes.get(before_cursor + 1)?)
            }
        }
    };
    let size = quote.len_utf8();
    if inner {
        return Some(open + size..close);
    }
    // The blanks after the closing quote, else the ones before the opening one.
    let mut end = close + size;
    while is_blank(at(t, end)) {
        end += 1;
    }
    if end > close + size {
        return Some(open..end);
    }
    let mut start = open;
    while start > ls && is_blank(before(t, start)) {
        start -= 1;
    }
    Some(start..close + size)
}

/// `i(` `a(` `i[` `i{` ...: the `n`th pair of brackets around the cursor. `i{` over several lines
/// takes the lines between the brackets, as Vim does when the brackets have their lines to themselves.
fn bracket_object(t: &Rope, c: usize, open: char, close: char, inner: bool, n: usize) -> Option<Found> {
    let mut open_at = enclosing_open(t, c, open, close, true)?;
    for _ in 1..n {
        open_at = enclosing_open(t, open_at, open, close, false)?;
    }
    let close_at = matching_close(t, open_at, open, close)?;
    if !inner {
        return Some(Found::Chars(open_at..close_at + close.len_utf8()));
    }
    let first = open_at + open.len_utf8();
    if at(t, first) == Some('\n') {
        let close_line = line_start(t, close_at);
        if first_non_blank(t, close_line) == close_at {
            return Some(if close_line > first + 1 {
                Found::Lines(first + 1, close_line - 1)
            } else {
                Found::Chars(first..first)
            });
        }
    }
    Some(Found::Chars(first..close_at))
}

fn blank_line(t: &Rope, start: usize) -> bool {
    first_non_blank(t, start) == line_end(t, start)
}

/// The end of the lines from `last` on that are blank (`blank`) or not.
fn block_end(t: &Rope, mut last: usize, blank: bool) -> usize {
    while at(t, last) == Some('\n') {
        let next = last + 1;
        if blank_line(t, next) != blank {
            break;
        }
        last = line_end(t, next);
    }
    last
}

/// `ip` / `ap`: the lines of the paragraph (or the blank lines) the cursor is in; `ap` takes the
/// blank lines after a paragraph (before it, at the end of the text) or the paragraph after blanks.
fn paragraph_object(t: &Rope, c: usize, inner: bool, n: usize) -> Option<(usize, usize)> {
    let start = line_start(t, c);
    let blank = blank_line(t, start);
    let mut first = start;
    while first > 0 {
        let prev = line_start(t, first - 1);
        if blank_line(t, prev) != blank {
            break;
        }
        first = prev;
    }
    let mut last = block_end(t, line_end(t, start), blank);
    let extend = |last: &mut usize, first: &mut usize, blank: bool| {
        if at(t, *last) != Some('\n') {
            // Nothing follows: `ap` of a paragraph takes the blank lines before it.
            if !inner && !blank {
                while *first > 0 {
                    let prev = line_start(t, *first - 1);
                    if !blank_line(t, prev) {
                        break;
                    }
                    *first = prev;
                }
            }
            return false;
        }
        let next = *last + 1;
        let next_blank = blank_line(t, next);
        *last = block_end(t, line_end(t, next), next_blank);
        true
    };
    if !inner {
        extend(&mut last, &mut first, blank);
    }
    for _ in 1..n {
        let next_blank = at(t, last) == Some('\n') && blank_line(t, last + 1);
        if !extend(&mut last, &mut first, next_blank) {
            break;
        }
        if !inner {
            extend(&mut last, &mut first, next_blank);
        }
    }
    Some((first, last))
}

// ---- search ---------------------------------------------------------------------------------

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// A pattern prepared for a scan (Knuth-Morris-Pratt, over chars): reversed for a backward scan.
struct Needle {
    chars: Vec<char>,
    fail: Vec<usize>,
    fold: bool,
}

impl Needle {
    fn new(pattern: &str, fold_case: bool, reversed: bool) -> Option<Self> {
        let mut chars: Vec<char> = pattern.chars().map(|c| if fold_case { fold(c) } else { c }).collect();
        if chars.is_empty() {
            return None;
        }
        if reversed {
            chars.reverse();
        }
        let mut fail = vec![0; chars.len()];
        let mut k = 0;
        for i in 1..chars.len() {
            while k > 0 && chars[i] != chars[k] {
                k = fail[k - 1];
            }
            if chars[i] == chars[k] {
                k += 1;
            }
            fail[i] = k;
        }
        Some(Self { chars, fail, fold: fold_case })
    }
}

/// The first match of `needle` found scanning the rope from `from`, forward or backward (the
/// needle is reversed then), that `accept(start, end)` takes; `None` when the scan reaches the
/// end, or (forward) no match can start at or before `stop` any more, or (backward) the scan is
/// left of `stop`. Each char is read once, from the rope's chunks; nothing is copied.
fn scan(
    t: &Rope,
    needle: &Needle,
    from: usize,
    forward: bool,
    stop: usize,
    accept: &dyn Fn(usize, usize) -> bool,
) -> Option<usize> {
    let m = needle.chars.len();
    let mut k = 0;
    // The (start, end) of the chars matched so far.
    let mut spans: VecDeque<(usize, usize)> = VecDeque::with_capacity(m);
    let mut offset = from;
    let mut step = |x: char, start: usize, end: usize, spans: &mut VecDeque<(usize, usize)>| -> Option<(usize, usize)> {
        let x = if needle.fold { fold(x) } else { x };
        while k > 0 && needle.chars[k] != x {
            let next = needle.fail[k - 1];
            for _ in 0..k - next {
                spans.pop_front();
            }
            k = next;
        }
        if needle.chars[k] != x {
            return None;
        }
        spans.push_back((start, end));
        k += 1;
        if k < m {
            return None;
        }
        // A match; the scan goes on from the longest border of it if it is not taken.
        let (first, last) = (spans.front().copied()?, spans.back().copied()?);
        let next = needle.fail[m - 1];
        let found = if forward { (first.0, last.1) } else { (last.0, first.1) };
        for _ in 0..k - next {
            spans.pop_front();
        }
        k = next;
        Some(found)
    };
    if forward {
        for x in t.chars_at(from) {
            let w = x.len_utf8();
            if spans.front().map_or(offset, |span| span.0) > stop {
                return None;
            }
            if let Some((start, end)) = step(x, offset, offset + w, &mut spans) {
                if accept(start, end) {
                    return Some(start);
                }
            }
            offset += w;
        }
    } else {
        for x in t.chars_at(from).reversed() {
            let w = x.len_utf8();
            offset -= w;
            if offset < stop {
                return None;
            }
            if let Some((start, end)) = step(x, offset, offset + w, &mut spans) {
                if accept(start, end) {
                    return Some(start);
                }
            }
        }
    }
    None
}

/// Where `search` matches next from `c`, wrapping around the text: after `c` going forward, before
/// it going back. A match that is only the one at `c` wraps onto itself.
fn search_text(t: &Rope, search: &Search, c: usize, forward: bool, env: &Env) -> Option<usize> {
    let fold_case = env.ignore_case && !(search.smart && env.smart_case && search.pattern.chars().any(char::is_uppercase));
    let needle = Needle::new(&search.pattern, fold_case, !forward)?;
    let whole = search.whole_word;
    let whole_word = |start: usize, end: usize| {
        !whole || (!before(t, start).is_some_and(is_word_char) && !at(t, end).is_some_and(is_word_char))
    };
    let len = t.len();
    if forward {
        let from = (c + width(at(t, c))).min(len);
        scan(t, &needle, from, true, usize::MAX, &whole_word).or_else(|| scan(t, &needle, 0, true, c, &whole_word))
    } else {
        // A match that starts before `c` may end after it.
        let mut from = c;
        for _ in 1..needle.chars.len() {
            if from >= len {
                break;
            }
            from += width(at(t, from));
        }
        let first = if c > 0 {
            scan(t, &needle, from.min(len), false, 0, &|start, end| start < c && whole_word(start, end))
        } else {
            None
        };
        first.or_else(|| scan(t, &needle, len, false, c, &whole_word))
    }
}

/// The word (letters, digits, `_`) at `c`, or the next one on the line.
fn word_under(t: &Rope, c: usize) -> Option<(usize, usize)> {
    let mut offset = c;
    for x in t.chars_at(c) {
        if x == '\n' || is_word_char(x) {
            break;
        }
        offset += x.len_utf8();
    }
    if !at(t, offset).is_some_and(is_word_char) {
        return None;
    }
    let mut start = offset;
    for x in t.chars_at(offset).reversed() {
        if !is_word_char(x) {
            break;
        }
        start -= x.len_utf8();
    }
    let mut end = offset;
    for x in t.chars_at(offset) {
        if !is_word_char(x) {
            break;
        }
        end += x.len_utf8();
    }
    Some((start, end))
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
        env: Env,
        /// The last scroll and yank highlight an effect asked for.
        scroll: Option<Scroll>,
        highlight: Option<Range<usize>>,
    }

    const ESC: char = '\u{1b}';
    const CTRL_R: char = '\u{12}';
    const CTRL_D: char = '\u{4}';
    const CTRL_U: char = '\u{15}';
    /// Enter and Backspace, for the search prompt.
    const ENTER: char = '\n';
    const BS: char = '\u{8}';
    /// In Insert mode: the editor's End key (the caret goes to the end of the line).
    const END: char = '\u{5}';

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
                env: Env::default(),
                scroll: None,
                highlight: None,
            }
        }

        fn keys(&mut self, keys: &str) -> &mut Self {
            for ch in keys.chars() {
                if self.vim.mode() == Mode::Insert && ch == END {
                    let line_end = self.text[self.selection.end..].find('\n').map_or(self.text.len(), |at| self.selection.end + at);
                    self.selection = line_end..line_end;
                    self.vim.insert_moved(line_end);
                    continue;
                }
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
                    CTRL_D => Key::CtrlD,
                    CTRL_U => Key::CtrlU,
                    ENTER => Key::Enter,
                    BS => Key::Backspace,
                    ch => Key::Char(ch),
                };
                let rope = Rope::from(self.text.as_str());
                let effect =
                    self.vim.key(key, &rope, self.selection.clone(), self.reversed, &self.register, &self.env);
                self.apply(effect);
            }
            self
        }

        fn apply(&mut self, effect: Effect) {
            self.scroll = effect.scroll.or(self.scroll);
            self.highlight = effect.highlight.clone().or(self.highlight.take());
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
        let env = Env::default();
        vim.key(Key::Char('y'), &rope, 0..0, false, &Register::default(), &env);
        let effect = vim.key(Key::Char('w'), &rope, 0..0, false, &Register::default(), &env);
        assert!(effect.copy);
        assert!(effect.edit.is_none());
        vim.key(Key::Char('d'), &rope, 0..0, false, &Register::default(), &env);
        let effect = vim.key(Key::Char('w'), &rope, 0..0, false, &Register::default(), &env);
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
        insert.key(Key::Char('i'), &rope("abc"), 0..0, false, &Register::default(), &Env::default());
        assert_eq!(insert.normal_caret(&rope("abc"), &(3..3)), None, "Insert may sit at the end");
    }

    #[test]
    fn a_long_text_is_edited_in_place() {
        let text = "line\n".repeat(20_000);
        let editor = run(&text, 0, "jjjdd");
        assert_eq!(editor.text.len(), text.len() - 5);
    }

    // ---- counts ----------------------------------------------------------------------------

    #[test]
    fn counts_move_by_lines_chars_and_words() {
        check("a\nb\nc\nd", 0, "3j", "a\nb\nc\nd", 6);
        check("a\nb", 0, "5j", "a\nb", 2);
        check("a\nb\nc\nd\ne", 8, "3k", "a\nb\nc\nd\ne", 2);
        check("abcdef", 0, "3l", "abcdef", 3);
        check("abcdef", 0, "9l", "abcdef", 5);
        check("abcdef", 5, "3h", "abcdef", 2);
        check("a b c d", 0, "2w", "a b c d", 4);
        check("a b c d", 0, "3w", "a b c d", 6);
        check("a b c d", 6, "2b", "a b c d", 2);
        check("ab cd ef", 0, "2e", "ab cd ef", 4);
        check("ab\ncd", 0, "2$", "ab\ncd", 4);
        check("a\nb\nc", 0, "2G", "a\nb\nc", 2);
        check("a\nb\nc", 0, "9G", "a\nb\nc", 4);
        check("a\nb\nc", 4, "2gg", "a\nb\nc", 2);
        check("a\n  b\nc", 0, "2G", "a\n  b\nc", 4);
        // A count that is not a number of its own: 0 is the line start.
        check("abc def", 5, "0", "abc def", 0);
        check("a b c d e f g h i j k l", 0, "10w", "a b c d e f g h i j k l", 20);
    }

    #[test]
    fn counts_with_operators() {
        check("1\n2\n3\n4\n5\n6", 0, "5dd", "6", 0);
        check("1\n2\n3\n4", 2, "9dd", "1", 0);
        check("1\n2\n3\n4", 0, "2dd", "3\n4", 0);
        check("a b c d", 0, "d3w", "d", 0);
        check("a b c d", 0, "3dw", "d", 0);
        check("a b c d", 0, "2d2w", "", 0);
        check("abcdef", 1, "3x", "aef", 1);
        check("abc", 1, "9x", "a", 0);
        check("a\nb\nc\nd", 0, "d2j", "d", 0);
        check("abcdef", 4, "d3h", "aef", 1);
        check("1\n2\n3\n4", 0, "2yyjp", "1\n2\n1\n2\n3\n4", 4);
        assert_eq!(run("1\n2\n3", 0, "3yy").register.text, "1\n2\n3\n");
        // `3dd` on the last line with fewer lines takes what is there.
        check("a\nb", 2, "3dd", "a", 0);
    }

    #[test]
    fn put_with_a_count_repeats_the_text() {
        check("ab", 0, "yl3p", "aaaab", 3);
        check("x\ny", 0, "yy2p", "x\nx\nx\ny", 2);
        check("x\ny", 0, "yy3P", "x\nx\nx\nx\ny", 0);
    }

    #[test]
    fn a_count_repeats_an_insert() {
        check("x", 0, &format!("3ifoo{ESC}"), "foofoofoox", 8);
        check("x", 0, &format!("2Aab{ESC}"), "xabab", 4);
        check("a", 0, &format!("3ofoo{ESC}"), "a\nfoo\nfoo\nfoo", 12);
        check("x", 0, &format!("2ia{ESC}"), "aax", 1);
        // Nothing typed: nothing to repeat.
        check("x", 0, &format!("3i{ESC}"), "x", 0);
    }

    // ---- f F t T ; , -------------------------------------------------------------------------

    #[test]
    fn find_on_the_line() {
        check("hello world", 0, "fo", "hello world", 4);
        check("hello world", 0, "2fo", "hello world", 7);
        check("hello world", 0, "fo;", "hello world", 7);
        check("hello world", 0, "fo;,", "hello world", 4);
        check("hello world", 10, "Fo", "hello world", 7);
        check("hello world", 10, "2Fo", "hello world", 4);
        check("hello world", 0, "to", "hello world", 3);
        check("hello world", 10, "To", "hello world", 8);
        // `;` after `t` goes on to the next match.
        check("hello world", 0, "to;", "hello world", 6);
        check("hello world", 10, "To;", "hello world", 5);
        // Not on the line, or not enough of them: stays.
        check("hello\nworld", 0, "fw", "hello\nworld", 0);
        check("hello world", 0, "3fo", "hello world", 0);
        check("hello", 0, ";", "hello", 0);
        // Multibyte chars.
        check("đã,xong", 0, "f,", "đã,xong", 4);
        check("đã,xong", 0, "fo", "đã,xong", 6);
        check("a,b", 0, "fế", "a,b", 0);
    }

    #[test]
    fn find_as_an_operator_motion() {
        check("a, b", 0, "dt,", ", b", 0);
        check("ab,cd", 0, "df,", "cd", 0);
        check("ab,cd", 4, "dF,", "abd", 2);
        check("ab,cd", 4, "dT,", "ab,d", 3);
        check("f(x) y", 0, &format!("cf)Z{ESC}"), "Z y", 0);
        check("a,b,c", 0, "d2f,", "c", 0);
        check("a,b,c", 0, "df,.", "c", 0);
        check("a,b,c", 0, "dt,", ",b,c", 0);
        // `dt,` with the comma next to the cursor takes the char under it.
        check(",b", 0, "dt,", ",b", 0);
        check("a,b", 0, "dfz", "a,b", 0);
        check("x a;b;c", 2, "d;", "x a;b;c", 2);
        check("x a;b;c", 2, "f;d;", "x ac", 3);
    }

    // ---- search ------------------------------------------------------------------------------

    #[test]
    fn slash_searches_forward_and_wraps() {
        check("a foo b foo", 0, &format!("/foo{ENTER}"), "a foo b foo", 2);
        check("a foo b foo", 0, &format!("/foo{ENTER}n"), "a foo b foo", 8);
        check("a foo b foo", 0, &format!("/foo{ENTER}nn"), "a foo b foo", 2);
        check("a foo b foo", 0, &format!("/foo{ENTER}N"), "a foo b foo", 8);
        check("a foo b foo", 2, &format!("/foo{ENTER}"), "a foo b foo", 8);
        // The only match wraps onto itself.
        check("a foo b", 2, &format!("/foo{ENTER}"), "a foo b", 2);
        check("a foo b", 0, &format!("/zzz{ENTER}"), "a foo b", 0);
        check("a foo b", 0, &format!("/foo{ENTER}3n"), "a foo b", 2);
    }

    #[test]
    fn question_mark_searches_backward_and_wraps() {
        check("a foo b foo", 10, &format!("?foo{ENTER}"), "a foo b foo", 8);
        check("a foo b foo", 10, &format!("?foo{ENTER}n"), "a foo b foo", 2);
        check("a foo b foo", 10, &format!("?foo{ENTER}nn"), "a foo b foo", 8);
        check("a foo b foo", 10, &format!("?foo{ENTER}N"), "a foo b foo", 2);
        // Wraps to the last match from before the first one.
        check("a foo b foo", 0, &format!("?foo{ENTER}"), "a foo b foo", 8);
        // A match the cursor is inside of starts before it.
        check("a foo b", 3, &format!("?foo{ENTER}"), "a foo b", 2);
        check("foo", 0, &format!("?foo{ENTER}"), "foo", 0);
    }

    #[test]
    fn search_honours_ignorecase_and_smartcase() {
        let with = |ignore: bool, smart: bool, text: &str, keys: &str| {
            let mut editor = Editor::new(text, 0);
            editor.env.ignore_case = ignore;
            editor.env.smart_case = smart;
            editor.keys(keys);
            editor.cursor()
        };
        let foo = format!("/foo{ENTER}");
        let capital = format!("/Foo{ENTER}");
        assert_eq!(with(true, true, "x Foo foo", &foo), 2, "ignorecase");
        assert_eq!(with(false, true, "x Foo foo", &foo), 6, "case sensitive");
        assert_eq!(with(true, true, "x foo Foo", &capital), 6, "smartcase: a capital makes it exact");
        assert_eq!(with(true, false, "x foo Foo", &capital), 2, "without smartcase it is still ignored");
        assert_eq!(with(true, true, "x FOO foo", &foo), 2, "all lowercase pattern ignores case");
        // Non-ASCII letters fold too.
        assert_eq!(with(true, true, "xin Đức", &format!("/đ{ENTER}")), 4);
        assert_eq!(with(false, true, "xin Đức đ", &format!("/đ{ENTER}")), 11);
    }

    #[test]
    fn the_prompt_is_typed_edited_and_cancelled() {
        let mut editor = run("a foo b", 0, "/fo");
        assert_eq!(editor.vim.prompt(), Some(('/', "fo")));
        assert_eq!(editor.cursor(), 0);
        editor.keys(&format!("{BS}x"));
        assert_eq!(editor.vim.prompt(), Some(('/', "fx")));
        editor.keys(&format!("{BS}o{ENTER}"));
        assert_eq!(editor.vim.prompt(), None);
        assert_eq!(editor.cursor(), 2);
        let editor = run("a foo b", 0, &format!("?fo{ESC}"));
        assert_eq!(editor.vim.prompt(), None);
        assert_eq!(editor.cursor(), 0);
        assert_eq!(editor.mode(), Mode::Normal);
        // Backspace on an empty prompt closes it.
        let editor = run("a foo b", 0, &format!("/{BS}"));
        assert_eq!(editor.vim.prompt(), None);
        // Keys typed in the prompt are text, not commands.
        let editor = run("a dd b", 0, "/dd");
        assert_eq!(editor.text, "a dd b");
        assert_eq!(editor.vim.prompt(), Some(('/', "dd")));
        // Enter with nothing typed repeats the last search; with none, nothing happens.
        check("a foo b foo", 0, &format!("/foo{ENTER}/{ENTER}"), "a foo b foo", 8);
        check("a foo b foo", 0, &format!("/{ENTER}"), "a foo b foo", 0);
        check("a foo b", 0, "n", "a foo b", 0);
        // A prompt opened after `/` and cancelled leaves nothing pending.
        check("a foo b", 0, &format!("/x{ESC}w"), "a foo b", 2);
    }

    #[test]
    fn star_and_hash_search_the_word_as_a_whole() {
        let text = "foo bar foo barfoo foo";
        check(text, 0, "*", text, 8);
        check(text, 0, "**", text, 19);
        check(text, 0, "***", text, 0);
        check(text, 8, "#", text, 0);
        check(text, 0, "#", text, 19);
        check(text, 0, "*N", text, 0);
        check(text, 0, "2*", text, 19);
        // The cursor before the word: the next word on the line is the one.
        check("  foo x foo", 0, "*", "  foo x foo", 8);
        check("a.b", 1, "*", "a.b", 2);
        check("a.b", 0, "*", "a.b", 0);
        check("   ", 0, "*", "   ", 0);
        // `n` goes on the way `*` / `#` went.
        check(text, 0, "*n", text, 19);
        check(text, 8, "#n", text, 19);
        // A word of non-ASCII letters.
        check("chào bạn chào", 0, "*", "chào bạn chào", 12);
    }

    #[test]
    fn search_as_an_operator_motion() {
        check("a foo b", 0, &format!("d/foo{ENTER}"), "foo b", 0);
        check("a foo b foo", 2, &format!("d/foo{ENTER}"), "a foo", 2);
        check("a foo b foo", 0, &format!("/foo{ENTER}0dn"), "foo b foo", 0);
        check("a foo b", 0, &format!("c/foo{ENTER}X{ESC}"), "Xfoo b", 0);
        // An exclusive motion that ends in the first column stops before the newline.
        check("a\nfoo", 0, &format!("d/foo{ENTER}"), "\nfoo", 0);
        check("a foo b", 0, &format!("d/zzz{ENTER}"), "a foo b", 0);
        check("a foo b", 0, "d*", "a foo b", 0);
        check("foo b foo", 0, "d*", "foo", 0);
    }

    #[test]
    fn search_in_visual_mode_extends_the_selection() {
        let editor = run("a foo b", 0, &format!("v/foo{ENTER}"));
        assert_eq!(editor.mode(), Mode::Visual);
        assert_eq!(editor.selection, 0..3);
        let editor = run("a foo b foo", 0, &format!("v/foo{ENTER}n"));
        assert_eq!(editor.selection, 0..9);
        let editor = run("a foo b", 0, &format!("v/foo{ENTER}d"));
        assert_eq!(editor.text, "oo b");
    }

    #[test]
    fn search_reads_a_large_buffer_in_one_pass() {
        let text: String = (0..60_000).map(|i| format!("row {i} abcdefgh\n")).collect();
        let last = text.rfind("row 59999").unwrap();
        let started = std::time::Instant::now();
        let editor = run(&text, 0, &format!("/row 59999{ENTER}"));
        assert_eq!(editor.cursor(), last);
        // And back, wrapping, from the middle.
        let editor = run(&text, text.len() / 2, &format!("?row 5{ENTER}"));
        assert_eq!(editor.cursor(), text[..text.len() / 2].rfind("row 5").unwrap());
        // No match: both passes read the buffer once.
        let editor = run(&text, 0, &format!("/nothing here{ENTER}"));
        assert_eq!(editor.cursor(), 0);
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
    }

    // ---- text objects ------------------------------------------------------------------------

    #[test]
    fn word_objects() {
        check("foo bar", 1, "diw", " bar", 0);
        check("foo bar", 5, "diw", "foo ", 3);
        check("foo bar", 1, "daw", "bar", 0);
        check("foo bar", 5, "daw", "foo", 2);
        check("foo bar baz", 5, "daw", "foo baz", 4);
        check("foo bar", 5, &format!("ciwX{ESC}"), "foo X", 4);
        check("foo bar", 1, &format!("caw{ESC}"), "bar", 0);
        check("a   b", 2, "diw", "ab", 1);
        check("a   b", 2, "daw", "a", 0);
        check("a-b c", 0, "diw", "-b c", 0);
        check("a-b c", 0, "diW", " c", 0);
        check("a-b c", 0, "daW", "c", 0);
        check("a b c d", 0, "d2aw", "c d", 0);
        check("a b c d", 0, "d3iw", " c d", 0);
        check("xin chào", 6, "diw", "xin ", 3);
        check("xin chào", 0, "yiwP", "xinxin chào", 2);
        // An empty line has no word.
        check("a\n\nb", 2, "diw", "a\n\nb", 2);
        check("foo bar", 1, "yiwP", "foofoo bar", 2);
        assert_eq!(run("foo bar", 5, "yaw").register.text, " bar");
        assert_eq!(run("foo bar", 5, "yiw").highlight, Some(4..7));
    }

    #[test]
    fn quote_objects() {
        check(r#"say "hi there" ok"#, 6, "di\"", r#"say "" ok"#, 5);
        check(r#"say "hi there" ok"#, 6, "da\"", "say ok", 4);
        check(r#"say "hi there" ok"#, 4, "di\"", r#"say "" ok"#, 5);
        check(r#"say "hi there" ok"#, 13, "di\"", r#"say "" ok"#, 5);
        // Before the first quote: the first pair on the line.
        check(r#"say "hi" ok"#, 0, "di\"", r#"say "" ok"#, 5);
        check(r#"say "hi" ok"#, 0, &format!("ci\"X{ESC}"), r#"say "X" ok"#, 5);
        // Between two pairs: the next one.
        check(r#""a" b "c""#, 4, "di\"", r#""a" b """#, 7);
        // An escaped quote is not a quote.
        check(r#"a "b\"c" d"#, 3, "di\"", r#"a "" d"#, 3);
        // No quote, one quote, another line.
        check("abc", 1, "di\"", "abc", 1);
        check("a \"b", 3, "di\"", "a \"b", 3);
        check("\"a\"\nb", 4, "di\"", "\"a\"\nb", 4);
        check("a 'b c' d", 4, "di'", "a '' d", 3);
        check("a `b c` d", 4, "di`", "a `` d", 3);
        check("a `b c` d", 4, "da`", "a d", 2);
        // Trailing blanks go with `a`; leading ones when nothing follows.
        check("x \"a\"", 3, "da\"", "x", 0);
        check("é \"ế\" ok", 4, "di\"", "é \"\" ok", 4);
    }

    #[test]
    fn bracket_objects() {
        check("f(a, (b), c)", 2, "di(", "f()", 2);
        check("f(a, (b), c)", 6, "di(", "f(a, (), c)", 6);
        check("f(a, (b), c)", 6, "d2i(", "f()", 2);
        check("f(a, (b), c)", 6, "da(", "f(a, , c)", 5);
        check("f(a, (b), c)", 2, "dib", "f()", 2);
        check("f(a, (b), c)", 2, "dab", "f", 0);
        check("f(a, (b), c)", 2, "di)", "f()", 2);
        // On a bracket itself.
        check("f(a)", 1, "di(", "f()", 2);
        check("f(a)", 3, "di(", "f()", 2);
        check("x[1, 2]", 3, "di[", "x[]", 2);
        check("x[1, 2]", 3, "da]", "x", 0);
        check("x{1, 2}", 3, "di{", "x{}", 2);
        check("x{1, 2}", 3, "diB", "x{}", 2);
        check("x{1, 2}", 3, "daB", "x", 0);
        check("f(a, (b), c)", 6, &format!("ci(X{ESC}"), "f(a, (X), c)", 6);
        check("f(a, (b), c)", 2, "yi(P", "f(a, (b), ca, (b), c)", 10);
        // Unmatched or absent brackets: nothing happens.
        check("a(b", 2, "di(", "a(b", 2);
        check("a)b", 0, "di(", "a)b", 0);
        check("abc", 1, "di(", "abc", 1);
        check("f()", 1, "di(", "f()", 1);
        check("f()", 1, "da(", "f", 0);
        // Brackets of another kind do not count.
        check("(a[b)c]", 2, "di(", "()c]", 1);
        // A block with its brackets on their own lines: the lines between.
        check("fn {\n  a;\n  b;\n}", 7, "di{", "fn {\n}", 5);
        check("fn {\n  a;\n  b;\n}", 7, "da{", "fn ", 2);
        check("fn {\n  a;\n}", 7, &format!("ci{{X{ESC}"), "fn {\nX\n}", 5);
        check("fn {\n}", 1, "di{", "fn {\n}", 1);
        check("fn {\n  a;\n  b;\n}", 7, "yi{", "fn {\n  a;\n  b;\n}", 7);
        assert_eq!(run("fn {\n  a;\n  b;\n}", 7, "yi{").register, Register { text: "  a;\n  b;\n".into(), linewise: true });
    }

    #[test]
    fn paragraph_objects() {
        check("a\nb\n\nc\nd", 0, "dip", "\nc\nd", 0);
        check("a\nb\n\nc\nd", 0, "dap", "c\nd", 0);
        check("a\nb\n\nc\nd", 5, "dip", "a\nb\n", 4);
        check("a\nb\n\nc\nd", 5, "dap", "a\nb", 2);
        check("a\nb\n\nc\nd", 4, "dip", "a\nb\nc\nd", 4);
        check("a\nb\n\nc\nd", 4, "dap", "a\nb", 2);
        check("a\n\n\nb", 0, "dap", "b", 0);
        check("a\nb\n\nc\nd", 0, "d2ap", "", 0);
        check("a\nb\n\nc\nd", 0, &format!("cipX{ESC}"), "X\n\nc\nd", 0);
        assert_eq!(run("a\nb\n\nc", 0, "yip").register, Register { text: "a\nb\n".into(), linewise: true });
        check("one", 1, "dip", "", 0);
    }

    #[test]
    fn text_objects_in_visual_mode() {
        let editor = run("foo bar", 1, "viw");
        assert_eq!(editor.selection, 0..3);
        assert_eq!(editor.mode(), Mode::Visual);
        let editor = run("foo bar", 1, "vaw");
        assert_eq!(editor.selection, 0..4);
        let editor = run("f(a, b) x", 3, "vi(");
        assert_eq!(editor.selection, 2..6);
        let editor = run("f(a, b) x", 3, "va(");
        assert_eq!(editor.selection, 1..7);
        let editor = run("say \"hi\" x", 6, "vi\"");
        assert_eq!(editor.selection, 5..7);
        let editor = run("a\nb\n\nc", 0, "vip");
        assert_eq!(editor.mode(), Mode::VisualLine);
        assert_eq!(editor.selection, 0..4);
        check("foo bar", 1, "viwd", " bar", 0);
        check("a\nb\n\nc", 0, "vipd", "\nc", 0);
        check("f(a, b) x", 3, &format!("vi(cX{ESC}"), "f(X) x", 2);
        // No object: the selection stays.
        let editor = run("abc", 1, "vi(");
        assert_eq!(editor.selection, 1..2);
    }

    // ---- `.` ---------------------------------------------------------------------------------

    #[test]
    fn dot_repeats_the_last_change() {
        check("abcd", 0, "x.", "cd", 0);
        check("a b c d", 0, "dw.", "c d", 0);
        check("a b c d", 0, "dw..", "d", 0);
        check("1\n2\n3\n4\n5\n6", 0, "2dd.", "5\n6", 0);
        check("a b c", 0, &format!("cwX{ESC}w."), "X X c", 2);
        check("a\nb", 0, &format!("A;{ESC}j."), "a;\nb;", 4);
        check("a", 0, &format!("ofoo{ESC}."), "a\nfoo\nfoo", 8);
        check("a", 0, ">>.", "    a", 4);
        check("a\nb", 0, "yyp.", "a\na\na\nb", 4);
        check("abc", 0, "rxl.", "xxc", 1);
        check("ab", 0, "~.", "AB", 1);
        check("a b c", 0, &format!("ciwX{ESC}w."), "X X c", 2);
        check("a(b) c(d)", 2, &format!("ci(X{ESC}f(."), "a(X) c(X)", 7);
        check("a b", 0, &format!("iX{ESC}w."), "Xa Xb", 3);
        check("a.b.c", 0, "dt..", ".c", 0);
    }

    #[test]
    fn dot_takes_a_new_count() {
        check("1\n2\n3\n4\n5\n6", 0, "2dd3.", "6", 0);
        check("abcdefg", 0, "x3.", "efg", 0);
        check("abcdefg", 0, "2x.", "efg", 0);
        check("abcdefghi", 0, "2x3..", "i", 0);
        check("a b c d e", 0, "d2w.", "e", 0);
        check("x", 0, &format!("3ifoo{ESC}."), "foofoofofoofoofooox", 16);
        check("x", 0, &format!("3ifoo{ESC}2."), "foofoofofoofooox", 13);
    }

    #[test]
    fn a_caret_move_in_insert_ends_the_insert_for_the_count_and_for_dot() {
        // The count is not applied to text from before the move, and nothing typed after it.
        check("abc def", 0, &format!("3iX{END}{ESC}"), "Xabc def", 7);
        // Only what is typed after the move is repeated (as `i`, whatever began the insert).
        check("abc def", 0, &format!("3iX{END}Y{ESC}"), "Xabc defY", 8);
        check("abc def", 0, &format!("3iX{END}Y{ESC}."), "Xabc defYY", 8);
        check("abc def", 0, &format!("iX{END}Y{ESC}j."), "Xabc defYY", 8);
        // `o` then a move: `.` opens no new line.
        check("a\nb", 0, &format!("oX{END}Y{ESC}."), "a\nXYY\nb", 3);
    }

    #[test]
    fn dot_ignores_what_is_not_a_change() {
        check("abc", 0, ".", "abc", 0);
        // A yank, a motion and an undo are not changes.
        check("abc", 0, "xyl.", "c", 0);
        check("abc", 0, "xl.", "b", 0);
        check("abc", 0, "xu.", "bc", 0);
        // Nor is a command that did nothing.
        check("a\n\nb", 0, "xj.", "\n\nb", 1);
        check("abc", 0, "xfz.", "c", 0);
    }

    #[test]
    fn dot_after_a_change_that_typed_nothing() {
        check("a b c", 0, &format!("cw{ESC}w."), "  c", 0);
    }

    // ---- r ~ J > < % -------------------------------------------------------------------------

    #[test]
    fn replace_one_or_more_chars() {
        check("abc", 1, "rx", "axc", 1);
        check("abcd", 0, "3rx", "xxxd", 2);
        check("abc", 0, "5rx", "abc", 0);
        check("abc", 0, "rế", "ếbc", 0);
        check("ế", 0, "ra", "a", 0);
        check("a\nb", 0, "2rx", "a\nb", 0);
        check("", 0, "rx", "", 0);
        check("abc", 0, &format!("r{ESC}x"), "bc", 0);
        assert_eq!(run("abc", 0, "rx").mode(), Mode::Normal);
    }

    #[test]
    fn tilde_toggles_case_and_moves_on() {
        check("abcd", 0, "~", "Abcd", 1);
        check("abcd", 0, "3~", "ABCd", 3);
        check("abcd", 0, "9~", "ABCD", 3);
        check("aBc", 0, "~~~", "AbC", 2);
        check("a1", 0, "~", "A1", 1);
        check("1a", 0, "~", "1a", 1);
        check("đẹp", 0, "~", "Đẹp", 2);
        check("Đẹp", 0, "~", "đẹp", 2);
        check("a\nb", 0, "5~", "A\nb", 0);
        check("", 0, "~", "", 0);
    }

    #[test]
    fn j_joins_lines() {
        check("a\n  b\nc", 0, "J", "a b\nc", 1);
        check("a\nb\nc\nd", 0, "3J", "a b c\nd", 3);
        check("a\nb\nc\nd", 0, "J.", "a b c\nd", 3);
        check("a \nb", 0, "J", "a b", 2);
        check("f(\n)", 0, "J", "f()", 2);
        check("a\n\nb", 0, "J", "a\nb", 0);
        check("\nb", 0, "J", "b", 0);
        check("a\n   \nb", 0, "J", "a\nb", 0);
        check("a", 0, "J", "a", 0);
        check("a\nb", 2, "J", "a\nb", 2);
        check("a\nb", 0, "9J", "a b", 1);
        check("đ\nế", 0, "J", "đ ế", 2);
        check("a\nb\nc", 0, "VjJ", "a b\nc", 1);
        check("a\nb\nc", 0, "VjjJ", "a b c", 3);
        check("a\nb\nc", 0, "vJ", "a b\nc", 1);
    }

    #[test]
    fn shifting_lines() {
        check("a\nb", 0, ">>", "  a\nb", 2);
        check("a\nb", 0, "2>>", "  a\n  b", 2);
        check("a\nb", 0, ">j", "  a\n  b", 2);
        check("a\nb\nc", 2, ">k", "  a\n  b\nc", 2);
        check("a\nb\nc", 0, ">G", "  a\n  b\n  c", 2);
        check("    a", 4, "<<", "  a", 2);
        check("  a", 2, "<<", "a", 0);
        check("a", 0, "<<", "a", 0);
        check(" a", 1, "<<", "a", 0);
        check("\ta", 1, "<<", "a", 0);
        check("a\n\nb", 0, "3>>", "  a\n\n  b", 2);
        check("  a", 2, ">>", "    a", 4);
        check("a b", 0, ">w", "  a b", 2);
        check("a\n\nb\nc", 0, ">ip", "  a\n\nb\nc", 2);
        check("a\nb", 0, "j>>", "a\n  b", 4);
        check("a\nb", 0, ">>.", "    a\nb", 4);
        check("  a", 2, "<<.", "a", 0);
        // The editor's indent width.
        let mut editor = Editor::new("a", 0);
        editor.env.indent = 4;
        editor.keys(">>");
        assert_eq!(editor.text, "    a");
        editor.keys("<<");
        assert_eq!(editor.text, "a");
        // Visual mode: the selected lines, `count` times.
        check("a\nb\nc", 0, "Vj>", "  a\n  b\nc", 2);
        check("a\nb\nc", 0, "V3>", "      a\nb\nc", 6);
        check("a\nb\nc", 0, "v>", "  a\nb\nc", 2);
        check("  a\n  b", 0, "Vj<", "a\nb", 0);
        check("a\nb", 0, "Vj>", "  a\n  b", 2);
    }

    #[test]
    fn percent_jumps_to_the_matching_bracket() {
        check("(a[b]c)", 0, "%", "(a[b]c)", 6);
        check("(a[b]c)", 6, "%", "(a[b]c)", 0);
        check("(a[b]c)", 2, "%", "(a[b]c)", 4);
        check("(a[b]c)", 4, "%", "(a[b]c)", 2);
        check("ab(c)", 0, "%", "ab(c)", 4);
        check("(a)b", 0, "d%", "b", 0);
        check("(a)b", 2, "d%", "b", 0);
        check("f(a, b)", 1, "y%", "f(a, b)", 1);
        check("{\n  a\n}", 0, "%", "{\n  a\n}", 6);
        check("{\n  a\n}", 6, "%", "{\n  a\n}", 0);
        check("x{(})", 1, "%", "x{(})", 3);
        // Nothing to match.
        check("(a", 0, "%", "(a", 0);
        check("abc", 0, "%", "abc", 0);
        check("a)", 0, "%", "a)", 0);
        let editor = run("(a)", 0, "v%");
        assert_eq!(editor.selection, 0..3);
    }

    #[test]
    fn big_word_motions() {
        check("a.b c.d", 0, "W", "a.b c.d", 4);
        check("a.b c.d", 4, "B", "a.b c.d", 0);
        check("a.b c.d", 0, "E", "a.b c.d", 2);
        check("a.b c.d", 0, "dW", "c.d", 0);
        check("a.b c.d", 0, "w", "a.b c.d", 1);
        check("a.b c.d", 6, "dB", "a.b d", 4);
    }

    #[test]
    fn s_big_s_x_and_y() {
        check("abc", 1, "X", "bc", 0);
        check("abc", 0, "X", "abc", 0);
        check("abc", 2, "2X", "c", 0);
        check("abc", 1, &format!("sX{ESC}"), "aXc", 1);
        check("abc", 0, &format!("2sX{ESC}"), "Xc", 0);
        check("  abc", 3, &format!("SX{ESC}"), "X", 0);
        check("a\nb\nc", 0, &format!("2SX{ESC}"), "X\nc", 0);
        check("a\nb", 0, "Yp", "a\na\nb", 2);
        check("a\nb\nc", 0, "2Yjp", "a\nb\na\nb\nc", 4);
        check("ab\ncd", 0, "2D", "", 0);
        check("ab\ncd\nef", 1, "2D", "a\nef", 0);
        check("abc", 1, &format!("CX{ESC}"), "aX", 1);
        // `s` on an empty line only starts the insert.
        let editor = run("a\n\nb", 2, "s");
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(editor.text, "a\n\nb");
        // `s` fills the register like `x`.
        assert_eq!(run("abc", 1, &format!("sX{ESC}")).register, Register { text: "b".into(), linewise: false });
    }

    #[test]
    fn x_on_nothing_leaves_the_register_alone() {
        let mut editor = run("ab\n\ncd", 0, "yl");
        editor.keys("jx");
        assert_eq!(editor.register.text, "a");
    }

    // ---- scrolling ---------------------------------------------------------------------------

    #[test]
    fn half_page_keys_move_by_the_pages_lines() {
        let text = (0..12).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let mut editor = Editor::new(&text, 0);
        editor.env.half_page = 3;
        editor.keys(&CTRL_D.to_string());
        assert_eq!(editor.text[..editor.cursor()].matches('\n').count(), 3, "three lines down");
        assert_eq!(editor.scroll, Some(Scroll::Lines(3)));
        editor.keys(&CTRL_D.to_string());
        editor.keys(&CTRL_U.to_string());
        assert_eq!(editor.text[..editor.cursor()].matches('\n').count(), 3);
        assert_eq!(editor.scroll, Some(Scroll::Lines(-3)));
        // A count is the number of lines.
        let mut editor = Editor::new(&text, 0);
        editor.keys(&format!("2{CTRL_D}"));
        assert_eq!(editor.text[..editor.cursor()].matches('\n').count(), 2);
        // The last line stops it, and the view moves only as far as the cursor did.
        let mut editor = Editor::new("a\nb\nc", 0);
        editor.env.half_page = 9;
        editor.keys(&CTRL_D.to_string());
        assert_eq!(editor.cursor(), 4);
        assert_eq!(editor.scroll, Some(Scroll::Lines(2)));
        // Up at the first line: nothing.
        let mut editor = Editor::new("a\nb", 0);
        editor.keys(&CTRL_U.to_string());
        assert_eq!(editor.cursor(), 0);
        assert_eq!(editor.scroll, None);
        // The column is kept.
        check("abc\nabc\nabc", 2, &CTRL_D.to_string(), "abc\nabc\nabc", 10);
        // Visual mode moves the head.
        let mut editor = Editor::new("a\nb\nc\nd", 0);
        editor.env.half_page = 2;
        editor.keys(&format!("v{CTRL_D}"));
        assert_eq!(editor.selection, 0..5);
    }

    #[test]
    fn z_commands_ask_the_view_to_scroll() {
        for (keys, scroll) in [("zz", Scroll::Center), ("zt", Scroll::Top), ("zb", Scroll::Bottom)] {
            let editor = run("a\nb\nc", 2, keys);
            assert_eq!(editor.scroll, Some(scroll), "{keys}");
            assert_eq!(editor.cursor(), 2, "{keys}: the cursor stays");
            assert_eq!(editor.text, "a\nb\nc");
        }
        // Anything else after `z` is nothing.
        let editor = run("a\nb", 0, "zxx");
        assert_eq!(editor.text, "\nb", "`zx` is nothing, the next x deletes");
    }

    #[test]
    fn a_yank_reports_the_text_it_took_for_the_highlight() {
        assert_eq!(run("foo bar", 0, "yw").highlight, Some(0..4));
        assert_eq!(run("foo bar", 0, "ye").highlight, Some(0..3));
        assert_eq!(run("a\nb\nc", 2, "yy").highlight, Some(2..3));
        assert_eq!(run("a\nb\nc", 0, "2yy").highlight, Some(0..3));
        assert_eq!(run("foo bar", 0, "vey").highlight, Some(0..3));
        assert_eq!(run("a\nb\nc", 2, "Vy").highlight, Some(2..3));
        assert_eq!(run("foo bar", 0, "dw").highlight, None);
        assert_eq!(run("foo bar", 0, "x").highlight, None);
    }

    // ---- Insert mode and the machine's reset ------------------------------------------------

    #[test]
    fn a_machine_can_start_in_insert_mode() {
        let mut vim = Vim::with_mode(Mode::Insert);
        assert_eq!(vim.mode(), Mode::Insert);
        let rope = Rope::from("abc");
        let effect = vim.key(Key::Esc, &rope, 3..3, false, &Register::default(), &Env::default());
        assert_eq!(vim.mode(), Mode::Normal);
        assert_eq!(effect.select, Some((2..2, false)));
    }

    #[test]
    fn unknown_keys_cancel_a_pending_command_without_inserting() {
        check("abc def", 0, "3qx", "bc def", 0);
        check("abc def", 0, "d3zx", "bc def", 0);
        check("abc def", 0, "2fzx", "bc def", 0);
        check("abc def", 0, "d2iz", "abc def", 0);
        check("abc def", 0, &format!("2{ESC}x"), "bc def", 0);
        check("abc def", 0, "2dwx", "", 0);
    }

    #[test]
    fn multibyte_text_through_the_new_commands() {
        check("Xin chào các bạn", 0, "3w", "Xin chào các bạn", 15);
        check("đẹp lắm", 0, "dw", "lắm", 0);
        check("đẹp lắm", 0, "d2w", "", 0);
        check("(đẹp)", 3, "di(", "()", 1);
        check("đẹp\nlắm", 0, "J", "đẹp lắm", 6);
        check("đẹp", 0, "3x", "", 0);
        check("lắm đẹp", 0, "2dd", "", 0);
        check("lắm", 0, "yyp", "lắm\nlắm", 6);
        check("ấ ế", 0, "fế", "ấ ế", 4);
    }
}
