//! The extra rows of Settings › AI Assistant: the API key of the selected provider (saved straight
//! to the system credential store, not through Apply, and never shown again) and the models
//! Refresh. The other rows (provider, model, effort, fallback, base URL) are ordinary registry
//! descriptors; the model and the address are the selected provider's.

use super::assistant::KeyState;
use super::*;
use crate::settings::AiProvider;
use gpui::{Entity, SharedString};
use gpui_component::input::InputState;

const KEY_WIDTH: f32 = 220.0;

/// The controls of the AI page that are not registry rows.
pub(super) struct AiPage {
    /// The key being typed; emptied the moment it is saved.
    pub(super) key_input: Option<Entity<InputState>>,
    /// The provider the typed key was typed for: a key is never kept across a change of provider.
    key_for: AiProvider,
    /// The provider and `models_version` the model picker's items were last loaded at.
    pub(super) models_seen: (AiProvider, u64),
}

impl Default for AiPage {
    fn default() -> Self {
        Self {
            key_input: None,
            key_for: AiProvider::Anthropic,
            models_seen: (AiProvider::Anthropic, 0),
        }
    }
}

impl AppRoot {
    /// The API key row of the provider the page has selected. The key goes from the input to the
    /// credential store and the input is emptied; nothing shows it again, only whether one is
    /// saved.
    pub(super) fn render_ai_key_row(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(provider) = self
            .settings_ui
            .page
            .as_ref()
            .map(|page| page.draft.ai.provider)
        else {
            return div().into_any_element();
        };
        // Whether a key is saved is asked of the credential store once, off the UI thread.
        self.assistant_refresh_key(provider, cx);
        let input = {
            let Some(page) = self.settings_ui.page.as_mut() else {
                return div().into_any_element();
            };
            // A key typed for another provider is not this one's: the box starts empty.
            if page.ai.key_for != provider {
                page.ai.key_input = None;
                page.ai.key_for = provider;
            }
            page.ai
                .key_input
                .get_or_insert_with(|| {
                    cx.new(|cx| {
                        InputState::new(window, cx)
                            .masked(true)
                            .placeholder("Paste your API key")
                    })
                })
                .clone()
        };
        let busy = self.assistant.key_busy(provider);
        let state = self.assistant.key_state(provider);
        let saved = *state == KeyState::Set;
        let optional = !provider.requires_key();
        let status = self.assistant.key_state_label(provider);
        let problem = self
            .assistant
            .key_error(provider)
            .map(str::to_owned)
            .or_else(|| match state {
                KeyState::Unavailable(why) => Some(why.clone()),
                _ => None,
            });
        let detail = if optional {
            format!(
                "{} works without a key; add one only if your server asks for it. Kept in the \
                 system credential store, never in the settings file. Saving does not wait for \
                 Apply, and a saved key is never shown again.",
                provider.label()
            )
        } else {
            format!(
                "The {} key. Kept in the system credential store, never in the settings file. \
                 Saving does not wait for Apply, and a saved key is never shown again.",
                provider.label()
            )
        };
        div()
            .id("ai-key-row")
            .debug_selector(|| "ai-key-row".into())
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.0))
            .px(px(12.0))
            .py(px(10.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme::text())
                            .child(if optional {
                                "API key (optional)"
                            } else {
                                "API key"
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(SharedString::from(detail)),
                    )
                    .child(
                        div()
                            .debug_selector(|| "ai-key-status".into())
                            .text_size(px(12.0))
                            .text_color(if saved {
                                theme::accent()
                            } else {
                                theme::text_faint()
                            })
                            .child(status),
                    )
                    .when_some(problem, |this, message| {
                        this.child(
                            div()
                                .debug_selector(|| "ai-key-error".into())
                                .text_size(px(12.0))
                                .text_color(theme::danger())
                                .child(message),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .flex_shrink_0()
                    .child(
                        div()
                            .debug_selector(|| "ai-key-input".into())
                            .w(px(KEY_WIDTH))
                            .child(Input::new(&input).small()),
                    )
                    .child(
                        Button::new("ai-key-save")
                            .debug_selector(|| "ai-key-save".into())
                            .label("Save")
                            .compact()
                            .primary()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.settings_ai_save_key(window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-key-remove")
                            .debug_selector(|| "ai-key-remove".into())
                            .label("Remove")
                            .compact()
                            .disabled(busy || !saved)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.assistant_remove_key(provider, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Save: the typed key leaves the input at once and is stored off the UI thread, as the key of
    /// the provider the page has selected.
    pub(super) fn settings_ai_save_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((input, provider, typed_for)) = self.settings_ui.page.as_ref().and_then(|page| {
            Some((
                page.ai.key_input.clone()?,
                page.draft.ai.provider,
                page.ai.key_for,
            ))
        }) else {
            return;
        };
        let key = input.read(cx).value().to_string();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        // Typed for another provider (the page has not been drawn since the change): not stored.
        let key = if typed_for == provider {
            key
        } else {
            String::new()
        };
        self.assistant_save_key(provider, key, cx);
    }

    /// The models row: Refresh loads the provider's list (with the draft's address) and merges it
    /// into the picker.
    pub(super) fn render_ai_models_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let (provider, draft_base_url) = self.settings_ui.page.as_ref().map_or_else(
            || (AiProvider::Anthropic, String::new()),
            |page| {
                (
                    page.draft.ai.provider,
                    page.draft.ai.selected().base_url.clone(),
                )
            },
        );
        let refreshing = self.assistant.models_refreshing(provider);
        let error = self.assistant.models_error(provider).map(str::to_owned);
        div()
            .debug_selector(|| "ai-models-row".into())
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(4.0))
            .child(
                Button::new("ai-models-refresh")
                    .debug_selector(|| "ai-models-refresh".into())
                    .label(if refreshing {
                        "Refreshing\u{2026}"
                    } else {
                        "Refresh models"
                    })
                    .compact()
                    .disabled(refreshing)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let Some(ai) = this
                            .settings_ui
                            .page
                            .as_ref()
                            .map(|page| page.draft.ai.clone())
                        else {
                            return;
                        };
                        this.assistant_refresh_models(ai, cx);
                    })),
            )
            .child(
                div()
                    .debug_selector(|| "ai-models-note".into())
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.0))
                    .text_color(if error.is_some() {
                        theme::danger()
                    } else {
                        theme::text_muted()
                    })
                    .child(match error {
                        Some(message) => SharedString::from(message),
                        None => SharedString::from(models_note(
                            provider,
                            &draft_base_url,
                            self.assistant.models_for(provider).len(),
                        )),
                    }),
            )
            .into_any_element()
    }
}

impl AppRoot {
    /// Where conversations are kept, or why they are not.
    pub(super) fn ai_conversations_place(&self) -> String {
        match self.assistant_store() {
            Some(store) if store.root_is_link() => format!(
                "Not kept: {} is a link, and TableX does not use a link for this. Replace it \
                 with a real folder.",
                store.root().display()
            ),
            Some(store) => format!("Kept in {}.", store.root().display()),
            None => "Nothing is kept in this window.".to_owned(),
        }
    }

    /// The row under "Keep conversations on this computer": where they are, and "Delete all
    /// conversations" (after a confirmation) with the result line.
    pub(super) fn render_ai_conversations_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.assistant.delete_all.busy;
        let result = self.assistant.delete_all.result.as_ref();
        let place = self.ai_conversations_place();
        let can_delete = self.assistant_store().is_some();
        div()
            .debug_selector(|| "ai-conversations-row".into())
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(4.0))
            .child(
                Button::new("ai-conversations-delete")
                    .debug_selector(|| "ai-conversations-delete".into())
                    .label("Delete all conversations")
                    .compact()
                    .disabled(busy || !can_delete)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_delete_all_conversations(window, cx)
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .text_size(px(12.0))
                    .child(
                        div()
                            .debug_selector(|| "ai-conversations-place".into())
                            .text_color(theme::text_muted())
                            .child(SharedString::from(place)),
                    )
                    .children(result.map(|result| {
                        let (text, color) = match result {
                            Ok(report) => {
                                let count = match report.deleted {
                                    1 => "1 conversation deleted".to_owned(),
                                    count => format!("{count} conversations deleted"),
                                };
                                match &report.error {
                                    None => (count, theme::text()),
                                    Some(why) => (
                                        format!("{count}; could not delete the rest: {why}"),
                                        theme::danger(),
                                    ),
                                }
                            }
                            Err(why) => (format!("Could not delete: {why}"), theme::danger()),
                        };
                        div()
                            .debug_selector(|| "ai-conversations-result".into())
                            .text_color(color)
                            .child(SharedString::from(text))
                    })),
            )
            .into_any_element()
    }

    /// Asks once, then deletes every kept conversation of every connection.
    fn confirm_delete_all_conversations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Delete all conversations?")
                .child(
                    "Every conversation the assistant kept on this computer, for every \
                     connection, is deleted. This cannot be undone.",
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Delete all"),
                )
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| this.assistant_delete_all_conversations(cx));
                    true
                })
        });
    }
}

/// The help under Refresh: which address is asked, and with what.
pub(super) fn models_note(provider: AiProvider, draft_base_url: &str, count: usize) -> String {
    let key = if provider.requires_key() {
        "the saved API key"
    } else {
        "the saved API key, if there is one"
    };
    let asked = format!(
        "Refresh asks {} with {key} (the Base URL below, applied or not).",
        draft_base_url.trim()
    );
    match (count, provider) {
        (0, AiProvider::Anthropic) => format!("No models listed yet. {asked}"),
        (0, _) => {
            format!("No models listed yet: type the model id above, or ask the server. {asked}")
        }
        (count, _) => format!("{count} models in the list. {asked}"),
    }
}
