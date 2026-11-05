//! The History of the chat's connection: what the panel lists, and the app-level calls behind it
//! (open, delete one, delete a connection's, delete all).
//!
//! * Nothing is read or scanned while the dock is closed, and nothing polls: the list is read, off
//!   the UI thread, when the dock opens, when the chat is about another connection, and after a
//!   conversation was written or deleted.
//! * Opening reads the whole file off the UI thread; nothing is replaced until it has loaded and
//!   the engine accepted it. A file that does not load is reported once in the Error Log and left
//!   out of the list.
//! * "Keep conversations on this computer" off: nothing is written (the run is given no store);
//!   what exists stays listed and can be opened.

use super::session::Opened;
use super::store::{ConversationStore, DeleteReport, Listing, StoreError};
use super::*;
use gpui::SharedString;
use std::collections::HashSet;

/// One conversation as the History list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRow {
    pub id: String,
    pub title: SharedString,
    /// Milliseconds since the Unix epoch.
    pub updated: u64,
}

/// The list of one connection's conversations and the work that fills it.
#[derive(Default)]
pub(in crate::app) struct History {
    /// The connection the rows are of.
    connection: Option<String>,
    pub(in crate::app) rows: Arc<[HistoryRow]>,
    /// The latest reading; an older one that arrives late is ignored.
    generation: u64,
    task: Option<gpui::Task<()>>,
    /// The latest open / delete (the one in flight, if any) and which open it is.
    work: Option<gpui::Task<()>>,
    opening: u64,
    /// Files already reported to the Error Log (`connection/file:size`): reported once.
    reported: HashSet<String>,
    /// Conversations that failed to open (connection, id), left out of the list.
    broken: HashSet<(String, String)>,
    /// Why the last open failed, shown under the header until the next action.
    pub(in crate::app) note: Option<String>,
}

impl History {
    /// The rows to hand the panel for a chat about `chat_connection`, and the connection they were
    /// listed for. Rows of any other connection are not handed over (the list is read again for
    /// the chat's own), so a click on a row can never meet a connection it does not belong to; and
    /// whoever acts on a row acts on the connection returned here, not on whatever the chat is
    /// about by then.
    pub(in crate::app) fn rows_for(
        &self,
        chat_connection: Option<&str>,
    ) -> (Arc<[HistoryRow]>, SharedString) {
        let own = self.connection.as_deref().zip(chat_connection);
        match own {
            Some((listed, chat)) if listed.eq_ignore_ascii_case(chat) => {
                (self.rows.clone(), SharedString::from(listed.to_owned()))
            }
            _ => (Arc::from([]), SharedString::default()),
        }
    }

    /// An open is being read (tests).
    #[cfg(test)]
    pub(in crate::app) fn is_opening(&self) -> bool {
        self.work.is_some()
    }
}

/// "Delete all conversations" in Settings.
#[derive(Default)]
pub(in crate::app) struct DeleteAll {
    pub(in crate::app) busy: bool,
    /// The line under the button: how many went (and the first thing that failed), or why none
    /// did.
    pub(in crate::app) result: Option<Result<DeleteReport, StoreError>>,
    task: Option<gpui::Task<()>>,
}

/// "3 min ago", "yesterday", "12 Oct 2026": how long ago `updated` was at `now` (both in
/// milliseconds since the Unix epoch).
pub fn when(updated: u64, now: u64) -> String {
    use chrono::TimeZone as _;
    let seconds = now.saturating_sub(updated) / 1000;
    match seconds {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        86_400..=172_799 => "yesterday".to_owned(),
        172_800..=604_799 => format!("{} days ago", seconds / 86_400),
        _ => chrono::Local
            .timestamp_millis_opt(i64::try_from(updated).unwrap_or(i64::MAX))
            .single()
            .map_or_else(String::new, |time| time.format("%-d %b %Y").to_string()),
    }
}

impl AppRoot {
    /// The id of the chat's connection.
    fn history_connection(&self, cx: &gpui::App) -> Option<String> {
        self.assistant.chat().and_then(|chat| {
            chat.read(cx)
                .scope()
                .map(|scope| scope.connection_id.clone())
        })
    }

    /// Reads the chat's connection's list off the UI thread. Free while the dock is closed, and
    /// when nothing changed it for a connection already listed (`force`). The rows of another
    /// connection are gone at once: a click must never meet them.
    pub(in crate::app) fn assistant_refresh_history(
        &mut self,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.assistant.dock.open {
            return;
        }
        let connection = self.history_connection(cx);
        let history = &mut self.assistant.history;
        if !force && history.connection == connection {
            return;
        }
        if history.connection != connection {
            history.rows = Arc::from([]);
            history.note = None;
        }
        history.connection = connection.clone();
        history.generation += 1;
        let generation = history.generation;
        let (Some(store), Some(connection)) = (self.assistant.store.clone(), connection) else {
            history.rows = Arc::from([]);
            history.task = None;
            self.assistant_push_context(cx);
            return;
        };
        history.task = Some(cx.spawn(async move |this, cx| {
            let wanted = connection.clone();
            let listing = cx
                .background_executor()
                .spawn(async move { store.list(&wanted) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant_history_listed(generation, &connection, listing, cx)
            });
        }));
        self.assistant_push_context(cx);
    }

    /// Looks at the conversations folder once, off the UI thread: a link there is reported (once,
    /// in the Error Log) and shown in Settings, and nothing is written or listed through it.
    pub(in crate::app) fn assistant_check_store_root(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.assistant.store.clone() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let checked = cx
                .background_executor()
                .spawn(async move { store.check_root() })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = checked {
                    this.assistant_report_store_root(&error);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A store call that found its folder unusable says so in the Error Log, once per run of the
    /// app, whichever call found it first.
    fn assistant_report_store_root(&mut self, error: &StoreError) {
        if !matches!(error, StoreError::RootIsLink) {
            return;
        }
        if self
            .assistant
            .history
            .reported
            .insert("root-link".to_owned())
        {
            self.report_error(
                Area::App,
                None,
                format!("Assistant history: conversations are not kept ({error})"),
            );
        }
    }

    fn assistant_history_listed(
        &mut self,
        generation: u64,
        connection: &str,
        listing: Result<Listing, StoreError>,
        cx: &mut Context<Self>,
    ) {
        if self.assistant.history.generation != generation {
            return;
        }
        let history = &mut self.assistant.history;
        history.task = None;
        let mut reports = Vec::new();
        let mut link = false;
        match listing {
            Ok(listing) => {
                history.rows = listing
                    .entries
                    .iter()
                    .filter(|entry| {
                        !history
                            .broken
                            .contains(&(connection.to_owned(), entry.header.id.clone()))
                    })
                    .map(|entry| HistoryRow {
                        id: entry.header.id.clone(),
                        title: SharedString::from(entry.header.title.clone()),
                        updated: entry.header.updated,
                    })
                    .collect();
                for problem in listing.problems {
                    if history
                        .reported
                        .insert(format!("{connection}/{}:{}", problem.file, problem.bytes))
                    {
                        reports.push(format!(
                            "Assistant history: skipped the saved conversation {} ({})",
                            problem.file, problem.why
                        ));
                    }
                }
            }
            Err(StoreError::RootIsLink) => {
                history.rows = Arc::from([]);
                link = true;
            }
            Err(error) => {
                history.rows = Arc::from([]);
                reports.push(format!(
                    "Assistant history: could not list the conversations ({error})"
                ));
            }
        }
        if link {
            self.assistant_report_store_root(&StoreError::RootIsLink);
        }
        for report in reports {
            self.report_warning(Area::App, Some(connection), report);
        }
        self.assistant_push_context(cx);
        cx.notify();
    }

    /// Whether replacing the chat with a stored conversation would lose messages that are on no
    /// disk (the panel asks first).
    pub(in crate::app) fn assistant_chat_would_be_lost(&self, cx: &gpui::App) -> bool {
        self.assistant.chat().is_some_and(|chat| {
            let chat = chat.read(cx);
            chat.has_unsaved_messages() && !chat.is_empty()
        })
    }

    /// Opens a stored conversation of `connection` (which must be the chat's) in the chat.
    /// Refused while a run goes or a question waits (History is disabled then too). The file is
    /// read and its transcript built off the UI thread; nothing is replaced until both are done
    /// and the chat is still the one that asked: a damaged file is reported and the chat stays as
    /// it is.
    pub(in crate::app) fn assistant_open_conversation(
        &mut self,
        connection: String,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.assistant.chat().cloned() else {
            return;
        };
        if self.history_connection(cx).as_deref() != Some(connection.as_str()) {
            return;
        }
        let Some(store) = self.assistant.store.clone() else {
            return;
        };
        if chat.read(cx).is_running() || chat.read(cx).is_asking() {
            return;
        }
        let epoch = chat.read(cx).epoch();
        self.assistant.history.note = None;
        self.assistant.history.opening += 1;
        let generation = self.assistant.history.opening;
        self.assistant.history.work = Some(cx.spawn(async move |this, cx| {
            let (wanted, file) = (connection.clone(), id.clone());
            let opened = cx
                .background_executor()
                .spawn(async move { store.load(&wanted, &file).map(Opened::new) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.assistant.history.opening != generation {
                    return;
                }
                this.assistant.history.work = None;
                this.assistant_conversation_loaded(&connection, &id, epoch, opened, cx);
            });
        }));
        cx.notify();
    }

    fn assistant_conversation_loaded(
        &mut self,
        connection: &str,
        id: &str,
        epoch: u64,
        opened: Result<Opened, StoreError>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.assistant.chat().cloned() else {
            return;
        };
        // A chat that started running, or was replaced, while the file was read is left alone.
        let unchanged = {
            let chat = chat.read(cx);
            chat.epoch() == epoch && !chat.is_running() && !chat.is_asking()
        };
        if !unchanged {
            return;
        }
        let name = self.connection_name(connection).map(str::to_owned);
        let problem = match (opened, name) {
            (Ok(_), None) => Some("its connection no longer exists".to_owned()),
            (Ok(opened), Some(name)) => {
                chat.update(cx, |chat, cx| chat.open_stored(opened, name, cx));
                self.assistant_sync(cx);
                None
            }
            // Deleted under the list (another window, a delete in flight): the list is read again.
            (Err(StoreError::Missing), _) => {
                self.assistant.history.note =
                    Some("That conversation is not there any more.".to_owned());
                self.assistant_refresh_history(true, cx);
                None
            }
            (Err(error), _) => Some(error.to_string()),
        };
        if let Some(why) = problem {
            let history = &mut self.assistant.history;
            history
                .broken
                .insert((connection.to_owned(), id.to_owned()));
            history.rows = history
                .rows
                .iter()
                .filter(|row| row.id != id)
                .cloned()
                .collect();
            history.note =
                Some("That conversation could not be opened and is left out.".to_owned());
            if history
                .reported
                .insert(format!("{connection}/{id}.json:open"))
            {
                self.report_warning(
                    Area::App,
                    Some(connection),
                    format!(
                        "Assistant history: could not open the saved conversation {id}.json ({why})"
                    ),
                );
            }
        }
        self.assistant_push_context(cx);
        cx.notify();
    }

    /// The live chat stops being kept under a conversation that was deleted.
    fn assistant_detach_chat(
        &mut self,
        connection: Option<&str>,
        id: Option<&str>,
        why: &str,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.assistant.chat().cloned() else {
            return;
        };
        let (chat_connection, chat_id) = {
            let chat = chat.read(cx);
            (
                chat.scope().map(|scope| scope.connection_id.clone()),
                chat.conversation_id().map(str::to_owned),
            )
        };
        let same_connection = connection.is_none_or(|wanted| {
            chat_connection
                .as_deref()
                .is_some_and(|own| own.eq_ignore_ascii_case(wanted))
        });
        let same_id = id.is_none_or(|wanted| chat_id.as_deref() == Some(wanted));
        if same_connection && same_id {
            chat.update(cx, |chat, cx| chat.detach(why, cx));
        }
    }

    /// Deletes one stored conversation (off the UI thread, not cancelled by the next click) and
    /// reads the list again. If it is the chat's own, the chat stops being kept under it.
    pub(in crate::app) fn assistant_delete_conversation(
        &mut self,
        connection: String,
        id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.assistant.store.clone() else {
            return;
        };
        self.assistant.history.note = None;
        self.assistant_detach_chat(
            Some(&connection),
            Some(&id),
            "The saved conversation was deleted. This chat will be saved again, as a new conversation, if you continue it.",
            cx,
        );
        cx.spawn(async move |this, cx| {
            let wanted = connection.clone();
            let deleted = cx
                .background_executor()
                .spawn(async move { store.delete(&wanted, &id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = deleted {
                    this.report_error(
                        Area::App,
                        Some(&connection),
                        format!("Assistant history: could not delete the conversation ({error})"),
                    );
                }
                this.assistant_refresh_history(true, cx);
            });
        })
        .detach();
    }

    /// Deletes everything stored for a connection ("Delete all for this connection").
    pub(in crate::app) fn assistant_delete_connection_conversations(
        &mut self,
        connection: String,
        cx: &mut Context<Self>,
    ) {
        let Some(store) = self.assistant.store.clone() else {
            return;
        };
        self.assistant.history.note = None;
        self.assistant_detach_chat(
            Some(&connection),
            None,
            "The saved conversations of this connection were deleted. This chat will be saved again, as a new conversation, if you continue it.",
            cx,
        );
        self.assistant_delete_connection_work(store, connection, "the conversations", cx);
    }

    /// A connection was deleted: so are its conversations, and the chat about it is no longer
    /// kept (off the UI thread, nothing to show).
    pub(in crate::app) fn assistant_forget_connection(
        &mut self,
        connection: &str,
        cx: &mut Context<Self>,
    ) {
        // An empty chat pinned to the deleted connection has nothing left to be pinned to: the
        // active tab pulls it again.
        if let Some(chat) = self.assistant.chat().cloned() {
            let about_it = chat
                .read(cx)
                .scope()
                .is_some_and(|scope| scope.connection_id.eq_ignore_ascii_case(connection));
            if about_it {
                chat.update(cx, |chat, _| chat.unpin_if_empty());
            }
        }
        let store = self.assistant.store.clone();
        if store.is_some() {
            self.assistant_detach_chat(
                Some(connection),
                None,
                "The connection was deleted, and so were its saved conversations. This chat is not kept any more.",
                cx,
            );
        }
        // The chat's `keeping` follows the connection: it no longer exists.
        self.assistant_sync(cx);
        if let Some(store) = store {
            self.assistant_delete_connection_work(
                store,
                connection.to_owned(),
                "the conversations of the deleted connection",
                cx,
            );
        }
    }

    fn assistant_delete_connection_work(
        &mut self,
        store: Arc<ConversationStore>,
        connection: String,
        what: &'static str,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let wanted = connection.clone();
            let deleted = cx
                .background_executor()
                .spawn(async move { store.delete_connection(&wanted) })
                .await;
            let _ = this.update(cx, |this, cx| {
                let problem = match deleted {
                    Ok(report) => report.error,
                    Err(error) => Some(error),
                };
                if let Some(error) = problem {
                    this.report_warning(
                        Area::App,
                        Some(&connection),
                        format!("Assistant history: could not delete {what} ({error})"),
                    );
                }
                this.assistant_refresh_history(true, cx);
            });
        })
        .detach();
    }

    /// Settings: "Delete all conversations" (the user has confirmed). Off the UI thread; the
    /// result line says how many went. The live chat stops being kept under its file.
    pub(in crate::app) fn assistant_delete_all_conversations(&mut self, cx: &mut Context<Self>) {
        let Some(store) = self.assistant.store.clone() else {
            self.assistant.delete_all.result = Some(Ok(DeleteReport::default()));
            cx.notify();
            return;
        };
        if self.assistant.delete_all.busy {
            return;
        }
        self.assistant.delete_all.busy = true;
        self.assistant.delete_all.result = None;
        self.assistant_detach_chat(
            None,
            None,
            "All saved conversations were deleted. This chat will be saved again, as a new conversation, if you continue it.",
            cx,
        );
        self.assistant.delete_all.task = Some(cx.spawn(async move |this, cx| {
            let deleted = cx
                .background_executor()
                .spawn(async move { store.delete_all() })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.assistant.delete_all.busy = false;
                this.assistant.delete_all.task = None;
                let problem = match &deleted {
                    Ok(report) => report.error.as_ref().map(ToString::to_string),
                    Err(error) => Some(error.to_string()),
                };
                if let Some(error) = problem {
                    this.report_error(
                        Area::App,
                        None,
                        format!(
                            "Assistant history: could not delete all the conversations ({error})"
                        ),
                    );
                }
                this.assistant.delete_all.result = Some(deleted);
                this.assistant_refresh_history(true, cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The store of the app, if it keeps conversations at all (a scenario or a test has none
    /// unless it is given one).
    pub(in crate::app) fn assistant_store(&self) -> Option<&Arc<ConversationStore>> {
        self.assistant.store.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_handed_over_only_for_the_connection_they_were_listed_for() {
        let row = |id: &str| HistoryRow {
            id: id.to_owned(),
            title: id.to_owned().into(),
            updated: 1,
        };
        let history = History {
            connection: Some("AAA".into()),
            rows: Arc::from([row("c0"), row("c1")]),
            ..Default::default()
        };
        let (rows, listed_for) = history.rows_for(Some("aaa"));
        assert_eq!((rows.len(), &*listed_for), (2, "AAA"));
        // The chat is about another connection, or none: no row, and no connection to act on.
        for chat in [Some("bbb"), None] {
            let (rows, listed_for) = history.rows_for(chat);
            assert!(rows.is_empty() && listed_for.is_empty(), "{chat:?}");
        }
    }

    #[test]
    fn a_time_is_worded_by_how_long_ago_it_was() {
        let now = 1_760_000_000_000_u64;
        let ago = |seconds: u64| when(now - seconds * 1000, now);
        assert_eq!(ago(5), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(60), "1 min ago");
        assert_eq!(ago(59 * 60), "59 min ago");
        assert_eq!(ago(3600), "1 h ago");
        assert_eq!(ago(23 * 3600), "23 h ago");
        assert_eq!(ago(24 * 3600), "yesterday");
        assert_eq!(ago(3 * 86_400), "3 days ago");
        assert!(ago(30 * 86_400).ends_with("2025") || ago(30 * 86_400).ends_with("2026"));
        // The clock going backwards is not a panic.
        assert_eq!(when(now + 5_000, now), "just now");
    }
}
