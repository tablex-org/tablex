//! "Reset Saved Passwords…": the way out when the master key of the saved passwords is gone or
//! does not match (see `engine_connect::secret_store`). One confirmation, then the secrets table
//! of the app database is emptied off the UI thread. Connections, history, settings and every
//! other table are not touched.

use super::*;
use crate::credentials::CredentialError;

/// What the confirmation says. The names are the same as the error texts use.
pub(super) const RESET_WARNING: &str = "All saved connection passwords, SSH passwords and key \
    passphrases, and the AI API key are removed from tablex (the items older versions kept in the \
    system credential store included), and you have to enter them again. \
    Your connections, query history and settings are kept. This cannot be undone.";

impl AppRoot {
    /// Asks once; on confirmation resets the saved passwords.
    pub(super) fn confirm_reset_saved_passwords(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Reset saved passwords?")
                .child(RESET_WARNING)
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Reset"))
                .confirm()
                .on_ok(move |_, window, cx| {
                    let _ = root.update(cx, |this, cx| this.reset_saved_passwords(window, cx));
                    true
                })
        });
    }

    /// Empties the secrets table (the OS may be asked for the new master key: not on the UI
    /// thread) and says how it went.
    fn reset_saved_passwords(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.connection_ui.credentials.clone();
        let job = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || store.reset()).await });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| this.saved_passwords_reset(result, window, cx));
        })
        .detach();
    }

    fn saved_passwords_reset(
        &mut self,
        result: Result<Vec<String>, CredentialError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(leftovers) if leftovers.is_empty() => {
                self.sidebar.notice = Some("Saved passwords were reset. Enter the passwords again.".into());
                window.push_notification("Saved passwords were reset. Enter the passwords again.", cx);
            }
            // The table is reset; items of an earlier version that the OS would not let go are said.
            Ok(leftovers) => {
                let message = format!(
                    "Saved passwords were reset, but {} older item(s) in the {} could not be removed ({}). \
                     Enter the passwords again.",
                    leftovers.len(),
                    crate::platform::CREDENTIAL_STORE_LABEL,
                    leftovers.join("; ")
                );
                self.report_warning(Area::App, None, &message);
                self.sidebar.notice = Some(message.clone());
                window.push_notification(message, cx);
            }
            Err(error) => {
                let message = format!("Could not reset the saved passwords: {error}");
                self.report_error(Area::App, None, message.clone());
                self.sidebar.notice = Some(message.clone());
                window.push_notification(message, cx);
            }
        }
        cx.notify();
    }
}
