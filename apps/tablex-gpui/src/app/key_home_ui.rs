//! Where the master key of the saved passwords lives (see `engine_connect::key_home`), on the
//! platforms where that is a choice (Linux and other Unix without a Keychain): the question asked
//! once when no Secret Service runs, and Settings > Security to change the answer later.
//! macOS and Windows never get here: their credential store is always there.

use super::*;
use engine_connect::credentials::key_store;
use engine_connect::key_home::{self, Home, Need};
use gpui::SharedString;

/// What the app knows about the key's home.
pub(super) struct KeyHomeUi {
    /// Where the key lives now (the choice file).
    pub(super) home: Home,
    /// Whether a Secret Service runs; `None` until the check at the start has answered.
    pub(super) keyring_available: Option<bool>,
    checked: bool,
    /// A switch runs off the UI thread.
    pub(super) changing: bool,
}

impl Default for KeyHomeUi {
    fn default() -> Self {
        Self {
            // Tests never read the machine's data directory.
            #[cfg(not(test))]
            home: key_store().home().unwrap_or(Home::Keyring),
            #[cfg(test)]
            home: Home::Keyring,
            keyring_available: None,
            checked: false,
            changing: false,
        }
    }
}

/// Tells the connection model whether passwords are saved at all: after "Don't save passwords"
/// every connection asks for its password. Before the saved connections are read.
pub(super) fn apply_saved_choice() {
    #[cfg(not(test))]
    engine_connect::credentials::apply_saved_choice();
}

impl AppRoot {
    /// From the first frame, once: looks for a Secret Service off the UI thread and asks the
    /// question when there is none and nothing was chosen yet.
    pub(super) fn check_key_home(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = &mut self.connection_ui.key_home;
        if state.checked || !key_home::FALLBACK_SUPPORTED || cfg!(test) || self.scenario_ui.service.is_some() {
            return;
        }
        state.checked = true;
        let store = key_store();
        let job = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                let available = store.keyring_available();
                (store.need(), available)
            })
            .await
        });
        cx.spawn_in(window, async move |this, cx| {
            let (need, available) = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.connection_ui.key_home.keyring_available = Some(available);
                match need {
                    Need::Choice => this.open_key_home_dialog(window, cx),
                    Need::KeyUnreachable { .. } => this.open_key_unreachable_dialog(window, cx),
                    Need::Nothing => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The question. Escape and the second button both mean "Don't save passwords", the safe
    /// answer; the local file has to be asked for.
    pub(super) fn open_key_home_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("key-home");
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (ok_root, cancel_root) = (root.clone(), root.clone());
            let paragraph = |text: std::borrow::Cow<'static, str>| div().child(text);
            dialog
                .title(ts!("security.dialog.title"))
                .width(px(540.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .text_size(px(13.0))
                        .child(paragraph(t!("security.dialog.intro")))
                        .child(paragraph(t!("security.dialog.file")))
                        .child(paragraph(t!("security.dialog.none")))
                        .child(paragraph(t!("security.dialog.keyring"))),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(ts!("security.dialog.use_file"))
                        .cancel_text(ts!("security.dialog.no_save")),
                )
                .confirm()
                .on_ok(move |_, window, cx| {
                    let _ = ok_root.update(cx, |this, cx| this.choose_key_home(Home::File, window, cx));
                    true
                })
                .on_cancel(move |_, window, cx| {
                    let _ = cancel_root.update(cx, |this, cx| this.choose_key_home(Home::NoSave, window, cx));
                    true
                })
        });
    }

    /// Saved secrets exist and their key is out of reach (see `engine_connect::key_home`): no
    /// file, no permanent "don't save". The one button, and Escape, stop saving for this session
    /// only; the real way out is a running keyring and a restart.
    pub(super) fn open_key_unreachable_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        #[cfg(test)]
        self.tunnel_ui.opened_dialogs.push("key-unreachable");
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let (ok_root, cancel_root) = (root.clone(), root.clone());
            let paragraph = |text: std::borrow::Cow<'static, str>| div().child(text);
            dialog
                .title(ts!("security.dialog.unreachable_title"))
                .width(px(540.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .text_size(px(13.0))
                        .child(paragraph(t!("security.dialog.unreachable")))
                        .child(paragraph(t!("security.dialog.unreachable_session"))),
                )
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(ts!("security.dialog.session_no_save")),
                )
                .on_ok(move |_, window, cx| {
                    let _ = ok_root.update(cx, |this, cx| this.no_save_this_session(window, cx));
                    true
                })
                .on_cancel(move |_, window, cx| {
                    let _ = cancel_root.update(cx, |this, cx| this.no_save_this_session(window, cx));
                    true
                })
        });
    }

    /// Stops saving passwords until the app quits: nothing is written to disk, so the next start
    /// asks again (and a running keyring then simply works).
    pub(super) fn no_save_this_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        key_home::set_passwords_saved(false);
        self.refresh_saved_connections(cx);
        window.push_notification(t!("security.changed.session").into_owned(), cx);
        cx.notify();
    }

    /// Moves the key to `target` off the UI thread (the keyring may ask to unlock) and says how it
    /// went. Nothing is lost on a failure: see `KeyHomeStore::switch`.
    pub(super) fn choose_key_home(&mut self, target: Home, window: &mut Window, cx: &mut Context<Self>) {
        // A reset or another move runs: say so (the dialog buttons stay enabled meanwhile).
        if self.connection_ui.key_home.changing {
            window.push_notification(super::reset_passwords::KEY_BEING_MOVED, cx);
            return;
        }
        self.connection_ui.key_home.changing = true;
        let job = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || key_store().switch(target)).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| this.key_home_changed(target, result, window, cx));
        })
        .detach();
        cx.notify();
    }

    fn key_home_changed(
        &mut self,
        target: Home,
        result: Result<(), crate::credentials::CredentialError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connection_ui.key_home.changing = false;
        match result {
            Ok(()) => {
                self.connection_ui.key_home.home = target;
                key_home::set_passwords_saved(target != Home::NoSave);
                // What the process kept of the old home goes: the next read asks the new one.
                engine_connect::credentials::default_store().forget_key();
                // Connections read as saving or not saving their passwords.
                self.refresh_saved_connections(cx);
                let message = match target {
                    Home::File => t!("security.changed.file"),
                    Home::Keyring => t!("security.changed.keyring"),
                    Home::NoSave => t!("security.changed.none"),
                };
                window.push_notification(message.into_owned(), cx);
            }
            Err(error) => {
                let message = t!("security.changed.failed", error = error.to_string()).into_owned();
                self.report_error(Area::App, None, message.clone());
                window.push_notification(message, cx);
            }
        }
        cx.notify();
    }

    /// Settings > Security: where the key is, and the ways to move it.
    pub(super) fn render_security_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let state = &self.connection_ui.key_home;
        let home = state.home;
        let (store, busy) = (key_store(), state.changing);
        let description = match home {
            Home::Keyring => t!("security.page.in_keyring").into_owned(),
            Home::File => t!(
                "security.page.in_file",
                path = store.key_path().map_or_else(String::new, |path| path.display().to_string())
            )
            .into_owned(),
            Home::NoSave => t!("security.page.not_saving").into_owned(),
        };
        let note = match (home, state.keyring_available) {
            (Home::Keyring, Some(false)) | (Home::NoSave | Home::File, Some(false)) => {
                Some(t!("security.page.no_keyring").into_owned())
            }
            (_, None) => Some(t!("security.page.checking").into_owned()),
            _ => None,
        };
        let keyring_ready = state.keyring_available == Some(true);
        let button = |id: &'static str, label: SharedString, target: Home, enabled: bool| {
            Button::new(id)
                .debug_selector(move || id.into())
                .label(label)
                .compact()
                .disabled(!enabled || busy)
                .on_click(cx.listener(move |this, _, window, cx| this.choose_key_home(target, window, cx)))
        };
        let mut buttons: Vec<Button> = Vec::new();
        match home {
            Home::Keyring => {
                buttons.push(button("security-use-file", ts!("security.dialog.use_file"), Home::File, true));
                buttons.push(button("security-no-save", ts!("security.dialog.no_save"), Home::NoSave, true));
            }
            Home::File => {
                buttons.push(button("security-use-keyring", ts!("security.page.use_keyring"), Home::Keyring, keyring_ready));
                buttons.push(button("security-no-save", ts!("security.dialog.no_save"), Home::NoSave, true));
            }
            Home::NoSave => {
                buttons.push(button("security-use-file", ts!("security.dialog.use_file"), Home::File, true));
                buttons.push(button("security-use-keyring", ts!("security.page.use_keyring_again"), Home::Keyring, keyring_ready));
            }
        }
        div()
            .debug_selector(|| "settings-security".into())
            .flex()
            .flex_col()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(6.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme::text())
                    .child(t!("security.page.heading")),
            )
            .child(
                div()
                    .debug_selector(|| "security-state".into())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(description),
            )
            .children(note.map(|note| {
                div()
                    .text_size(px(12.0))
                    .text_color(theme::text_faint())
                    .child(note)
            }))
            .child(div().flex().gap(px(8.0)).children(buttons))
            .into_any_element()
    }
}
