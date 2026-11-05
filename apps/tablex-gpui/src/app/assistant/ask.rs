//! Actions that hand the assistant something to work on: "Explain", "Fix" and "Optimise" on the
//! SQL editor (its context menu, the command palette), and "Ask the assistant about this table" in
//! the navigator.
//!
//! Each one opens the dock, finds the chat it belongs to and sends ONE composed user message: a
//! fixed instruction sentence, then the SQL in a fenced block (the fence is longer than any run of
//! backticks inside it, the text is cut at [`SQL_CHARS`] and says so), and for Fix the error text
//! in a block of its own. The message is a user message like any other: it is shown as typed, not
//! rendered as Markdown, and the model reads the fences. The chat that is used is the current one
//! when it is about the same connection, database and schema; otherwise a new chat for the
//! action's connection starts (and says so, naming where the old one is). When the chat cannot
//! take a message now (no key, access off, a run going, a question waiting) the text goes into the
//! composer and the panel says why it was not sent.
//!
//! Only SQL editors have these actions: a Redis console, a Mongo script or a table-data tab is not
//! SQL text for the model to explain, and has none.

use super::dock::is_sql_editor;
use super::model::{ChatItem, ChatNotice};
use super::session::{Readiness, SendOutcome};
use super::*;
use crate::app::{AssistantExplain, AssistantFix, AssistantOptimise, FormatSql};
use crate::navigator::NavigatorNodeId;
use crate::query::QueryStatus;
use gpui::SharedString;

/// The most SQL characters sent in one message.
pub const SQL_CHARS: usize = 20_000;
/// The most characters of an error text sent with Fix.
pub const ERROR_CHARS: usize = 4_000;

/// What the user asks the assistant to do with a statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAsk {
    Explain,
    Fix,
    Optimise,
}

impl EditorAsk {
    /// The fixed sentence the message opens with.
    pub fn instruction(self) -> &'static str {
        match self {
            Self::Explain => {
                "Explain what this SQL does, step by step, and point out anything in it that is \
                 unusual or risky."
            }
            Self::Fix => {
                "This SQL failed. Explain what is wrong and give a corrected version. The error \
                 the database returned follows the statement."
            }
            Self::Optimise => {
                "Suggest how to make this SQL faster or simpler without changing what it returns. \
                 Look at the tables it uses before you advise on indexes."
            }
        }
    }
}

/// The instruction for the table action.
pub const TABLE_INSTRUCTION: &str = "Tell me about this table or view: what its columns hold, how \
    it relates to other tables, and what to watch for when querying it. Look at its definition \
    first.";

/// `text` in a fenced block. The fence is one backtick longer than the longest run of backticks
/// in `text` (at least three), so nothing in the text can close it early; `info` is the language
/// tag after the opening fence.
pub fn fenced(info: &str, text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{info}\n{text}\n{fence}")
}

/// `text` cut at `max` characters, and whether it was cut.
fn cut(text: &str, max: usize) -> (&str, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (&text[..at], true),
        None => (text, false),
    }
}

/// Fix when the statement that failed is no longer known (only the error is): the message says
/// the error is from the last run.
pub const FIX_UNSURE_INSTRUCTION: &str =
    "This SQL may be what failed: the error below is from the \
    last run of this editor, and the SQL may have changed since. Explain what is wrong and give a \
    corrected version if it is this statement.";

/// Fix when the error text could not be included (the connection's secrets could not be had to
/// mask it with): the message says so and why instead of the text.
pub const FIX_WITHHELD_INSTRUCTION: &str =
    "This SQL may have failed in the last run of this editor, but the error text could not be \
    included (the reason follows the statement). Say what could be wrong with it and give a \
    corrected version if you can.";

/// The line that stands for the error text in a Fix message that has none.
const WITHHELD_LINE: &str = "The error text was left out: ";

/// Every instruction sentence a composed message can open with.
const INSTRUCTIONS: [&str; 6] = [
    "Explain what this SQL does, step by step, and point out anything in it that is unusual or \
     risky.",
    "This SQL failed. Explain what is wrong and give a corrected version. The error the database \
     returned follows the statement.",
    "Suggest how to make this SQL faster or simpler without changing what it returns. Look at the \
     tables it uses before you advise on indexes.",
    FIX_UNSURE_INSTRUCTION,
    FIX_WITHHELD_INSTRUCTION,
    TABLE_INSTRUCTION,
];

fn sql_cut_note() -> String {
    format!(
        "(The statement is longer than {SQL_CHARS} characters; only the first {SQL_CHARS} are \
         shown.)"
    )
}

fn error_cut_note() -> String {
    format!(
        "(The error text is longer than {ERROR_CHARS} characters; only the first {ERROR_CHARS} \
         are shown.)"
    )
}

/// The message of an editor action: `instruction`, the SQL in a fence (cut at [`SQL_CHARS`] with
/// a line saying so), and for Fix the error text in a fence of its own.
fn compose_with(instruction: &str, sql: &str, error: Option<&str>) -> String {
    let (shown, was_cut) = cut(sql, SQL_CHARS);
    let mut message = format!("{instruction}\n\n{}", fenced("sql", shown));
    if was_cut {
        message.push_str(&format!("\n\n{}", sql_cut_note()));
    }
    if let Some(error) = error {
        let (shown, was_cut) = cut(error, ERROR_CHARS);
        message.push_str(&format!("\n\nThe error:\n\n{}", fenced("", shown)));
        if was_cut {
            message.push_str(&format!("\n\n{}", error_cut_note()));
        }
    }
    message
}

/// The one message of an editor action: the instruction, the SQL in a fence (cut at
/// [`SQL_CHARS`] with a line saying so), and for Fix the error text in a fence of its own.
pub fn compose(ask: EditorAsk, sql: &str, error: Option<&str>) -> String {
    let error = error.filter(|_| ask == EditorAsk::Fix);
    compose_with(ask.instruction(), sql, error)
}

/// [`compose`] for Fix when the failed statement is not known: the SQL is what the editor holds.
pub fn compose_fix_unsure(sql: &str, error: &str) -> String {
    compose_with(FIX_UNSURE_INSTRUCTION, sql, Some(error))
}

/// [`compose`] for Fix when the error text must not go: the statement, and a line that says the
/// text was left out and `why` (cut to one short line; it holds no secret).
pub fn compose_fix_withheld(sql: &str, why: &str) -> String {
    let why =
        engine_agent::sanitize::clean(&why.split_whitespace().collect::<Vec<_>>().join(" "), 200);
    let mut message = compose_with(FIX_WITHHELD_INSTRUCTION, sql, None);
    message.push_str(&format!("\n\n{WITHHELD_LINE}{why}"));
    message
}

/// The message of the table action: the instruction and the table's name in a fence.
pub fn compose_table(qualified_name: &str) -> String {
    format!("{TABLE_INSTRUCTION}\n\n{}", fenced("", qualified_name))
}

/// A box of a composed message: what it holds, a word for what it is, and the note about a cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedBlock {
    pub label: &'static str,
    pub text: SharedString,
    pub note: Option<SharedString>,
}

/// A user message of exactly the shape the composers above make, taken apart to be drawn as its
/// instruction and plain boxes. The text sent to the model is the message, unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composed {
    pub instruction: SharedString,
    pub blocks: Vec<ComposedBlock>,
}

/// The text of a fenced block at the start of `text`, and what follows its closing fence. The
/// fence is a run of at least three backticks followed by `info` and a line break, and closes at
/// the first line that is that same run: the composers choose a run longer than any in the body.
fn take_fence<'a>(text: &'a str, info: &str) -> Option<(&'a str, &'a str)> {
    let ticks = text.chars().take_while(|c| *c == '`').count();
    if ticks < 3 {
        return None;
    }
    let body = text[ticks..].strip_prefix(info)?.strip_prefix('\n')?;
    let close = format!("\n{}", "`".repeat(ticks));
    let mut from = 0;
    loop {
        let at = from + body[from..].find(&close)?;
        let end = at + close.len();
        if !body[end..].starts_with('`') {
            return Some((&body[..at], &body[end..]));
        }
        from = at + 1;
    }
}

/// Takes `text` apart if it is exactly a composed message (see [`Composed`]); anything else,
/// including a message a person typed that is only close, is `None` and is shown as typed.
pub fn parse_composed(text: &str) -> Option<Composed> {
    let (instruction, rest) = INSTRUCTIONS.iter().find_map(|instruction| {
        let rest = text.strip_prefix(instruction)?.strip_prefix("\n\n")?;
        Some((*instruction, rest))
    })?;
    let mut blocks = Vec::new();
    let note_after = |rest: &mut &str, note: String, block: &mut ComposedBlock| {
        if let Some(after) = rest
            .strip_prefix("\n\n")
            .and_then(|after| after.strip_prefix(note.as_str()))
        {
            block.note = Some(note.into());
            *rest = after;
        }
    };
    let mut rest = rest;
    if instruction == TABLE_INSTRUCTION {
        let (name, after) = take_fence(rest, "")?;
        blocks.push(ComposedBlock {
            label: "Table",
            text: name.to_owned().into(),
            note: None,
        });
        rest = after;
    } else {
        let (sql, after) = take_fence(rest, "sql")?;
        let mut block = ComposedBlock {
            label: "SQL",
            text: sql.to_owned().into(),
            note: None,
        };
        rest = after;
        note_after(&mut rest, sql_cut_note(), &mut block);
        blocks.push(block);
        if instruction == FIX_WITHHELD_INSTRUCTION {
            let why = rest.strip_prefix("\n\n")?.strip_prefix(WITHHELD_LINE)?;
            if why.is_empty() || why.contains('\n') {
                return None;
            }
            blocks.push(ComposedBlock {
                label: "Error left out",
                text: why.to_owned().into(),
                note: None,
            });
            rest = "";
        }
        let fix =
            instruction == EditorAsk::Fix.instruction() || instruction == FIX_UNSURE_INSTRUCTION;
        if fix {
            let after = rest.strip_prefix("\n\nThe error:\n\n")?;
            let (error, after) = take_fence(after, "")?;
            let mut block = ComposedBlock {
                label: "Error",
                text: error.to_owned().into(),
                note: None,
            };
            rest = after;
            note_after(&mut rest, error_cut_note(), &mut block);
            blocks.push(block);
        }
    }
    rest.is_empty().then(|| Composed {
        instruction: instruction.to_owned().into(),
        blocks,
    })
}

/// Which editor actions the active tab offers right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditorAsks {
    /// Explain and Optimise: a SQL editor on a connection.
    pub explain: bool,
    /// Fix: and the tab's last run failed.
    pub fix: bool,
}

/// Which editing items of the menu are on: the component's own rules for its default menu (read
/// from the editor's capabilities): Cut needs a selection and an editor that takes changes, Copy a
/// selection that may be copied, Paste an editor that takes changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Editing {
    pub cut: bool,
    pub copy: bool,
    pub paste: bool,
}

/// One row of the SQL editor's right-click menu; `action: None` is a separator.
pub struct MenuRow {
    pub label: &'static str,
    pub action: Option<Box<dyn gpui::Action>>,
    pub disabled: bool,
}

/// The rows of the SQL editor's right-click menu. The component's own menu is replaced whole by a
/// custom one (there is no way to add to it), so the editing items are made again here with the
/// same actions and the same enablement rules; "Go to Definition" and "Show Code Actions" are
/// left out, because the SQL editor has neither a definition provider nor a code-action provider
/// and they would always be off. The labels are English: the component keeps its own (translated)
/// strings behind a crate-private backend and offers no way to take its default menu or its text
/// (see the plan). Then Format SQL, then the assistant's actions, where the tab offers them.
pub fn editor_menu_rows(editing: Editing, asks: EditorAsks) -> Vec<MenuRow> {
    use gpui_component::input::{Copy, Cut, Paste, SelectAll};
    let item = |label, action: Box<dyn gpui::Action>, disabled| MenuRow {
        label,
        action: Some(action),
        disabled,
    };
    let separator = || MenuRow {
        label: "",
        action: None,
        disabled: true,
    };
    let mut rows = vec![
        item("Cut", Box::new(Cut), !editing.cut),
        item("Copy", Box::new(Copy), !editing.copy),
        item("Paste", Box::new(Paste), !editing.paste),
        separator(),
        item("Select All", Box::new(SelectAll), false),
        separator(),
        item("Format SQL", Box::new(FormatSql), !editing.paste),
    ];
    if asks.explain {
        rows.push(separator());
        rows.push(item(
            "Assistant: Explain",
            Box::new(AssistantExplain),
            false,
        ));
        if asks.fix {
            rows.push(item("Assistant: Fix", Box::new(AssistantFix), false));
        }
        rows.push(item(
            "Assistant: Optimise",
            Box::new(AssistantOptimise),
            false,
        ));
    }
    rows
}

/// The native menu for `rows`.
pub fn native_menu(rows: Vec<MenuRow>) -> gpui_component::native_menu::NativeMenu {
    rows.into_iter().fold(
        gpui_component::native_menu::NativeMenu::new(),
        |menu, row| match row.action {
            Some(action) => menu.menu_with_disabled(row.label, row.disabled, action),
            None => menu.separator(),
        },
    )
}

/// Why a message was not sent, for the line above the composer.
fn why_not_sent(outcome: &SendOutcome, asking: bool) -> Option<String> {
    let why = match outcome {
        SendOutcome::Started | SendOutcome::Empty => return None,
        SendOutcome::Busy if asking => "a statement is waiting for your answer".to_owned(),
        SendOutcome::Busy => "an answer is still being written".to_owned(),
        SendOutcome::SetupFailed => "the assistant could not be set up".to_owned(),
        SendOutcome::NotReady(readiness) => match readiness {
            Readiness::Ready => return None,
            Readiness::NoConnection => "there is no connection to ask about".to_owned(),
            Readiness::AccessOff => "the assistant is off for this connection".to_owned(),
            Readiness::NoApiKey => "no API key is set".to_owned(),
            Readiness::NoModel => "no model is chosen".to_owned(),
            Readiness::CheckingKey => "the API key is still being checked".to_owned(),
            Readiness::KeyUnavailable(_) => "the API key could not be read".to_owned(),
            Readiness::AccessLowered | Readiness::ReadOnly => {
                "this chat cannot go on at the connection's present access level".to_owned()
            }
        },
    };
    Some(format!("Not sent: {why}. The message is in the box."))
}

/// A database's error text as it may go into a message: of the connection's secrets (exact
/// values), of URI credentials and of values behind secret-looking keys (what the agent layer
/// does to every error it hands the model), and of invisible and direction-changing characters.
pub fn mask_error(error: &str, secrets: &[engine_agent::SecretText]) -> String {
    let cleaned = engine_agent::sanitize::clean_multiline(error);
    engine_agent::redact::Masks::new(secrets).redact(&cleaned)
}

/// An error text as it may go into a message: masked, or left out with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorText {
    Masked(String),
    Withheld(String),
}

/// Masks `error` of the secrets of `connection` on a thread of its own (the lookup reads the
/// connection's record and the credential store: not for the UI thread, and not for the shared
/// background pool, where a locked keychain would hold a thread). When the secrets cannot be had
/// the text is withheld: a password the pattern masks do not know could be in it.
pub fn mask_off_thread(
    secrets: Arc<dyn SecretSource>,
    connection: String,
    error: String,
) -> smol::channel::Receiver<ErrorText> {
    let (done, answer) = smol::channel::bounded(1);
    let sent = done.clone();
    let started = std::thread::Builder::new()
        .name("assistant-mask".into())
        .spawn(move || {
            let text = match secrets.secrets(&connection) {
                Ok(secrets) => ErrorText::Masked(mask_error(&error, &secrets)),
                Err(why) => ErrorText::Withheld(why.to_string()),
            };
            let _ = sent.send_blocking(text);
        });
    if started.is_err() {
        let _ = done.try_send(ErrorText::Withheld(
            "no thread to read the credentials on".to_owned(),
        ));
    }
    answer
}

/// Whether a run that failed was a run of what the tab is on now: the same connection, database
/// and schema. A tab that was pointed elsewhere since keeps the failure on its status line, but
/// the statement and error are about a place it is no longer in.
fn run_is_here(run: &crate::query::RunContext, tab: &crate::workspace::Tab) -> bool {
    run.connection == tab.connection_id && run.database == tab.database && run.schema == tab.schema
}

impl AppRoot {
    /// What the active tab offers: Explain / Optimise on a SQL editor with a connection, Fix when
    /// the tab's last run failed and was a run of where the tab is now.
    pub(in crate::app) fn assistant_editor_asks(&self) -> EditorAsks {
        let Some(tab) = self.workspace.active_tab() else {
            return EditorAsks::default();
        };
        let explain = is_sql_editor(tab) && !tab.connection_id.is_empty();
        let fix = explain
            && self.workspace.runtime(tab.id).is_some_and(|runtime| {
                matches!(runtime.query.status(), QueryStatus::Failed(_))
                    && runtime
                        .query
                        .failed_run()
                        .is_none_or(|run| run_is_here(run, tab))
            });
        EditorAsks { explain, fix }
    }

    /// Explain / Fix / Optimise on the active SQL editor. Explain and Optimise go over the
    /// selection, else the whole text. Fix goes over the statement that FAILED (the tab keeps it
    /// with the error; the editor may hold something else by now), and its error text is masked
    /// of the connection's secrets and cleaned off the UI thread before it is sent.
    pub(in crate::app) fn assistant_editor_ask(
        &mut self,
        ask: EditorAsk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offered = self.assistant_editor_asks();
        let offered = match ask {
            EditorAsk::Fix => offered.fix,
            EditorAsk::Explain | EditorAsk::Optimise => offered.explain,
        };
        let Some(tab) = self.workspace.active_tab().filter(|_| offered).cloned() else {
            return;
        };
        let (scope, label) = self.assistant_active_scope();
        let Some(scope) = scope else {
            return;
        };
        if ask == EditorAsk::Fix {
            let (failed, error) = self
                .workspace
                .runtime(tab.id)
                .map(|runtime| {
                    let error = match runtime.query.status() {
                        QueryStatus::Failed(error) => error.clone(),
                        _ => String::new(),
                    };
                    (runtime.query.failed_run().map(|run| run.sql.clone()), error)
                })
                .unwrap_or_default();
            let known = failed.is_some();
            let sql =
                failed.unwrap_or_else(|| self.editor_text_for_assistant(tab.id, &tab.sql, cx));
            if sql.trim().is_empty() {
                window.push_notification("The editor is empty: there is nothing to ask about", cx);
                return;
            }
            let masked = mask_off_thread(
                Arc::clone(&self.assistant.secrets),
                tab.connection_id.clone(),
                error,
            );
            cx.spawn_in(window, async move |this, cx| {
                // A lookup that died says nothing about the text: it is withheld, like any other
                // failure to read the secrets.
                let masked = masked
                    .recv()
                    .await
                    .unwrap_or_else(|_| ErrorText::Withheld("the lookup failed".into()));
                let message = match masked {
                    ErrorText::Masked(text) if known => compose(EditorAsk::Fix, &sql, Some(&text)),
                    ErrorText::Masked(text) => compose_fix_unsure(&sql, &text),
                    ErrorText::Withheld(why) => compose_fix_withheld(&sql, &why),
                };
                let _ = this.update_in(cx, |this, window, cx| {
                    this.assistant_ask(scope, label, message, window, cx)
                });
            })
            .detach();
            return;
        }
        let sql = self.editor_text_for_assistant(tab.id, &tab.sql, cx);
        if sql.trim().is_empty() {
            window.push_notification("The editor is empty: there is nothing to ask about", cx);
            return;
        }
        self.assistant_ask(scope, label, compose(ask, &sql, None), window, cx);
    }

    /// The selection of the tab's editor, else all of its text (the tab's own copy when the
    /// editor has not been made yet).
    fn editor_text_for_assistant(&self, tab_id: u64, tab_sql: &str, cx: &gpui::App) -> String {
        let Some(editor) = self.query_editors.by_tab.get(&tab_id) else {
            return tab_sql.to_owned();
        };
        let editor = editor.read(cx);
        let text = editor.value();
        let selected = editor.selected_range();
        if selected.is_empty() {
            return text.to_string();
        }
        text.get(selected.start..selected.end)
            .map_or_else(|| text.to_string(), str::to_owned)
    }

    /// "Ask the assistant about this table" on a navigator row of a table or view.
    pub(in crate::app) fn assistant_table_ask(
        &mut self,
        node: &NavigatorNodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let NavigatorNodeId::Object {
            connection,
            database,
            schema,
            object,
        } = node
        else {
            return;
        };
        let non_empty = |text: &String| (!text.is_empty()).then(|| text.clone());
        let scope = engine_ai::Scope {
            connection_id: connection.clone(),
            database: non_empty(database),
            schema: non_empty(schema),
        };
        let label = self
            .connection_name(connection)
            .map(str::to_owned)
            .unwrap_or_default();
        let qualified = if schema.is_empty() {
            object.clone()
        } else {
            format!("{schema}.{object}")
        };
        self.assistant_ask(scope, label, compose_table(&qualified), window, cx);
    }

    /// Whether the chat cannot be replaced by one for `scope` now: it is working (or a statement
    /// waits) and about something else.
    fn assistant_chat_blocks(&self, scope: &engine_ai::Scope, cx: &gpui::App) -> bool {
        self.assistant.chat().is_some_and(|chat| {
            let chat = chat.read(cx);
            chat.scope() != Some(scope) && (chat.is_running() || chat.is_asking())
        })
    }

    /// Opens the dock, finds the chat for `scope` and sends `message`, or leaves it in the
    /// composer with the reason it was not sent (and, when the chat is busy with something else,
    /// where Send will take it).
    fn assistant_ask(
        &mut self,
        scope: engine_ai::Scope,
        label: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.assistant.dock.open {
            self.open_assistant(window, cx);
        }
        let panel = self.assistant_panel(window, cx);
        self.assistant_chat(cx);
        if self.assistant_chat_blocks(&scope, cx) {
            // A chat that is answering is not replaced; the message waits for the user, with its
            // scope: Send starts the chat the action meant.
            let why = "Not sent: the current chat is still working and is about another \
                       connection. The message is in the box.";
            panel.update(cx, |panel, cx| {
                panel.hold(message, why.to_owned(), Some((scope, label)), window, cx)
            });
            return;
        }
        let outcome = self.assistant_deliver(scope, label, message.clone(), cx);
        let asking = self
            .assistant
            .chat()
            .is_some_and(|chat| chat.read(cx).is_asking());
        match why_not_sent(&outcome, asking) {
            Some(why) => panel.update(cx, |panel, cx| panel.hold(message, why, None, window, cx)),
            None => panel.update(cx, |panel, cx| panel.sent(cx)),
        }
    }

    /// Sends `message` in the chat when it is about `scope`, else in a new chat pinned to
    /// `scope`, which says where the old one went. A chat that is working on something else is
    /// not replaced (`Busy`, nothing changed). The path of an action and of the Send of a message
    /// an action had to hold.
    pub(in crate::app) fn assistant_deliver(
        &mut self,
        scope: engine_ai::Scope,
        label: String,
        message: String,
        cx: &mut Context<Self>,
    ) -> SendOutcome {
        let chat = self.assistant_chat(cx);
        if self.assistant_chat_blocks(&scope, cx) {
            return SendOutcome::Busy;
        }
        let (same, had_messages) = {
            let chat = chat.read(cx);
            (chat.scope() == Some(&scope), !chat.is_empty())
        };
        if same {
            // An empty chat that already is about this scope stays so: the next sync of the active
            // tab does not move it.
            chat.update(cx, |chat, _| chat.pin());
        } else {
            // What the chat was, before it is replaced.
            let (old_label, old_kept) = {
                let chat = chat.read(cx);
                (chat.scope_label().to_owned(), chat.is_kept())
            };
            // The scope is the action's, not the active tab's: the chat stays pinned to it.
            chat.update(cx, |chat, cx| {
                chat.new_chat_pinned(Some(scope), label.clone(), cx)
            });
            self.assistant_sync(cx);
            if had_messages {
                let old = if old_label.is_empty() {
                    "its connection".to_owned()
                } else {
                    old_label
                };
                let text = format!(
                    "New chat for {}. The previous chat, about {old}, {}.",
                    if label.is_empty() {
                        "this connection"
                    } else {
                        &label
                    },
                    if old_kept {
                        format!("is in the History of {old}")
                    } else {
                        "was not kept".to_owned()
                    }
                );
                chat.update(cx, |chat, cx| {
                    chat.add_item(ChatItem::Notice(ChatNotice::Other(text)), cx)
                });
            }
        }
        self.assistant_send(message, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fix_message_without_the_error_text_says_why_and_is_taken_apart_again() {
        let message = compose_fix_withheld(
            "SELECT 1",
            "the credential store could not be read\n(locked)  ",
        );
        assert!(message.starts_with(FIX_WITHHELD_INSTRUCTION));
        assert!(message.ends_with(
            "The error text was left out: the credential store could not be read (locked)"
        ));
        let parsed = parse_composed(&message).expect("a composed message");
        assert_eq!(parsed.blocks.len(), 2);
        assert_eq!(
            (parsed.blocks[0].label, parsed.blocks[0].text.as_ref()),
            ("SQL", "SELECT 1")
        );
        assert_eq!(parsed.blocks[1].label, "Error left out");
        assert_eq!(
            parsed.blocks[1].text.as_ref(),
            "the credential store could not be read (locked)"
        );
        // Anything after the reason, or no reason, is not this shape.
        assert!(parse_composed(&format!("{message}\nmore")).is_none());
        let none = format!(
            "{}\n\n{}",
            compose_with(FIX_WITHHELD_INSTRUCTION, "x", None),
            WITHHELD_LINE
        );
        assert!(parse_composed(&none).is_none());
    }

    #[test]
    fn the_message_is_the_instruction_then_the_sql_in_a_fence() {
        let message = compose(EditorAsk::Explain, "SELECT 1", None);
        assert_eq!(
            message,
            format!(
                "{}\n\n```sql\nSELECT 1\n```",
                EditorAsk::Explain.instruction()
            )
        );
        // The instructions are fixed sentences, one per action.
        let all = [EditorAsk::Explain, EditorAsk::Fix, EditorAsk::Optimise];
        for (ix, ask) in all.iter().enumerate() {
            for other in &all[ix + 1..] {
                assert_ne!(ask.instruction(), other.instruction());
            }
            assert!(ask.instruction().ends_with('.'));
        }
    }

    #[test]
    fn the_fence_is_longer_than_any_run_of_backticks_inside() {
        assert_eq!(fenced("sql", "SELECT 1"), "```sql\nSELECT 1\n```");
        let text = "SELECT '```'; -- ````\n```\nx";
        let fence = fenced("sql", text);
        assert!(fence.starts_with("`````sql\n"), "{fence}");
        assert!(fence.ends_with("\n`````"));
        // Whatever the text holds, no line of it is a fence that closes the block.
        for line in text.lines() {
            assert!(!line.trim_start().starts_with("`````"));
        }
        let lone = fenced("", &"`".repeat(7));
        assert!(lone.starts_with(&"`".repeat(8)));
    }

    #[test]
    fn fix_adds_the_error_in_a_block_of_its_own_and_only_fix() {
        let fixed = compose(EditorAsk::Fix, "SELEC 1", Some("syntax error at \"SELEC\""));
        assert!(fixed
            .contains("```sql\nSELEC 1\n```\n\nThe error:\n\n```\nsyntax error at \"SELEC\"\n```"));
        let explained = compose(EditorAsk::Explain, "SELEC 1", Some("syntax error"));
        assert!(
            !explained.contains("syntax error"),
            "only Fix carries the error"
        );
        // An error with backticks gets a longer fence too.
        let tricky = compose(EditorAsk::Fix, "x", Some("column `a``b` unknown"));
        assert!(tricky.contains("\n```\ncolumn `a``b` unknown\n```"));
        let worse = compose(EditorAsk::Fix, "x", Some("see ``` here"));
        assert!(worse.contains("````\nsee ``` here\n````"));
    }

    #[test]
    fn long_sql_is_cut_at_twenty_thousand_characters_and_says_so() {
        let sql = "é".repeat(SQL_CHARS + 5);
        let message = compose(EditorAsk::Optimise, &sql, None);
        let kept = message.matches('é').count();
        assert_eq!(kept, SQL_CHARS, "cut on a character boundary at 20 000");
        assert!(message.contains("longer than 20000 characters"));
        // The note stands outside the block, after the closing fence.
        let (block, note) = message.rsplit_once("\n```\n").unwrap();
        assert!(!block.contains("longer than"));
        assert!(note.contains("longer than"));
        // Exactly 20 000 is not cut.
        let exact = compose(EditorAsk::Optimise, &"x".repeat(SQL_CHARS), None);
        assert!(!exact.contains("longer than"));
        // A long error is cut as well.
        let error = "e".repeat(ERROR_CHARS + 1);
        let message = compose(EditorAsk::Fix, "x", Some(&error));
        assert!(message.contains(&"e".repeat(ERROR_CHARS)));
        assert!(!message.contains(&"e".repeat(ERROR_CHARS + 1)));
        assert!(message.contains("only the first 4000 are shown"));
    }

    #[test]
    fn the_table_message_names_the_table_in_a_fence() {
        let message = compose_table("public.orders");
        assert_eq!(
            message,
            format!("{TABLE_INSTRUCTION}\n\n```\npublic.orders\n```")
        );
        assert!(compose_table("we`ird").contains("```\nwe`ird\n```"));
        assert!(compose_table("a```b").contains("````\na```b\n````"));
    }

    const NOTHING_SELECTED: Editing = Editing {
        cut: false,
        copy: false,
        paste: true,
    };
    const SELECTED: Editing = Editing {
        cut: true,
        copy: true,
        paste: true,
    };

    #[test]
    fn a_composed_message_is_taken_apart_into_its_instruction_and_plain_boxes() {
        // Backtick runs in the SQL, in the error and in a table name.
        let sql = "SELECT '```', '````' -- ``\n```\nFROM t";
        let error = "no column `x` near ``` here";
        let message = compose(EditorAsk::Fix, sql, Some(error));
        let parsed = parse_composed(&message).expect("the exact shape");
        assert_eq!(parsed.instruction.as_ref(), EditorAsk::Fix.instruction());
        let boxes: Vec<(&str, &str)> = parsed
            .blocks
            .iter()
            .map(|block| (block.label, block.text.as_ref()))
            .collect();
        assert_eq!(boxes, [("SQL", sql), ("Error", error)]);
        for ask in [EditorAsk::Explain, EditorAsk::Optimise] {
            let parsed = parse_composed(&compose(ask, sql, None)).unwrap();
            assert_eq!(parsed.blocks.len(), 1);
            assert_eq!(parsed.blocks[0].text.as_ref(), sql);
        }
        let unsure = parse_composed(&compose_fix_unsure(sql, error)).unwrap();
        assert_eq!(unsure.instruction.as_ref(), FIX_UNSURE_INSTRUCTION);
        assert_eq!(unsure.blocks.len(), 2);
        let table = parse_composed(&compose_table("we```ird.\"t\"")).unwrap();
        assert_eq!(table.blocks[0].label, "Table");
        assert_eq!(table.blocks[0].text.as_ref(), "we```ird.\"t\"");
        // The notes about a cut are part of the shape.
        let long = "x".repeat(SQL_CHARS + 10);
        let cut = parse_composed(&compose(EditorAsk::Explain, &long, None)).unwrap();
        assert_eq!(cut.blocks[0].text.chars().count(), SQL_CHARS);
        assert!(cut.blocks[0].note.as_ref().unwrap().contains("longer than"));
        let cut = parse_composed(&compose(
            EditorAsk::Fix,
            "x",
            Some(&"e".repeat(ERROR_CHARS + 1)),
        ))
        .unwrap();
        assert!(cut.blocks[1].note.is_some());
    }

    #[test]
    fn anything_that_is_not_exactly_the_shape_is_shown_as_typed() {
        let good = compose(EditorAsk::Explain, "SELECT 1", None);
        assert!(parse_composed(&good).is_some());
        for (what, text) in [
            ("nothing", String::new()),
            ("a person's own words", "explain this for me".to_owned()),
            ("text before", format!("Please. {good}")),
            ("text after", format!("{good}\n\nThanks")),
            (
                "another instruction",
                good.replace("Explain what", "Tell me what"),
            ),
            ("one blank line", good.replacen("\n\n", "\n", 1)),
            ("another language", good.replace("```sql", "```python")),
            ("a short fence", good.replace("```", "``")),
            ("no closing fence", good[..good.len() - 3].to_owned()),
            (
                "a closing fence of another length",
                format!("{}\n````", &good[..good.len() - 4]),
            ),
            (
                "an error block on Explain",
                format!("{good}\n\nThe error:\n\n```\nx\n```"),
            ),
            (
                "Fix without its error",
                compose(EditorAsk::Fix, "SELECT 1", None),
            ),
            (
                "an invented note",
                format!("{good}\n\n(The statement is longer than 5 characters.)"),
            ),
        ] {
            assert!(parse_composed(&text).is_none(), "{what}: {text:?}");
        }
    }

    #[test]
    fn an_error_text_is_masked_and_cleaned_before_it_goes_into_a_message() {
        let secrets = [engine_agent::SecretText::new("Hunter2-DISTINCT")];
        let error = "FATAL: password authentication failed (Hunter2-DISTINCT)\n\
                     postgres://app:Hunter2-DISTINCT@db/x api_key=sk-live-123456 hidden\u{202E}end\u{200B}";
        let masked = mask_error(error, &secrets);
        assert!(!masked.contains("Hunter2-DISTINCT"), "{masked}");
        assert!(!masked.contains("sk-live-123456"), "{masked}");
        assert!(
            !masked.contains('\u{202E}') && !masked.contains('\u{200B}'),
            "{masked:?}"
        );
        assert!(masked.contains("FATAL: password authentication failed"));
        assert!(masked.contains('\n'), "line breaks stay");
        // Without the connection's secrets the key forms and URIs are still masked.
        let without = mask_error(error, &[]);
        assert!(!without.contains("sk-live-123456"), "{without}");
    }

    #[test]
    fn the_editor_menu_keeps_the_editing_items_and_adds_the_assistants_where_offered() {
        let labels = |editing, asks| -> Vec<(&'static str, bool)> {
            editor_menu_rows(editing, asks)
                .iter()
                .filter(|row| row.action.is_some())
                .map(|row| (row.label, row.disabled))
                .collect()
        };
        // No connection: only the editing items.
        assert_eq!(
            labels(NOTHING_SELECTED, EditorAsks::default()),
            [
                ("Cut", true),
                ("Copy", true),
                ("Paste", false),
                ("Select All", false),
                ("Format SQL", false)
            ]
        );
        // A SQL editor on a connection: Explain and Optimise.
        let asks = EditorAsks {
            explain: true,
            fix: false,
        };
        let rows = labels(SELECTED, asks);
        assert_eq!(&rows[..2], [("Cut", false), ("Copy", false)]);
        assert_eq!(
            &rows[5..],
            [
                ("Assistant: Explain", false),
                ("Assistant: Optimise", false)
            ]
        );
        // Fix only after a failed run.
        let asks = EditorAsks {
            explain: true,
            fix: true,
        };
        let names: Vec<_> = labels(NOTHING_SELECTED, asks)
            .iter()
            .map(|(label, _)| *label)
            .collect();
        assert_eq!(
            &names[5..],
            [
                "Assistant: Explain",
                "Assistant: Fix",
                "Assistant: Optimise"
            ]
        );
        // The actions are the real ones.
        let rows = editor_menu_rows(NOTHING_SELECTED, asks);
        let fix = rows
            .iter()
            .find(|row| row.label == "Assistant: Fix")
            .unwrap();
        assert_eq!(fix.action.as_ref().unwrap().name(), "tablex::AssistantFix");
        // Fix without Explain is not a state the tab can be in; nothing is offered.
        assert_eq!(
            labels(
                NOTHING_SELECTED,
                EditorAsks {
                    explain: false,
                    fix: true
                }
            )
            .len(),
            5
        );
    }

    #[test]
    fn a_message_that_was_not_sent_says_why() {
        let says = |outcome: SendOutcome, asking: bool| why_not_sent(&outcome, asking);
        assert_eq!(says(SendOutcome::Started, false), None);
        assert!(says(SendOutcome::Busy, true)
            .unwrap()
            .contains("waiting for your answer"));
        assert!(says(SendOutcome::Busy, false)
            .unwrap()
            .contains("still being written"));
        assert!(says(SendOutcome::NotReady(Readiness::NoApiKey), false)
            .unwrap()
            .contains("no API key"));
        assert!(says(SendOutcome::NotReady(Readiness::AccessOff), false)
            .unwrap()
            .contains("off for this connection"));
        assert!(says(SendOutcome::NotReady(Readiness::ReadOnly), false)
            .unwrap()
            .contains("cannot go on"));
    }
}
