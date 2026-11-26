//! The one-time notice after the settings moved to schema 7. Until then a switch ("Let MCP clients
//! run changes") had to be on before an MCP client could ask to change data; the switch is gone and
//! the access level alone decides, each change still asked about one by one. When the switch was
//! off, the connections that are set to Read and write could not be changed through MCP and now can
//! be asked to: this tells the user which ones, with a way to Settings > AI. The migration flags it
//! (`ai.access_notice_pending`); showing it (or finding nothing to say) clears the flag, so it never
//! repeats.

use super::*;
use engine_agent::AgentAccess;
use gpui::SharedString;
use gpui_component::notification::{Notification, NotificationType};
use gpui_component::ActiveTheme as _;

/// Marks the notice, so a second one replaces the first instead of stacking.
struct AccessNotice;

/// How many connection names the notice lists before "and N more".
const NAMES_SHOWN: usize = 5;

impl AppRoot {
    /// From a frame with a window: runs the notice once when the migration flagged it.
    pub(super) fn show_access_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings.ai.access_notice_pending {
            cx.defer_in(window, |this, window, cx| this.run_access_notice(window, cx));
        }
    }

    /// The connections whose effective agent access is Read and write, by name.
    fn read_write_connection_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .saved_connections
            .iter()
            .filter(|saved| !saved.disabled)
            .filter(|saved| {
                let effective = self.effective_for(&saved.id.to_string());
                // A production connection set to Read and write is still refused by MCP.
                effective.agent_access == AgentAccess::ReadWrite
                    && !effective.connection_type.as_ref().is_some_and(|ty| ty.is_production())
            })
            .map(|saved| saved.name.clone())
            .collect();
        names.sort_by_key(|name| name.to_lowercase());
        names
    }

    fn run_access_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings.ai.access_notice_pending {
            return;
        }
        // The list of connections could not be read: an empty list says nothing. The flag stays, and
        // the next launch looks again.
        if self.settings_ui.connections_unreadable {
            return;
        }
        let names = self.read_write_connection_names();
        // Recorded first: whatever happens next, it is not shown again.
        let mut next = (**self.settings).clone();
        next.ai.access_notice_pending = false;
        self.apply_settings(next, Some(window), cx);
        if names.is_empty() {
            return;
        }
        let mut list = names.iter().take(NAMES_SHOWN).cloned().collect::<Vec<_>>().join(", ");
        if names.len() > NAMES_SHOWN {
            list.push_str(&t!("access_notice.more", count = names.len() - NAMES_SHOWN));
        }
        let message = SharedString::from(t!("access_notice.message", names = list).into_owned());
        #[cfg(test)]
        self.settings_ui.access_notices.push(message.to_string());
        let root = cx.entity().downgrade();
        let notice = Notification::new()
            .id::<AccessNotice>()
            .with_type(NotificationType::Info)
            .title(t!("access_notice.title").into_owned())
            .autohide(false)
            .content(move |_, _, cx| {
                let root = root.clone();
                gpui_component::v_flex()
                    .child(div().text_sm().text_color(cx.theme().muted_foreground).child(message.clone()))
                    .child(
                        gpui_component::h_flex().gap_2().mt_2().child(
                            gpui_component::button::Button::new("access-notice-settings")
                                .debug_selector(|| "access-notice-settings".into())
                                .label(t!("access_notice.open_settings").into_owned())
                                .small()
                                .on_click(move |_, window, cx| {
                                    window.remove_notification::<AccessNotice>(cx);
                                    let _ = root.update(cx, |this, cx| this.open_ai_settings(window, cx));
                                }),
                        ),
                    )
                    .into_any_element()
            });
        window.push_notification(notice, cx);
    }
}
