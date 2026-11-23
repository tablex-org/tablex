//! Basic Vim keys in the query editors (SQL, MongoDB script, Redis console). The state machine is
//! `crate::vim`; this file feeds it the keys of an editor that has Vim on and applies what it asks
//! to the editor's `EditorState`.
//!
//! Vim is on for a tab when the switch in the editor's status line says so, else when Settings >
//! Editor > Vim keys does (the switch is session-only). The editor is wrapped in a div only while Vim
//! is on, so a tab without it renders and behaves exactly as before.
//!
//! Key flow (gpui dispatches bound actions first, then key listeners, capture before bubble):
//! plain keys are not bound in the editor, so the wrapper's `capture_key_down` takes them and stops
//! them before the editor can insert them (Normal and Visual; in Insert they pass). The keys the
//! editor binds and Vim changes the meaning of (Escape, Enter, Backspace, Delete, Tab, Cut, Paste,
//! the delete-word / delete-line chords, and Ctrl-[ off macOS) are taken with `capture_action`.
//! Other Cmd / Alt chords are never touched, so the app's shortcuts (Run, Save, Format ...) work in
//! every mode; the editor's completion menu keeps Enter, Escape and the arrows in Insert mode.
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
use crate::vim::{Effect, History, Key, Mode, Register, Vim};
use gpui_component::input::{
    Backspace, Cut, Delete, DeleteToBeginningOfLine, DeleteToEndOfLine, DeleteToNextWordEnd,
    DeleteToPreviousWordStart, Enter as InputEnter, Escape as InputEscape, IndentInline, Outdent,
    OutdentInline, Paste, Redo, Undo,
};

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
}

/// Next-frame rounds a waiting Undo / Redo gets for its frame to be drawn (a tab that is not on
/// screen is never drawn: the keys are dropped).
const HISTORY_FRAMES: u8 = 4;

/// Vim of all editors: the register is shared by every tab.
#[derive(Default)]
pub(super) struct VimState {
    register: Register,
    tabs: HashMap<u64, VimTab>,
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
        self.query_editors.vim.tabs.get(&tab_id).map_or(Mode::Normal, |tab| tab.vim.mode())
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
        let tabs = &mut self.query_editors.vim.tabs;
        match tabs.get_mut(&tab_id) {
            Some(tab) if tab.running != on => {
                tab.running = on;
                tab.vim = Vim::default();
            }
            None if on => {
                tabs.insert(tab_id, VimTab { running: true, ..VimTab::default() });
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
        let tab = self.query_editors.vim.tabs.entry(tab_id).or_default();
        tab.enabled = Some(on);
        tab.running = on;
        tab.vim = Vim::default();
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
            .capture_action(cx.listener(move |this, _: &InputEscape, window, cx| {
                this.vim_escape(tab_id, window, cx)
            }))
            // The editor's own meaning of these keys is an edit: only Insert mode has it.
            .capture_action(cx.listener(move |this, action: &InputEnter, window, cx| {
                if !action.secondary {
                    this.vim_swallow(tab_id, window, cx);
                } else if this.vim_editor(tab_id, window, cx).is_some() && this.vim_mode(tab_id) != Mode::Insert {
                    // Cmd+Enter is the editor's Run; a read-only editor does not handle it any more.
                    this.run_tab_query_explicit(tab_id, cx);
                    cx.stop_propagation();
                }
            }))
            .capture_action(cx.listener(move |this, _: &Backspace, window, cx| this.vim_swallow(tab_id, window, cx)))
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
                this.vim_history_action(tab_id, History::Undo, window, cx)
            }))
            .capture_action(cx.listener(move |this, _: &Redo, window, cx| {
                this.vim_history_action(tab_id, History::Redo, window, cx)
            }))
            // Ctrl-[ is Escape; the editor binds it to Outdent off macOS (on macOS that is Cmd-[).
            .when(cfg!(not(target_os = "macos")), |this| {
                this.capture_action(cx.listener(move |this, _: &Outdent, window, cx| {
                    this.vim_feed_key(tab_id, Key::Esc, window, cx)
                }))
            })
            .child(content)
            // Normal and Visual mode: a box over the char, not the editor's bar (see `vim_caret`).
            .children(self.query_editors.by_tab.get(&tab_id).filter(|_| matches!(self.vim_mode(tab_id), Mode::Normal | Mode::Visual)).map(
                |editor| vim_caret::BlockCaret::new(editor.clone(), self.vim_mode(tab_id), cx),
            ))
            .into_any_element()
    }

    /// Stops an editing key the editor would handle, unless the tab is in Insert mode.
    fn vim_swallow(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.vim_editor(tab_id, window, cx).is_some() && self.vim_mode(tab_id) != Mode::Insert {
            cx.stop_propagation();
        }
    }

    fn vim_escape(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.vim_editor(tab_id, window, cx) else {
            return;
        };
        let mode = self.vim_mode(tab_id);
        // The completion menu closes first; Escape in Normal mode only cancels a pending command
        // (and passes on to the app).
        if mode == Mode::Insert && editor.read(cx).completion_menu_state().open {
            return;
        }
        self.vim_feed(tab_id, &editor, Key::Esc, window, cx);
        if mode != Mode::Normal {
            cx.stop_propagation();
        }
    }

    fn vim_feed_key(&mut self, tab_id: u64, key: Key, window: &mut Window, cx: &mut Context<Self>) {
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
            match stroke.key.as_str() {
                "[" => Key::Esc,
                "r" if !modifiers.shift => Key::CtrlR,
                _ => return,
            }
        } else {
            let mut chars = stroke.key_char.as_deref().unwrap_or_default().chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_control() => Key::Char(ch),
                _ => return,
            }
        };
        // In Insert mode the keys are typing.
        if key != Key::Esc && self.vim_mode(tab_id) == Mode::Insert {
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
        let VimState { register, tabs } = &mut self.query_editors.vim;
        let Some(tab) = tabs.get_mut(&tab_id) else {
            return;
        };
        let before = tab.vim.mode();
        let effect = {
            let state = editor.read(cx);
            tab.vim.key(key, state.text(), state.selected_range(), state.is_selection_reversed(), register)
        };
        let after = tab.vim.mode();
        let Effect { edit, select, register: new_register, copy, history } = effect;
        if let Some(new_register) = new_register {
            if copy {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(new_register.text.clone()));
            }
            *register = new_register;
        }
        if edit.is_some() || select.is_some() {
            editor.update(cx, |state, cx| {
                // One atomic edit: an undo step of its own.
                if let Some((range, text)) = edit {
                    state.set_selected_range(range, cx);
                    state.replace(text, window, cx);
                }
                if let Some((range, reversed)) = select {
                    state.set_selected_range_with_direction(range, reversed, cx);
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
            self.notify_pane_views(cx);
        }
    }
}
