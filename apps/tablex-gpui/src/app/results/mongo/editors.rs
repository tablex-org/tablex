use super::super::super::*;

impl AppRoot {
    pub(in crate::app) fn mongo_filter_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(editor) = self.mongo_ui.filter_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.filter.clone())
            .unwrap_or_else(|| "{}".into());
        let value = value.replace(['\n', '\r'], " ");
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("{}")
                .default_value(value)
        });
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                match event {
                    gpui_component::input::InputEvent::Change => {
                        let value = editor_for_events.read(cx).value().to_string();
                        if this
                            .workspace
                            .runtime(tab_id)
                            .is_some_and(|runtime| runtime.mongo.find_expanded)
                        {
                            return;
                        }
                        if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                            runtime.mongo.filter = value;
                            runtime.mongo.mark_filter_draft();
                            runtime.mongo.inspector.error = None;
                        }
                        this.notify_tab(tab_id, cx);
                    }
                    gpui_component::input::InputEvent::PressEnter { .. } => {
                        this.run_tab_query(tab_id, cx);
                    }
                    _ => {}
                }
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
    ) -> gpui::Entity<InputState> {
        if let Some(editor) = self.mongo_ui.sort_editors.get(&tab_id) {
            return editor.clone();
        }
        let value = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.mongo.sort.clone())
            .unwrap_or_else(|| r#"{"_id":1}"#.into());
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("{ createdAt: -1 }")
                .default_value(value)
        });
        let editor_for_events = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                match event {
                    gpui_component::input::InputEvent::Change => {
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
                    }
                    gpui_component::input::InputEvent::PressEnter { .. } => {
                        this.run_tab_query(tab_id, cx);
                    }
                    _ => {}
                }
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
                .completion(completion)
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
                .completion(completion)
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
