//! Vim keys in the query editors (SQL, MongoDB script, Redis console). The state machine is
//! `crate::vim`; this file feeds it the keys of an editor that has Vim on and applies what it asks
//! to the editor's `EditorState`.
//!
//! Vim is on for a tab when the switch in the editor's status line says so, else when Settings >
//! Editor > Vim keys does (the switch is session-only). The editor is wrapped in a div only while Vim
//! is on, so a tab without it renders and behaves exactly as before.
//!
//! Settings > Editor > Vim are read when a key is handled, so a change reaches the open editors at
//! once (only the start mode waits for the next editor or switch): the start mode, how the register
//! and the system clipboard are tied (`vim_feed`), the Insert-mode escape sequence
//! (`vim_escape_sequence_key`: its first key is held back, not typed, until the second comes or the
//! timeout runs out, so "jk" never shows a j), the yank highlight (`vim_caret::YankFlash`), whether
//! Vim takes Ctrl-R / Ctrl-D / Ctrl-U / Ctrl-[, and the search's case rules (`vim_env`). The search
//! prompt is the machine's; the status line shows it (`render_vim_controls`) and Enter / Backspace /
//! Escape reach it through the editor's own actions. The relative line numbers other Vims have are
//! not here: gpui-base draws the gutter with a `GutterStyle` that gpui-component keeps private and
//! gives no hook for the number's text, so they would need a gutter of our own.
//!
//! Key flow (gpui dispatches bound actions first, then key listeners, capture before bubble):
//! plain keys are not bound in the editor, so the wrapper's `capture_key_down` takes them and stops
//! them before the editor can insert them (Normal and Visual; in Insert they pass). The keys the
//! editor binds and Vim changes the meaning of (Escape, Enter, Backspace, Delete, Tab, Cut, Paste,
//! the delete-word / delete-line chords, and Ctrl-[ off macOS) are taken with `capture_action`.
//! Other Cmd / Alt chords are never touched, so the app's shortcuts (Run, Save, Format ...) work in
//! every mode; the editor's completion menu keeps Enter, Escape and the arrows in Insert mode. Of
//! the Ctrl chords only Ctrl-R / Ctrl-D / Ctrl-U / Ctrl-[ are Vim's (and only with "Take Ctrl keys"):
//! Ctrl-C and Ctrl-A (copy, select all) reach the editor in every mode; Ctrl-X and Ctrl-V are edits,
//! refused outside Insert mode like Cmd-X and Cmd-V.
//!
//! Text that does not come as a key press (an input method's composition, dead keys, Option
//! chords on macOS: the platform hands it to the editor's input handler without a key-down event)
//! cannot be stopped by those listeners. So outside Insert mode the editor is read-only
//! (`Editor::readonly`, set on every render and at once when the mode changes): the input handler
//! refuses the text, whatever its source. Vim's own edits (`EditorState::replace`) and the document
//! APIs are not limited by it. Undo and Redo are: a read-only editor does not even register them
//! (and the editor offers no direct call), so `u`, Ctrl-R and Cmd-Z wait until a frame has been drawn
//! editable and dispatch the action behind that frame (the platform runs the next-frame callbacks
//! before it draws, so the first callback only finds the previous, read-only, frame).
use super::*;
use crate::settings::VimClipboard;
use crate::vim::{Effect, Env, History, Key, Mode, Register, Scroll, Vim};
use gpui_component::input::{
    Backspace, Cut, Delete, DeleteToBeginningOfLine, DeleteToEndOfLine, DeleteToNextWordEnd,
    DeleteToPreviousWordStart, Enter as InputEnter, Escape as InputEscape, IndentInline, MoveDown,
    MoveLeft, MoveRight, MoveUp, Outdent, OutdentInline, Paste, Redo, SelectAll, Undo,
};
use std::time::Duration;

/// One tab's Vim state, made when the tab first has Vim on or a switch.
#[derive(Default)]
pub(super) struct VimTab {
    /// The header switch; `None` follows Settings.
    enabled: Option<bool>,
    /// Whether Vim was on when the tab last rendered: a change starts over in Normal mode.
    running: bool,
    vim: Vim,
    /// `u` / Ctrl-R waiting for the frame that draws the editor editable (see the module doc).
    history: Vec<History>,
    /// Whether the tab was rendered (editable) since `history` got its first entry.
    history_drawn: bool,
    /// The waiting history is being dispatched: the Undo / Redo it sends is the editor's.
    replaying: bool,
    /// The first key of the escape sequence, typed but held back (not in the editor yet), and the
    /// timeout that types it when the second key does not come.
    esc_held: Option<char>,
    esc_task: Option<gpui::Task<()>>,
    /// The text a yank took while it is highlighted, and the timer that ends that.
    yank: Option<std::ops::Range<usize>>,
    yank_task: Option<gpui::Task<()>>,
}

impl VimTab {
    fn started(mode: Mode) -> Self {
        Self { running: true, vim: Vim::with_mode(mode), ..Self::default() }
    }
}

/// Next-frame rounds a waiting Undo / Redo gets for its frame to be drawn (a tab that is not on
/// screen is never drawn: the keys are dropped).
const HISTORY_FRAMES: u8 = 4;

/// Vim of all editors: the register is shared by every tab.
#[derive(Default)]
pub(super) struct VimState {
    register: Register,
    tabs: HashMap<u64, VimTab>,
    /// The keystroke watches of `AppRoot::watch_vim_keys`.
    watch: Option<(gpui::Subscription, gpui::Subscription)>,
}

impl VimState {
    pub(super) fn tab_removed(&mut self, tab_id: u64) {
        self.tabs.remove(&tab_id);
    }

    #[cfg(test)]
    pub(super) fn register(&self) -> &Register {
        &self.register
    }
}

fn mode_label(mode: Mode) -> String {
    let label = match mode {
        Mode::Normal => t!("editor.vim_normal"),
        Mode::Insert => t!("editor.vim_insert"),
        Mode::Visual => t!("editor.vim_visual"),
        Mode::VisualLine => t!("editor.vim_visual_line"),
    };
    label.to_string()
}

impl AppRoot {
    /// Whether the tab's editor uses Vim keys.
    pub(super) fn vim_on(&self, tab_id: u64) -> bool {
        self.query_editors
            .vim
            .tabs
            .get(&tab_id)
            .and_then(|tab| tab.enabled)
            .unwrap_or(self.settings.editor.vim_mode)
    }

    pub(super) fn vim_mode(&self, tab_id: u64) -> Mode {
        self.query_editors.vim.tabs.get(&tab_id).map_or_else(|| self.vim_start_mode(), |tab| tab.vim.mode())
    }

    /// The mode Settings > Editor > Vim > Start in gives an editor that opens or gets Vim turned on.
    fn vim_start_mode(&self) -> Mode {
        match self.settings.editor.vim_default_mode {
            crate::settings::VimDefaultMode::Normal => Mode::Normal,
            crate::settings::VimDefaultMode::Insert => Mode::Insert,
        }
    }

    /// What the machine needs of the editor and the settings.
    fn vim_env(&self, editor: &gpui::Entity<EditorState>, cx: &gpui::App) -> Env {
        let settings = &self.settings.editor;
        let rows = editor.read(cx).visible_row_range().map_or(20, |rows| rows.len());
        Env {
            half_page: (rows / 2).max(1),
            ignore_case: settings.vim_ignore_case,
            smart_case: settings.vim_smart_case,
            indent: settings.tab_size as usize,
        }
    }

    /// The two keys of the escape sequence, when the setting holds exactly two.
    fn vim_escape_pair(&self) -> Option<(char, char)> {
        let mut keys = self.settings.editor.vim_escape_sequence.chars();
        match (keys.next(), keys.next(), keys.next()) {
            (Some(first), Some(second), None) => Some((first, second)),
            _ => None,
        }
    }

    /// Whether the tab's editor refuses text: Vim is on and the mode is not Insert.
    pub(super) fn vim_readonly(&self, tab_id: u64) -> bool {
        self.vim_on(tab_id)
            && self.vim_mode(tab_id) != Mode::Insert
            && self.query_editors.vim.tabs.get(&tab_id).is_none_or(|tab| tab.history.is_empty())
    }

    /// Called when the tab's editor is rendered: an Undo / Redo that waits for an editable frame has it.
    pub(super) fn vim_rendered(&mut self, tab_id: u64) {
        if let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) {
            tab.history_drawn = !tab.history.is_empty();
        }
    }

    /// Puts the editor's read-only flag where the mode wants it now, without waiting for the next
    /// render (a key typed right after `i` must already be text).
    fn vim_apply_readonly(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let readonly = self.vim_readonly(tab_id);
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
            editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
        }
    }

    /// A Normal-mode caret sits on a char: a click after the last char of a line (or Vim just turned
    /// on there) puts it on that char.
    fn vim_clamp_caret(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.query_editors.vim.tabs.get(&tab_id) else {
            return;
        };
        let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() else {
            return;
        };
        let caret = {
            let state = editor.read(cx);
            tab.vim.normal_caret(state.text(), &state.selected_range())
        };
        if let Some(caret) = caret {
            editor.update(cx, |state, cx| state.set_selected_range(caret..caret, cx));
        }
    }

    /// Called when a tab renders: Vim turning on (here, by Settings) starts in Normal mode.
    pub(super) fn vim_sync(&mut self, tab_id: u64) -> bool {
        let on = self.vim_on(tab_id);
        let start = self.vim_start_mode();
        let tabs = &mut self.query_editors.vim.tabs;
        match tabs.get_mut(&tab_id) {
            Some(tab) if tab.running != on => {
                *tab = VimTab { enabled: tab.enabled, ..VimTab::started(start) };
                tab.running = on;
            }
            None if on => {
                tabs.insert(tab_id, VimTab::started(start));
            }
            _ => {}
        }
        on
    }

    /// The Toggle Vim Mode command: flips Vim for the active tab's editor, as its status-line switch does.
    pub(super) fn toggle_active_vim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id).filter(|id| self.query_editors.by_tab.contains_key(id)) {
            self.toggle_tab_vim(tab_id, window, cx);
        }
    }

    /// The status-line switch: flips Vim for this tab only, starting in Normal mode.
    pub(super) fn toggle_tab_vim(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let on = !self.vim_on(tab_id);
        let start = self.vim_start_mode();
        let tab = self.query_editors.vim.tabs.entry(tab_id).or_default();
        *tab = VimTab { enabled: Some(on), ..VimTab::started(start) };
        tab.running = on;
        self.vim_apply_readonly(tab_id, cx);
        self.vim_clamp_caret(tab_id, cx);
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id) {
            editor.update(cx, |state, cx| state.focus(window, cx));
        }
        self.notify_pane_views(cx);
    }

    /// The switch and, while Vim is on, the mode as a small badge in the mode's color, for the
    /// status line of a query editor.
    pub(super) fn render_vim_controls(&self, tab_id: u64, cx: &mut Context<Self>) -> AnyElement {
        let on = self.vim_on(tab_id);
        // The search being typed: `/foo` and a caret.
        let prompt = self.vim_prompt_text(tab_id).filter(|_| on);
        let mono = gpui_component::theme::Theme::global(cx).mono_font_family.clone();
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .child(
                // The official Switch and the name, one target: a click anywhere on it (the switch included, which
                // has no handler of its own) reaches the wrapper. Keyboard: the Toggle Vim Mode command.
                div()
                    .id(("vim-toggle", tab_id))
                    .debug_selector(|| "vim-toggle".into())
                    .h(px(18.0))
                    .px(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(if on { theme::accent() } else { theme::text_muted() })
                    .hover(|style| style.bg(theme::hover()).text_color(theme::text()))
                    .tooltip(|window, cx| gpui_component::tooltip::Tooltip::new(t!("editor.vim_toggle_tip").to_string()).build(window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| this.toggle_tab_vim(tab_id, window, cx)))
                    .child(Switch::new(("vim-switch", tab_id)).xsmall().checked(on).tab_stop(false))
                    .child(div().whitespace_nowrap().child(t!("editor.vim"))),
            )
            .when(on, |this| {
                let mode = self.vim_mode(tab_id);
                let color = editor_status::mode_color(mode);
                this.child(
                    div()
                        .debug_selector(|| "vim-mode".into())
                        .px(px(5.0))
                        .h(px(14.0))
                        .flex()
                        .items_center()
                        .rounded(px(3.0))
                        .bg(gpui::Hsla::from(color).opacity(0.18))
                        .text_size(px(10.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(color)
                        .whitespace_nowrap()
                        .child(mode_label(mode)),
                )
            })
            .when_some(prompt, |this, prompt| {
                this.child(
                    div()
                        .debug_selector(|| "vim-prompt".into())
                        .flex()
                        .items_center()
                        .whitespace_nowrap()
                        .font_family(mono)
                        .text_color(theme::text())
                        .child(prompt)
                        .child(div().w(px(1.0)).h(px(12.0)).ml(px(1.0)).bg(theme::text())),
                )
            })
            .into_any_element()
    }

    /// The editor of `tab_id` when Vim is on for it and the editor itself (not its search field)
    /// has the keyboard focus.
    fn vim_editor(&self, tab_id: u64, window: &Window, cx: &Context<Self>) -> Option<gpui::Entity<EditorState>> {
        if !self.vim_on(tab_id) {
            return None;
        }
        let editor = self.query_editors.by_tab.get(&tab_id)?;
        editor.focus_handle(cx).is_focused(window).then(|| editor.clone())
    }

    /// `content` (the editor) with the Vim key handling around it, when Vim is on for the tab.
    pub(super) fn vim_wrap(&self, tab_id: u64, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        if !self.vim_on(tab_id) {
            return content;
        }
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            // The block caret is positioned against this box (it is drawn over the editor).
            .relative()
            .capture_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                this.vim_key_down(tab_id, event, window, cx)
            }))
            // The click put the caret where it fell, maybe after the line's last char.
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(move |this, _, _, cx| this.vim_clamp_caret(tab_id, cx)))
            // A held escape-sequence key is typed before the caret moves.
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.vim_flush_held(tab_id, window, cx);
                    // The click moves the caret after this listener: look once it has.
                    cx.defer_in(window, move |this, window, cx| this.vim_insert_moved(tab_id, window, cx));
                }),
            )
            .capture_action(cx.listener(move |this, _: &InputEscape, window, cx| {
                this.vim_escape(tab_id, window, cx)
            }))
            // The editor's own meaning of these keys is an edit: only Insert mode has it. With the search
            // prompt open, Enter runs it and Backspace edits it.
            .capture_action(cx.listener(move |this, action: &InputEnter, window, cx| {
                if this.vim_prompt_key(tab_id, Key::Enter, window, cx) {
                    return;
                }
                if !action.secondary {
                    this.vim_swallow(tab_id, window, cx);
                } else if this.vim_editor(tab_id, window, cx).is_some() && this.vim_mode(tab_id) != Mode::Insert {
                    // Cmd+Enter is the editor's Run; a read-only editor does not handle it any more.
                    this.run_tab_query_explicit(tab_id, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(move |this, _: &Backspace, window, cx| {
                if !this.vim_prompt_key(tab_id, Key::Backspace, window, cx) {
                    this.vim_swallow(tab_id, window, cx);
                }
            }))
            // These move the caret (or the completion menu's choice): a held key is typed first.
            .capture_action(cx.listener(move |this, _: &MoveLeft, window, cx| this.vim_flush_held(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &MoveRight, window, cx| this.vim_flush_held(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &MoveUp, window, cx| this.vim_flush_held(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &MoveDown, window, cx| this.vim_flush_held(tab_id, window, cx)))
            // Edit > Select All reaches the editor without a key press of the wrapper's.
            .capture_action(cx.listener(move |this, _: &SelectAll, window, cx| this.vim_flush_held(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &Delete, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &IndentInline, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &OutdentInline, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &Cut, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &Paste, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &DeleteToBeginningOfLine, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &DeleteToEndOfLine, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &DeleteToPreviousWordStart, window, cx| this.vim_swallow(tab_id, window, cx)))
            .capture_action(cx.listener(move |this, _: &DeleteToNextWordEnd, window, cx| this.vim_swallow(tab_id, window, cx)))
            // Cmd-Z / Cmd-Shift-Z: a read-only editor does not register them; here they are `u` and Ctrl-R.
            .capture_action(cx.listener(move |this, _: &Undo, window, cx| {
                this.vim_flush_held(tab_id, window, cx);
                this.vim_history_action(tab_id, History::Undo, window, cx)
            }))
            .capture_action(cx.listener(move |this, _: &Redo, window, cx| {
                this.vim_flush_held(tab_id, window, cx);
                this.vim_history_action(tab_id, History::Redo, window, cx)
            }))
            // Ctrl-[ is Escape; the editor binds it to Outdent off macOS (on macOS that is Cmd-[).
            .when(cfg!(not(target_os = "macos")), |this| {
                this.capture_action(cx.listener(move |this, _: &Outdent, window, cx| {
                    if this.settings.editor.vim_ctrl_keys {
                        this.vim_flush_held(tab_id, window, cx);
                        this.vim_feed_key(tab_id, Key::Esc, window, cx)
                    }
                }))
            })
            .child(content)
            // Normal and Visual mode: a box over the char, not the editor's bar (see `vim_caret`).
            .children(self.query_editors.by_tab.get(&tab_id).filter(|_| matches!(self.vim_mode(tab_id), Mode::Normal | Mode::Visual)).map(
                |editor| vim_caret::BlockCaret::new(editor.clone(), self.vim_mode(tab_id), cx),
            ))
            // A yank, for a moment (Settings > Highlight on yank).
            .children(
                self.query_editors
                    .vim
                    .tabs
                    .get(&tab_id)
                    .and_then(|tab| tab.yank.clone())
                    .zip(self.query_editors.by_tab.get(&tab_id))
                    .map(|(range, editor)| vim_caret::YankFlash::new(editor.clone(), range)),
            )
            .into_any_element()
    }

    /// Stops an editing key the editor would handle, unless the tab is in Insert mode.
    fn vim_swallow(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.vim_flush_held(tab_id, window, cx);
        if self.vim_editor(tab_id, window, cx).is_some() && self.vim_mode(tab_id) != Mode::Insert {
            cx.stop_propagation();
        }
    }

    fn vim_escape(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.vim_flush_held(tab_id, window, cx);
        let Some(editor) = self.vim_editor(tab_id, window, cx) else {
            return;
        };
        let mode = self.vim_mode(tab_id);
        let prompt = self.vim_prompt_open(tab_id);
        // The completion menu closes first; Escape in Normal mode only cancels a pending command
        // (and passes on to the app).
        if mode == Mode::Insert && editor.read(cx).completion_menu_state().open {
            return;
        }
        self.vim_feed(tab_id, &editor, Key::Esc, window, cx);
        if mode != Mode::Normal || prompt {
            cx.stop_propagation();
        }
    }

    /// The search being typed in the tab's editor, with its `/` or `?`.
    pub(super) fn vim_prompt_text(&self, tab_id: u64) -> Option<String> {
        let tab = self.query_editors.vim.tabs.get(&tab_id)?;
        tab.vim.prompt().map(|(lead, text)| format!("{lead}{text}"))
    }

    fn vim_prompt_open(&self, tab_id: u64) -> bool {
        self.query_editors.vim.tabs.get(&tab_id).is_some_and(|tab| tab.vim.prompt().is_some())
    }

    /// Enter / Backspace while the search prompt is open belong to the prompt: whether it took the key.
    fn vim_prompt_key(&mut self, tab_id: u64, key: Key, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.vim_prompt_open(tab_id) {
            return false;
        }
        let Some(editor) = self.vim_editor(tab_id, window, cx) else {
            return false;
        };
        self.vim_feed(tab_id, &editor, key, window, cx);
        cx.stop_propagation();
        true
    }

    /// Two watches on every keystroke of the app, installed once. Keys the editor binds to an action
    /// (arrows, Shift+arrows, Home / End, word moves, Cmd-A ...) never reach the wrapper's key listener,
    /// so these are what see them:
    /// - before the key is resolved, any key that is not a plain character ends the hold of the escape
    ///   sequence's first key: it is typed where the caret and the selection were when it was pressed,
    ///   not later by the timer over what that key selected;
    /// - after it was resolved, a key that moved the caret in Insert mode starts a new insert there
    ///   (`Vim::insert_moved`), so a count or `.` never repeats text typed before the move.
    pub(super) fn watch_vim_keys(&mut self, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let before = cx.intercept_keystrokes(move |event, window, cx| {
            let stroke = &event.keystroke;
            let lone_modifier = stroke.modifiers == gpui::Modifiers::default()
                && matches!(stroke.key.as_str(), "shift" | "control" | "alt" | "platform" | "function");
            if lone_modifier || Self::vim_plain_char(stroke) {
                return;
            }
            let _ = weak.update(cx, |this, cx| this.vim_flush_all_held(window, cx));
        });
        let after = cx.observe_keystrokes(|this, event, window, cx| {
            if matches!(event.keystroke.key.as_str(), "left" | "right" | "up" | "down" | "home" | "end" | "pageup" | "pagedown") {
                this.vim_insert_moved_all(window, cx);
            }
        });
        self.query_editors.vim.watch = Some((before, after));
    }

    /// A key that types a character (Shift allowed): the second key of an escape sequence may be one.
    fn vim_plain_char(stroke: &gpui::Keystroke) -> bool {
        let modifiers = stroke.modifiers;
        !(modifiers.platform || modifiers.alt || modifiers.function || modifiers.control)
            && stroke.key_char.as_deref().is_some_and(|text| {
                let mut chars = text.chars();
                matches!((chars.next(), chars.next()), (Some(ch), None) if !ch.is_control())
            })
    }

    fn vim_flush_all_held(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let held: Vec<u64> = self
            .query_editors
            .vim
            .tabs
            .iter()
            .filter(|(_, tab)| tab.esc_held.is_some())
            .map(|(id, _)| *id)
            .collect();
        for tab_id in held {
            self.vim_flush_held(tab_id, window, cx);
        }
    }

    /// After a key that moves the caret: every editor in Insert mode that has the focus.
    fn vim_insert_moved_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inserting: Vec<u64> = self
            .query_editors
            .vim
            .tabs
            .iter()
            .filter(|(_, tab)| tab.vim.mode() == Mode::Insert)
            .map(|(id, _)| *id)
            .collect();
        for tab_id in inserting {
            self.vim_insert_moved(tab_id, window, cx);
        }
    }

    /// The caret of the focused editor of `tab_id` was moved by the editor in Insert mode (a key, a
    /// click): the machine starts a new insert where it is now.
    fn vim_insert_moved(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.vim_editor(tab_id, window, cx) else {
            return;
        };
        // The arrows choose in the completion menu while it is open; the caret stays.
        if editor.read(cx).completion_menu_state().open {
            return;
        }
        let state = editor.read(cx);
        let selection = state.selected_range();
        let caret = if state.is_selection_reversed() { selection.start } else { selection.end };
        if let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) {
            tab.vim.insert_moved(caret);
        }
    }

    /// Types the held first key of the escape sequence, when the second did not come.
    fn vim_flush_held(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let held = self.query_editors.vim.tabs.get_mut(&tab_id).and_then(|tab| {
            tab.esc_task = None;
            tab.esc_held.take()
        });
        if let Some(ch) = held {
            self.vim_type(tab_id, ch, window, cx);
        }
    }

    /// Types `ch` into the editor, as the key it was.
    fn vim_type(&mut self, tab_id: u64, ch: char, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_mode(tab_id) != Mode::Insert {
            return;
        }
        if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
            editor.update(cx, |state, cx| state.replace(ch.to_string(), window, cx));
        }
    }

    /// A plain key typed in Insert mode with an escape sequence set: the first key is held back, the
    /// second (within the timeout) leaves Insert mode and neither is ever in the text; any other key
    /// types the held one first. Returns whether the key was taken.
    fn vim_escape_sequence_key(&mut self, tab_id: u64, ch: char, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((first, second)) = self.vim_escape_pair() else {
            self.vim_flush_held(tab_id, window, cx);
            return false;
        };
        let held = self.query_editors.vim.tabs.get_mut(&tab_id).and_then(|tab| {
            tab.esc_task = None;
            tab.esc_held.take()
        });
        if held.is_some() && ch == second {
            if let Some(editor) = self.query_editors.by_tab.get(&tab_id).cloned() {
                self.vim_feed(tab_id, &editor, Key::Esc, window, cx);
            }
            return true;
        }
        if let Some(held) = held {
            self.vim_type(tab_id, held, window, cx);
        }
        if ch != first {
            return false;
        }
        let timeout = Duration::from_millis(u64::from(self.settings.editor.vim_escape_timeout.clamp(100, 2000)));
        let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) else {
            return false;
        };
        tab.esc_held = Some(ch);
        tab.esc_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(timeout).await;
            let _ = this.update_in(cx, |this, window, cx| this.vim_flush_held(tab_id, window, cx));
        }));
        true
    }

    fn vim_feed_key(&mut self, tab_id: u64, key: Key, window: &mut Window, cx: &mut Context<Self>) {
        self.vim_flush_held(tab_id, window, cx);
        let Some(editor) = self.vim_editor(tab_id, window, cx) else {
            return;
        };
        if self.vim_mode(tab_id) == Mode::Insert && editor.read(cx).completion_menu_state().open {
            return;
        }
        self.vim_feed(tab_id, &editor, key, window, cx);
        cx.stop_propagation();
    }

    fn vim_key_down(
        &mut self,
        tab_id: u64,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stroke = &event.keystroke;
        let modifiers = stroke.modifiers;
        // Cmd and Alt chords are the app's and the editor's; Escape is taken as an action.
        if modifiers.platform || modifiers.alt || modifiers.function || stroke.key == "escape" {
            return;
        }
        let key = if modifiers.control {
            // Settings > Vim > Take Ctrl keys: off leaves every Ctrl chord to the editor and the app.
            if !self.settings.editor.vim_ctrl_keys {
                return;
            }
            match (stroke.key.as_str(), modifiers.shift) {
                ("[", _) => Key::Esc,
                ("r", false) => Key::CtrlR,
                ("d", false) => Key::CtrlD,
                ("u", false) => Key::CtrlU,
                _ => return,
            }
        } else {
            let mut chars = stroke.key_char.as_deref().unwrap_or_default().chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_control() => Key::Char(ch),
                _ => return,
            }
        };
        // In Insert mode the keys are typing (but for Escape and the escape sequence).
        if key != Key::Esc && self.vim_mode(tab_id) == Mode::Insert {
            match key {
                // Only the editor itself (not its search field) has the escape sequence.
                Key::Char(ch) if !modifiers.control => {
                    if self.vim_editor(tab_id, window, cx).is_some() && self.vim_escape_sequence_key(tab_id, ch, window, cx) {
                        cx.stop_propagation();
                    }
                }
                _ => self.vim_flush_held(tab_id, window, cx),
            }
            return;
        }
        self.vim_feed_key(tab_id, key, window, cx);
    }

    /// The editor's own Undo / Redo (Cmd-Z, Cmd-Shift-Z) outside Insert mode.
    fn vim_history_action(&mut self, tab_id: u64, history: History, window: &mut Window, cx: &mut Context<Self>) {
        let replaying = self.query_editors.vim.tabs.get(&tab_id).is_some_and(|tab| tab.replaying);
        if !replaying && self.vim_editor(tab_id, window, cx).is_some() && self.vim_mode(tab_id) != Mode::Insert {
            self.vim_queue_history(tab_id, history, window, cx);
            cx.stop_propagation();
        }
    }

    /// Queues an Undo / Redo: the editor is editable at once and the action goes after the first frame
    /// that is drawn like that.
    fn vim_queue_history(&mut self, tab_id: u64, history: History, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) else {
            return;
        };
        let first = tab.history.is_empty();
        tab.history.push(history);
        if first {
            tab.history_drawn = false;
            window.on_next_frame(Self::vim_history_callback(cx.weak_entity(), tab_id, HISTORY_FRAMES));
        }
        self.vim_apply_readonly(tab_id, cx);
        self.notify_pane_views(cx);
    }

    fn vim_history_callback(
        root: gpui::WeakEntity<Self>,
        tab_id: u64,
        frames: u8,
    ) -> impl FnOnce(&mut Window, &mut gpui::App) + 'static {
        move |window, cx| {
            // The Undo / Redo is dispatched outside the update: its capture listener updates the app.
            let Ok(Some((focus, waiting))) = root.update(cx, |this, cx| this.vim_take_history(&root, tab_id, frames, window, cx))
            else {
                return;
            };
            for history in waiting {
                match history {
                    History::Undo => focus.dispatch_action(&Undo, window, cx),
                    History::Redo => focus.dispatch_action(&Redo, window, cx),
                }
            }
            let _ = root.update(cx, |this, cx| {
                if let Some(tab) = this.query_editors.vim.tabs.get_mut(&tab_id) {
                    tab.replaying = false;
                }
                this.vim_apply_readonly(tab_id, cx);
                this.notify_pane_views(cx);
            });
        }
    }

    /// In a next-frame callback: once a frame has been drawn editable, the waiting Undo / Redo to
    /// dispatch (then read-only again); until then (the callbacks run before the frame is drawn) another
    /// round; after the last one, read-only again and the keys dropped.
    fn vim_take_history(
        &mut self,
        root: &gpui::WeakEntity<Self>,
        tab_id: u64,
        frames: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(gpui::FocusHandle, Vec<History>)> {
        let tab = self.query_editors.vim.tabs.get_mut(&tab_id)?;
        if !tab.history_drawn && frames > 1 {
            window.on_next_frame(Self::vim_history_callback(root.clone(), tab_id, frames - 1));
            return None;
        }
        let drawn = std::mem::take(&mut tab.history_drawn);
        let waiting = std::mem::take(&mut tab.history);
        let editor = self.query_editors.by_tab.get(&tab_id).cloned().filter(|_| drawn);
        if let Some(editor) = editor {
            if let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) {
                tab.replaying = true;
            }
            return Some((editor.focus_handle(cx), waiting));
        }
        self.vim_apply_readonly(tab_id, cx);
        self.notify_pane_views(cx);
        None
    }

    /// Runs `key` through the tab's machine and applies the effect to the editor.
    fn vim_feed(
        &mut self,
        tab_id: u64,
        editor: &gpui::Entity<EditorState>,
        key: Key,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let env = self.vim_env(editor, cx);
        let clipboard = self.settings.editor.vim_clipboard;
        let highlight_for = Duration::from_millis(u64::from(self.settings.editor.vim_highlight_yank));
        // "Always": `p` puts what the system clipboard holds (the register keeps the lines/chars
        // distinction only for what Vim itself put there).
        let from_clipboard = (clipboard == VimClipboard::Always && matches!(key, Key::Char('p' | 'P')))
            .then(|| cx.read_from_clipboard().and_then(|item| item.text()))
            .flatten()
            .filter(|text| !text.is_empty() && *text != self.query_editors.vim.register.text)
            .map(|text| Register { text, linewise: false });
        let VimState { register, tabs, .. } = &mut self.query_editors.vim;
        let Some(tab) = tabs.get_mut(&tab_id) else {
            return;
        };
        let (before, prompt_before) = (tab.vim.mode(), tab.vim.prompt().is_some());
        let effect = {
            let state = editor.read(cx);
            let put = from_clipboard.as_ref().unwrap_or(register);
            tab.vim.key(key, state.text(), state.selected_range(), state.is_selection_reversed(), put, &env)
        };
        let (after, prompt_after) = (tab.vim.mode(), tab.vim.prompt().is_some());
        let Effect { edit, select, register: new_register, copy, history, scroll, highlight } = effect;
        if let Some(new_register) = new_register {
            // Always: every register write; on yank: a yank; never: Vim's own register only.
            let to_clipboard = match clipboard {
                VimClipboard::Always => true,
                VimClipboard::OnYank => copy,
                VimClipboard::Never => false,
            };
            if to_clipboard {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(new_register.text.clone()));
            }
            *register = new_register;
        }
        // A key ends the last yank's highlight; a new yank starts one.
        let had_yank = tab.yank.take().is_some();
        tab.yank_task = None;
        let yanked = highlight.filter(|_| !highlight_for.is_zero());
        if let Some(range) = &yanked {
            tab.yank = Some(range.clone());
            tab.yank_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(highlight_for).await;
                let _ = this.update(cx, |this, cx| this.vim_end_yank_highlight(tab_id, cx));
            }));
        }
        if edit.is_some() || select.is_some() || scroll.is_some() {
            editor.update(cx, |state, cx| {
                // One atomic edit: an undo step of its own.
                if let Some((range, text)) = edit {
                    state.set_selected_range(range, cx);
                    state.replace(text, window, cx);
                }
                if let Some((range, reversed)) = select {
                    state.set_selected_range_with_direction(range, reversed, cx);
                }
                if let Some(scroll) = scroll {
                    scroll_editor(state, scroll, cx);
                }
            });
        }
        if let Some(history) = history {
            self.vim_queue_history(tab_id, history, window, cx);
        }
        if after == Mode::Normal && before != after {
            self.vim_clamp_caret(tab_id, cx);
        }
        if before != after {
            // At once, not at the next render: a key typed right after `i` is already text.
            self.vim_apply_readonly(tab_id, cx);
        }
        if before != after || prompt_before || prompt_after || had_yank || yanked.is_some() {
            self.notify_pane_views(cx);
        }
    }

    /// The highlight timer ran out.
    fn vim_end_yank_highlight(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.query_editors.vim.tabs.get_mut(&tab_id) {
            tab.yank = None;
            tab.yank_task = None;
        }
        self.notify_pane_views(cx);
    }
}

/// Scrolls the editor's view as a command asks: the cursor's line to the middle / top / bottom of the
/// view, or the text by lines (`Ctrl-D`). The caret's place is the one the editor drew last.
fn scroll_editor(state: &mut EditorState, scroll: Scroll, cx: &mut Context<EditorState>) {
    let Some(line_height) = state.line_height() else {
        return;
    };
    if let Scroll::Lines(lines) = scroll {
        state.scroll_by_y_f64(-(lines as f64) * f64::from(f32::from(line_height)), cx);
        return;
    }
    let Some(geometry) = state.prepaint_caret_geometry() else {
        return;
    };
    let row = geometry.line_height();
    let area = geometry.input_bounds();
    let now = geometry.cursor_bounds().origin.y + geometry.scroll_offset().y;
    let want = match scroll {
        Scroll::Top => area.top(),
        Scroll::Bottom => area.bottom() - row,
        _ => area.center().y - row / 2.0,
    };
    state.scroll_by_y_f64(f64::from(f32::from(want - now)), cx);
}
