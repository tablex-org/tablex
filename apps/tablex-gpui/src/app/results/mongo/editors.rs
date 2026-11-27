use super::super::super::*;

/// What an empty inline FILTER box shows as a hint.
pub(in crate::app) const FILTER_HINT: &str = "{}";
/// What an empty inline SORT box shows as a hint.
pub(in crate::app) const SORT_HINT: &str = "{ createdAt: -1 }";

/// The inline FILTER / SORT box (and the SQL table bar's WHERE / ORDER BY): the code editor of the
/// expanded boxes (`language`, completion), kept to one line. Enter is taken by [`single_line_keys`]
/// and a paste by [`paste_as_single_line`]. It has no placeholder of its own: the component paints one in the muted foreground, the colour of the
/// punctuation of a typed value, so it read as one (see [`empty_hint`]).
pub(in crate::app) fn single_line_editor(
    window: &mut Window,
    cx: &mut Context<EditorState>,
    language: &'static str,
    value: String,
    completion: TabCompletion,
) -> EditorState {
    EditorState::new(window, cx)
        .language(language)
        .line_number(false)
        // One line has nothing to fold, and the fold column would push the text right.
        .folding(false)
        .soft_wrap(false)
        .default_value(value)
        .completion(completion, window, cx)
}

/// Enter in the box calls `run` and is never a newline; Shift+Enter does nothing. Taken before the
/// editor sees it (a submit by the editor itself would also let the key's own "\n" through), except
/// while its completion menu is open: the menu then accepts the highlighted item.
pub(in crate::app) fn single_line_keys(
    element: gpui::Div,
    editor: &gpui::Entity<EditorState>,
    tab_id: u64,
    run: fn(&mut AppRoot, u64, &mut Context<AppRoot>),
    cx: &mut Context<AppRoot>,
) -> gpui::Div {
    let editor = editor.clone();
    element
        .capture_action(cx.listener(move |this, action: &gpui_component::input::Enter, _, cx| {
            if !action.shift && editor.read(cx).completion_menu_state().open {
                return;
            }
            cx.stop_propagation();
            if !action.shift {
                run(this, tab_id, cx);
            }
        }))
        // The key's own "\n" must not reach the editor after its action (one that accepts a completion too).
        .capture_key_down(|event: &gpui::KeyDownEvent, _, cx| {
            if event.keystroke.key == "enter" {
                cx.stop_propagation();
            }
        })
}

/// The inline box's view: the toolbar's own font (not the code editor's monospace), no frame, no padding.
pub(in crate::app) fn inline_editor(
    editor: &gpui::Entity<EditorState>,
    disabled: bool,
    cx: &gpui::App,
) -> Editor {
    use gpui_component::ActiveTheme as _;
    let for_paste = editor.clone();
    Editor::new(editor)
        .appearance(false)
        .xsmall()
        // A code editor is two rows tall at least (its rows cannot be set for one): the bar shows the
        // first, and a line as tall as the bar centers its text.
        .h(px(24.0))
        .line_height(px(24.0))
        .disabled(disabled)
        // No gutter to the left of the text (a code editor keeps a gap there even without line numbers):
        // the text starts at the box's own padding, where the hint does.
        .gutter_style(gpui_component::input::GutterStyle::default().with_gap(px(0.0)))
        .font_family(cx.theme().font_family.clone())
        // The size the plain input had before it became an editor.
        .text_sm()
        .on_paste(move |item, window, cx| paste_as_single_line(&for_paste, item, window, cx))
}

/// The hint of an empty inline box, laid over it where its text starts: the box's font and size in
/// the faintest text colour and italic, so it does not read as a value. `None` as soon as the box
/// holds any text (a space too), so nothing is built then. It has no mouse handler and no hitbox, so
/// a click on it reaches the editor under it. The parent must be `relative`.
pub(in crate::app) fn empty_hint(
    editor: &gpui::Entity<EditorState>,
    hint: &'static str,
    selector: &'static str,
    cx: &gpui::App,
) -> Option<gpui::Div> {
    use gpui_component::ActiveTheme as _;
    // The rope's length: reading the value would copy the text.
    (editor.read(cx).text().len() == 0).then(|| {
        div()
            .debug_selector(move || selector.into())
            .absolute()
            .top_0()
            .left(INLINE_TEXT_INSET)
            .h(px(24.0))
            .flex()
            .items_center()
            .whitespace_nowrap()
            .italic()
            .font_family(cx.theme().font_family.clone())
            .text_sm()
            .text_color(theme::text_faint())
            .child(hint)
    })
}

/// Where the inline box's text starts: the code editor's left padding at the box's (extra small) size.
const INLINE_TEXT_INSET: gpui::Pixels = px(4.0);

/// `text` with each line break (`\r\n`, `\n` or `\r`) as one space, in one pass.
fn flatten_line_breaks(text: &str) -> String {
    let mut flat = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                flat.push(' ');
            }
            '\n' => flat.push(' '),
            c => flat.push(c),
        }
    }
    flat
}

/// A paste with line breaks goes in as one line (the breaks become spaces); anything else is the
/// editor's own paste.
pub(in crate::app) fn paste_as_single_line(
    editor: &gpui::Entity<EditorState>,
    item: &gpui::ClipboardItem,
    window: &mut Window,
    cx: &mut gpui::App,
) -> bool {
    let Some(text) = item.text().filter(|text| text.contains(['\n', '\r'])) else {
        return false;
    };
    let flat = flatten_line_breaks(&text);
    editor.update(cx, |state, cx| state.replace(flat, window, cx));
    true
}

impl AppRoot {
    pub(in crate::app) fn mongo_filter_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.mongo_ui.filter_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter.clone())
            .unwrap_or_else(|| "{}".into());
        let value = value.replace(['\n', '\r'], " ");
        // One line like the expanded editor's twin, with the same completion: Enter runs (it is
        // never a newline), see `single_line_editor`.
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Filter);
        let editor = cx.new(|cx| single_line_editor(window, cx, "json", value, completion));
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                if this
                    .workspace
                    .runtime(tab_id)
                    .is_some_and(|runtime| runtime.mongo.find_expanded)
                {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.mongo.filter = value;
                    runtime.mongo.mark_filter_draft();
                    runtime.mongo.inspector.error = None;
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        self.mongo_ui.filter_editors.insert(tab_id, editor.clone());
        editor
    }

    pub(in crate::app) fn mongo_sort_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.mongo_ui.sort_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.sort.clone())
            .unwrap_or_else(|| r#"{"_id":1}"#.into());
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Sort);
        let editor = cx.new(|cx| single_line_editor(window, cx, "json", value, completion));
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                if this
                    .workspace
                    .runtime(tab_id)
                    .is_some_and(|runtime| runtime.mongo.sort_expanded)
                {
                    return;
                }
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.mongo.sort = value;
                    runtime.mongo.inspector.error = None;
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        self.mongo_ui.sort_editors.insert(tab_id, editor.clone());
        editor
    }

    pub(in crate::app) fn mongo_expanded_find_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.mongo_ui.expanded_find_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter.clone())
            .unwrap_or_else(|| "{}".into());
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Filter);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(false)
                .soft_wrap(false)
                .default_value(value)
                .completion(completion, window, cx)
        });
        let editor_for_events = editor.clone();
        cx.subscribe_in(
            &editor,
            window,
            move |this: &mut Self,
                  _: &gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, gpui_component::input::InputEvent::Blur) {
                    this.view_mut(tab_id).mongo.find_enter_held = false;
                    return;
                }
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.mongo.filter = value.clone();
                    runtime.mongo.mark_filter_draft();
                    runtime.mongo.inspector.error = None;
                }
                if let Some(compact) = this.mongo_ui.filter_editors.get(&tab_id).cloned() {
                    let compact_value = value.replace(['\n', '\r'], " ");
                    if compact.read(cx).value().as_ref() != compact_value {
                        compact.update(cx, |state, cx| state.set_value(compact_value, window, cx));
                    }
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        self.mongo_ui
            .expanded_find_editors
            .insert(tab_id, editor.clone());
        editor
    }

    pub(in crate::app) fn mongo_expanded_sort_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.mongo_ui.expanded_sort_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.sort.clone())
            .unwrap_or_else(|| r#"{"_id":1}"#.into());
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Sort);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(false)
                .soft_wrap(false)
                .default_value(value)
                .completion(completion, window, cx)
        });
        let editor_for_events = editor.clone();
        cx.subscribe_in(
            &editor,
            window,
            move |this: &mut Self,
                  _: &gpui::Entity<EditorState>,
                  event: &gpui_component::input::InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, gpui_component::input::InputEvent::Blur) {
                    this.view_mut(tab_id).mongo.sort_enter_held = false;
                    return;
                }
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    runtime.mongo.sort = value.clone();
                    runtime.mongo.inspector.error = None;
                }
                if let Some(compact) = this.mongo_ui.sort_editors.get(&tab_id).cloned() {
                    let compact_value = value.replace(['\n', '\r'], " ");
                    if compact.read(cx).value().as_ref() != compact_value {
                        compact.update(cx, |state, cx| state.set_value(compact_value, window, cx));
                    }
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        self.mongo_ui
            .expanded_sort_editors
            .insert(tab_id, editor.clone());
        editor
    }

    pub(in crate::app) fn mongo_column_search_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(editor) = self.mongo_ui.column_search_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.column_search.clone())
            .unwrap_or_default();
        let editor = Self::new_column_search_input(tab_id, value, window, cx, |this, tab_id, text| {
            if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                runtime.mongo.column_search = text;
            }
        });
        self.mongo_ui
            .column_search_editors
            .insert(tab_id, editor.clone());
        editor
    }

    pub(in crate::app) fn mongo_filter_rule_editor(
        &mut self,
        tab_id: u64,
        rule_id: u64,
        value: String,
        field: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        let editors = if field {
            &mut self.mongo_ui.filter_field_editors
        } else {
            &mut self.mongo_ui.filter_value_editors
        };
        if let Some(editor) = editors.get(&(tab_id, rule_id)) {
            return editor.clone();
        }
        let editor = cx.new(|cx| InputState::new(window, cx).default_value(value));
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                if !matches!(event, gpui_component::input::InputEvent::Change) {
                    return;
                }
                let value = editor_for_events.read(cx).value().to_string();
                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                    if let Some(rule) = runtime
                        .mongo
                        .filter_rules
                        .iter_mut()
                        .find(|rule| rule.id == rule_id)
                    {
                        if field {
                            rule.field = value;
                        } else {
                            rule.value = value;
                        }
                        runtime.mongo.mark_filter_draft();
                    }
                }
                this.notify_tab(tab_id, cx);
            },
        )
        .detach();
        editors.insert((tab_id, rule_id), editor.clone());
        editor
    }
}

#[cfg(test)]
mod flatten_tests {
    use super::flatten_line_breaks;

    #[test]
    fn every_line_break_becomes_one_space() {
        assert_eq!(flatten_line_breaks("a\r\nb\nc\rd\n\ne"), "a b c d  e");
        assert_eq!(flatten_line_breaks("{\"é\": 1}"), "{\"é\": 1}");
    }
}
