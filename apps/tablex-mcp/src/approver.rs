//! The write approver of `tablex-mcp`: a statement that changes data is put to the user through MCP
//! **elicitation** (`elicitation/create`), and runs only when the client answers `accept` with the
//! choice [`ANSWER_PROPERTY`] set to [`ANSWER_RUN`] while the app's settings still allow it.
//!
//! **The answer is a required single choice with no default** ("Run this statement" or "Do not run
//! it"), not a checkbox. A client that fills in defaults, or whose Accept / Submit button sends the
//! form as it stands, has no default to fill in: the answer is missing, and a missing answer never
//! runs anything. Only `accept` with exactly [`ANSWER_RUN`] approves; every other answer is a
//! reason of its own the agent can tell the user: a decline, a dismissal, the choice "Do not run
//! it", an accept with no choice ([`no_choice`]), and an unusable value ([`REASON_NO_ANSWER`]).
//!
//! The client is the user's own agent host and may answer without showing the question; tablex
//! cannot see that. So conditions must all hold before anything is asked, checked in this order,
//! each a [`ApprovalDecision::Refuse`] with a fixed reason the model can act on:
//! 1. the app's `settings.json` allows asking ([`McpWrites`]: read at the time of each write, and
//!    **again when the client says yes**). There is no switch for it in the app any more (every
//!    write is asked about, per statement); only a file written by an older app (schema below 7)
//!    still carries "Let MCP clients run changes", and `false` there blocks writes. A missing or
//!    unreadable file blocks them too;
//! 2. the connection is not of a production type;
//! 3. the client declared the elicitation capability (form mode), or else the tablex app is running
//!    and reachable on its local channel (below);
//! 4. the statement is small enough to be reviewed, as shown ([`MAX_LINES`], [`MAX_CHARS`],
//!    [`MAX_LINE_CHARS`]);
//! 5. the connection's declines plus its open questions stay under [`MAX_DECLINES`] (a decline is
//!    a no, a dismissal, no usable answer, or the call cancelled after the question was sent;
//!    counted for this server session), and fewer than [`MAX_OPEN_QUESTIONS`] questions are open in all.
//!
//! Dangerous statements (`DROP`, `TRUNCATE`, a `DELETE` without `WHERE`, ...) are asked about like
//! any write, with the danger and its reason stated first, but only on a connection that is not of
//! a production type (`allows_dangerous`: the layer refuses them without asking on a production
//! one); the approver refuses one on a production connection itself as well.
//!
//! The same question serves every write: an SQL statement or a Redis command, a MongoDB command
//! (database, collection and the command as indented Extended JSON in the code block) and a script
//! of SQL statements (`execute_batch`: every statement that changes data, listed in order, accepted
//! or declined together).
//!
//! **Approval in the app.** A client that cannot ask gets its question put to the user in the tablex
//! app instead ([`AppClient::ask_approval`]: a modal dialog with the same facts). Elicitation is
//! preferred when the client has it; with neither, the write is refused and the reason says what the
//! user can do. The app is a trust boundary here. The boundary itself is the socket (owned by this
//! user, private, in a folder only this user can write to, with a peer of this user); on top of it a
//! per-launch token file is the key of an HMAC challenge-response, so the token is never sent, a
//! question is signed over what is shown, and the answer is signed over the same question, a nonce
//! and the choice (see `engine_connect::app_channel`). Every other condition above applies
//! unchanged. An app
//! that closes, or a channel that drops, while the question is open is "not delivered"; a cancelled
//! call closes the connection, which withdraws the question in the app.
//!
//! **Refusals.** Every refusal tells the agent what to tell the user (the setting by the name the
//! app shows, a client that supports MCP elicitation, the tablex app) and ends with
//! [`OUTSIDE_TABLEX`]: an agent that is refused must not go around tablex's access levels,
//! approvals and audit by connecting to the database itself. [`refusal`] adds it, in the one place a
//! refusal leaves this module.
//!
//! **Threads.** [`WriteApprover::approve`] blocks the call's own blocking thread. The question is
//! sent by a task spawned on the server's runtime (so the protocol thread is never blocked and a
//! current-thread runtime cannot deadlock); the blocking thread waits for that task's answer on a
//! channel. The wait ends in these ways and no other: the client answers, the connection to it
//! ends (end of input, a signal: the server stops writing, so the client is not told), or the
//! call's [`CancelToken`] fires. When the request itself is cancelled the client is told with
//! `notifications/cancelled` for the question. There is no timeout that approves; an unanswered
//! question holds one call permit and one blocking thread until one of those ends it, which is why
//! at most [`MAX_OPEN_QUESTIONS`] may be open.
//!
//! **Which peer.** The layer's approver gets only the request and the call's token, so the server
//! hands the call's client to the approver through a thread-local set around the call
//! ([`Caller::enter`]); a call from a thread that has none is refused.
//!
//! **Markdown.** A client may render the message as Markdown. The statement therefore goes in a
//! fenced code block whose fence is longer than any run of backticks in it, and the names are
//! code spans, so what is shown is what runs (up to the documented `<U+XXXX>` spelling); a plain-text
//! client shows two extra lines.

use std::cell::RefCell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard};

use engine_agent::sanitize::{is_right_to_left, spell_out};
use engine_agent::{
    ApprovalDecision, CancelToken, ConnectionInfo, Language, TransactionApproval, WriteApproval,
    WriteApprover,
};
use engine_connect::app_channel::{new_request_id, AppClient, ApprovalAsk, ClientError};
use engine_connect::profile::{mcp_writes_state, McpWrites};
use rmcp::model::{
    ClientResult, ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationSchema, EnumSchema, LegacyEnumSchema, ServerRequest,
};
use rmcp::service::{ElicitationMode, Peer, PeerRequestOptions, RoleServer};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// The reason when the question was accepted with no choice made (a client's Accept or Submit
/// button on a form left as it was). It counts as a decline ([`MAX_DECLINES`]), and says so.
pub fn no_choice(title: &str) -> String {
    format!(
        "the question was accepted without choosing \"{title}\" or \"{}\", so nothing ran. Ask the \
         user whether to go ahead, and tell them to choose \"{title}\" and accept. An answer without a \
         choice counts as a decline: after {MAX_DECLINES} declines or unchosen answers on a connection, \
         changes to it through MCP are off until the MCP server is restarted",
        no_title(title)
    )
}

/// The reason when the user chose the option that does not run it.
pub fn chose_not_to(title: &str) -> String {
    format!("the user chose \"{}\"", no_title(title))
}

/// The option that says no, for the question titled `title`.
pub fn no_title(title: &str) -> &'static str {
    match title {
        COMMIT_TITLE => COMMIT_NO_TITLE,
        CONNECTION_TITLE => CONNECTION_NO_TITLE,
        _ => ANSWER_NO_TITLE,
    }
}

/// The longest client name shown (spelled out), in characters.
const MAX_CLIENT_NAME: usize = 80;

/// `spelled` cut to `max` characters, never inside a `<U+XXXX>` unit, with an ellipsis when it was
/// cut.
fn cut_spelled(spelled: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut rest = spelled;
    while let Some(next) = rest.chars().next() {
        // One unit: a spelled character, or one character.
        let unit = if rest.starts_with("<U+") {
            rest.find('>').map_or(next.len_utf8(), |end| end + 1)
        } else {
            next.len_utf8()
        };
        let length = rest[..unit].chars().count();
        if count + length > max {
            out.push('\u{2026}');
            return out;
        }
        out.push_str(&rest[..unit]);
        count += length;
        rest = &rest[unit..];
    }
    out
}

/// The sentence every refusal ends with (see [`refusal`]).
pub const OUTSIDE_TABLEX: &str = "Do not connect to the database outside tablex (for example with \
     the URI or a script); ask the user instead.";

/// A refusal as the agent receives it: `reason`, then [`OUTSIDE_TABLEX`]. The one place it is added.
pub fn refusal(reason: &str) -> String {
    format!("{}. {OUTSIDE_TABLEX}", reason.trim_end_matches('.'))
}

/// Refusal: the settings were written by an older tablex app whose switch "Let MCP clients run
/// changes" is off (a file of schema 7 or later has no switch), when asking or when the client said
/// yes. The setting is named as that app shows it.
pub const REASON_SWITCH_OFF: &str = "changes through the MCP server are turned off in tablex. Ask the \
     user to turn on \"Let MCP clients run changes\" in the tablex app (Settings \u{2192} AI Assistant \
     \u{2192} MCP server), or to update tablex, which no longer has this switch";
/// Refusal: tablex's settings are missing or cannot be read, so nothing can be asked.
pub const REASON_SETTINGS_UNAVAILABLE: &str = "the tablex settings could not be read, so changes \
     through the MCP server are off. Ask the user to open the tablex app once, and to check that it \
     can write its settings";
/// Refusal: the connection's type is a production one.
pub const REASON_PRODUCTION: &str = "a production connection can only be changed from the tablex app, \
     never through MCP. Ask the user to make this change in the tablex app";
/// Refusal: the client cannot show questions (the start of the reason; what the user can do about
/// it follows, see [`no_way_to_ask`]).
pub const REASON_NO_ELICITATION: &str =
    "this MCP client cannot show approval questions (it does not support MCP elicitation)";
/// Refusal: [`MAX_DECLINES`] questions about the connection ended in a no or in no usable answer.
pub const REASON_DECLINED_OFTEN: &str =
    "declined, or left without a usable answer, several times in this server session; changes \
     to this connection through MCP are off until the MCP server is restarted. Ask the user to \
     restart it, or to make the change in the tablex app";
/// Refusal: too big to be reviewed in a question.
pub const REASON_TOO_LONG: &str = "too long to be shown for approval through MCP (more than 200 \
     lines, 8000 characters in all or 400 in one line). Ask the user to run it in the tablex app, or \
     send it in smaller parts";
/// Refusal: a line hides text behind a run of blanks.
pub const REASON_HIDDEN_BLANKS: &str = "a line has a run of more than 24 spaces or tabs (a tab counts \
     as 8) followed by more text, which a code block can hide off-screen. Ask the user to run it in \
     the tablex app, or send it without the blanks";
/// Refusal: the writes of a transaction cannot be reviewed in one commit question.
pub const REASON_COMMIT_NOT_REVIEWABLE: &str = "the changes in this transaction are too long to be \
     shown for approval through MCP (more than 200 lines, 8000 characters in all or 400 in one line, \
     or a line hides text behind a run of blanks). Roll the transaction back, and ask the user to run \
     the changes in the tablex app";
/// Refusal: [`MAX_OPEN_QUESTIONS`] questions are open.
pub const REASON_BUSY: &str =
    "other changes are waiting for the user's answer. Wait until they are answered, then try again";
/// Refusal: the layer should never send a dangerous statement for a production connection here.
pub const REASON_DANGEROUS: &str = "a dangerous statement is never run through MCP on a production \
     connection. Ask the user to run it in the tablex app";
/// Refusal: the question did not reach the user.
pub const REASON_NOT_DELIVERED: &str = "the question could not be delivered: the MCP client or the \
     tablex app closed, or the connection to it dropped. Ask the user to check that it is open, and \
     try again";
/// Not approved: the question was sent but nothing usable came back.
pub const REASON_NO_ANSWER: &str = "no usable answer came back from the MCP client";
/// The reason in the audit line and for the model when the user dismissed the question.
pub const REASON_DISMISSED: &str = "the question was dismissed";
/// The reason when the call was cancelled while the question was open (the layer turns a decision
/// under a cancelled token into its own `Cancelled`).
const REASON_CANCELLED: &str = "cancelled";

/// The one property of the requested schema: a required choice with no default.
pub const ANSWER_PROPERTY: &str = "answer";
/// Its value that runs it (the only one that does).
pub const ANSWER_RUN: &str = "run";
/// Its other value.
pub const ANSWER_DO_NOT_RUN: &str = "do_not_run";
/// The option that runs it, as the client shows it (and the title of the question).
pub const ANSWER_TITLE: &str = "Run this statement";
/// The option that does not.
pub const ANSWER_NO_TITLE: &str = "Do not run it";
/// The title when the question is the COMMIT of a transaction.
pub const COMMIT_TITLE: &str = engine_connect::app_channel::COMMIT_TITLE;
pub const COMMIT_NO_TITLE: &str = "Do not commit it";
/// The title when the question is a change to the saved connections (`add_connection`,
/// `duplicate_connection`, `remove_connection`).
pub const CONNECTION_TITLE: &str = "Make this change to your connections";
pub const CONNECTION_NO_TITLE: &str = "Do not make this change";
/// The question budget (declines, open questions) the connection tools share, in place of a
/// connection: three declines end them for the server session, like a connection's writes.
const CONNECTIONS_BUDGET: &str = "(saved connections)";

/// Declines of one connection after which it is closed to MCP writes for the rest of the session.
pub const MAX_DECLINES: u32 = 3;
/// Questions that may wait for an answer at once (each holds a call permit and a blocking thread).
pub const MAX_OPEN_QUESTIONS: usize = 4;
/// The most lines / characters of a statement that is put to the user.
pub const MAX_LINES: usize = 200;
pub const MAX_CHARS: usize = 8000;
/// The most characters in one line of it (a non-wrapping code block hides the rest).
pub const MAX_LINE_CHARS: usize = 400;
/// The widest run of blanks (a tab is [`TAB_COLUMNS`]) that may be followed by more text on a line.
pub const MAX_BLANK_RUN: usize = 24;
/// How wide a tab is for the limits above.
pub const TAB_COLUMNS: usize = 8;

/// What one connection has used of the question budget in this server session.
#[derive(Debug, Default)]
struct Budget {
    declines: u32,
    open: u32,
}

/// Asks the user through MCP elicitation. State: where the app's settings are, for each connection
/// how often it was declined and how many questions are open, and how many are open in all.
#[derive(Debug)]
pub struct ElicitingApprover {
    settings_path: Option<PathBuf>,
    /// The running app, which gets the question of a client that cannot ask (`None`: none known).
    app: Option<AppClient>,
    budgets: Mutex<HashMap<String, Budget>>,
    open: AtomicUsize,
}

/// One open question, counted (in all and on its connection) until dropped.
struct OpenQuestion<'a> {
    approver: &'a ElicitingApprover,
    connection: String,
}

impl Drop for OpenQuestion<'_> {
    fn drop(&mut self) {
        let mut budgets = self.approver.budgets();
        if let Some(budget) = budgets.get_mut(&self.connection) {
            budget.open = budget.open.saturating_sub(1);
        }
        self.approver.open.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ElicitingApprover {
    /// `settings_path`: the app's `settings.json` (`None`: unknown, so changes are not allowed).
    pub fn new(settings_path: Option<PathBuf>) -> Self {
        Self {
            settings_path,
            app: None,
            budgets: Mutex::default(),
            open: AtomicUsize::new(0),
        }
    }

    /// Puts the questions of a client that cannot ask to the user in the app behind `app`.
    #[must_use]
    pub fn with_app(mut self, app: Option<AppClient>) -> Self {
        self.app = app;
        self
    }

    fn switch_on(&self) -> bool {
        self.writes() == McpWrites::Allowed
    }

    fn writes(&self) -> McpWrites {
        mcp_writes_state(self.settings_path.as_deref())
    }

    /// The refusal text for settings that do not allow changes right now (see [`switch_off`]).
    fn not_allowed(&self, instead: Option<&str>) -> String {
        match self.writes() {
            McpWrites::SwitchOff => switch_off(instead),
            _ => settings_unavailable(instead),
        }
    }

    fn budgets(&self) -> MutexGuard<'_, HashMap<String, Budget>> {
        self.budgets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Takes a place for a question about `connection`, or says why not. The check and the count
    /// are one step under one lock, so questions that are open at once cannot get past the limit
    /// together: a new one is refused when the connection's declines plus its open questions
    /// would reach [`MAX_DECLINES`], or when [`MAX_OPEN_QUESTIONS`] are open in all.
    fn open_question(&self, connection: &str) -> Result<OpenQuestion<'_>, &'static str> {
        let mut budgets = self.budgets();
        let budget = budgets.entry(connection.to_owned()).or_default();
        if budget.declines >= MAX_DECLINES {
            return Err(REASON_DECLINED_OFTEN);
        }
        if budget.declines + budget.open >= MAX_DECLINES {
            return Err(REASON_BUSY);
        }
        if self.open.fetch_add(1, Ordering::SeqCst) >= MAX_OPEN_QUESTIONS {
            self.open.fetch_sub(1, Ordering::SeqCst);
            return Err(REASON_BUSY);
        }
        budget.open += 1;
        Ok(OpenQuestion {
            approver: self,
            connection: connection.to_owned(),
        })
    }

    fn record_decline(&self, connection: &str) {
        self.budgets()
            .entry(connection.to_owned())
            .or_default()
            .declines += 1;
    }

    /// Why `statement` cannot be reviewed as it is shown, if it cannot: too many lines or
    /// characters (a spelled character counts as the several it is written with), a line wider than
    /// [`MAX_LINE_CHARS`] columns (a tab is [`TAB_COLUMNS`]), or text pushed out of sight behind a
    /// run of more than [`MAX_BLANK_RUN`] blanks (indentation at the start of a line is fine).
    fn review_problem(statement: &str) -> Option<&'static str> {
        let (shown, _) = spell_out(statement, true);
        if shown.chars().count() > MAX_CHARS || shown.split('\n').count() > MAX_LINES {
            return Some(REASON_TOO_LONG);
        }
        // A blank is anything that draws as empty space: the tab and every Unicode space (no-break
        // spaces, the en/em family, the ideographic space, which is two columns wide), not only
        // U+0020. Zero-width characters cannot split a run: they are spelled out before this.
        let columns = |c: char| match c {
            '\t' => TAB_COLUMNS,
            '\u{3000}' => 2,
            _ => 1,
        };
        let is_blank = |c: char| c.is_whitespace();
        for line in shown.split('\n') {
            // The `\r` of a `\r\n` is the line's end, not text after its blanks.
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.chars().map(columns).sum::<usize>() > MAX_LINE_CHARS {
                return Some(REASON_TOO_LONG);
            }
            let mut run = 0;
            let mut seen_text = false;
            for c in line.chars() {
                if is_blank(c) {
                    run += columns(c);
                } else {
                    if seen_text && run > MAX_BLANK_RUN {
                        return Some(REASON_HIDDEN_BLANKS);
                    }
                    run = 0;
                    seen_text = true;
                }
            }
        }
        None
    }

    /// The text of the question for `request`. The statement is in a fenced code block that is the
    /// last thing in the message, its fence longer than any run of backticks in it, so a Markdown
    /// client shows it literally and nothing can end the block early; the names are code spans. Characters without a shape of their own are spelled `<U+XXXX>` (and the message
    /// says so), right-to-left letters are announced, and the size is stated in lines and
    /// characters.
    pub fn message(request: &WriteApproval<'_>) -> String {
        if let Some(TransactionApproval {
            id,
            commit: Some(statements),
        }) = request.transaction
        {
            return Self::commit_message(request, id, statements);
        }
        let noun = match (request.language, request.script) {
            (Language::Redis | Language::Mongo, _) => "command",
            (_, Some(_)) => "script",
            (Language::Sql | Language::Other, None) => "statement",
        };
        let label = |text: &str| code_span(&spell_out(text, false).0);
        let connection_type = if request.connection_type.trim().is_empty() {
            "No type".to_owned()
        } else {
            label(request.connection_type)
        };
        let (shown, spelled) = spell_out(request.statement, true);
        let lines = request.statement.split('\n').count();
        let characters = request.statement.chars().count();
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let fence = "`".repeat(longest_backtick_run(&shown).max(2) + 1);
        let mut message = match request.dangerous {
            Some(reason) => format!(
                "DANGEROUS: an MCP client asks to run a {noun} that can destroy or change data or \
                 structure that a rollback may not bring back. Nothing runs unless you accept. To run it, choose \
                 \"{ANSWER_TITLE}\" and accept.\n\n\
                 Why it is dangerous: {reason}\n\n\
                 Connection: {} ({connection_type})\n\n",
                label(request.connection_name),
            ),
            None => format!(
                "An MCP client asks to run a {noun} that changes data. Nothing runs unless you accept. To run it, \
                 choose \"{ANSWER_TITLE}\" and accept.\n\n\
                 Connection: {} ({connection_type})\n\n",
                label(request.connection_name),
            ),
        };
        if let Some(database) = request.database {
            message.push_str(&format!("Database: {}\n\n", label(database)));
        }
        if let Some(collection) = request.collection {
            message.push_str(&format!("Collection: {}\n\n", label(collection)));
        }
        if let Some(changes) = request.script {
            message.push_str(&format!(
                "The script is one question: the {changes} statement{} below that change{} data are \
                 listed in order, and the statements that only read are not shown. Accepting runs \
                 the whole script.\n\n",
                plural(changes),
                if changes == 1 { "s" } else { "" },
            ));
        }
        if let Some(transaction) = request.transaction {
            message.push_str(&format!(
                "The {noun} runs inside transaction {}. It is NOT committed until you approve the \
                 commit of the transaction; a rollback undoes it.\n\n",
                label(transaction.id)
            ));
        }
        message.push_str(&format!(
            "The {noun} has {lines} line{} and {characters} character{}.\n\n",
            plural(lines),
            plural(characters)
        ));
        if spelled > 0 {
            message.push_str(&format!(
                "Note: the {noun} has {spelled} character{} without a shape of its own (controls, \
                 direction changes, zero-width characters), written as <U+XXXX> below. The {noun} \
                 that runs has the original characters.\n\n",
                plural(spelled)
            ));
        }
        let names = [
            Some(request.connection_name),
            Some(request.connection_type),
            request.database,
            request.collection,
        ];
        if names
            .into_iter()
            .flatten()
            .any(|name| name.chars().any(is_right_to_left))
        {
            message.push_str(
                "Warning: a name above has right-to-left letters, which can change how the text \
                 around it is displayed. Read it with care.\n\n",
            );
        }
        if request.statement.chars().any(is_right_to_left) {
            message.push_str(&format!(
                "Warning: the {noun} has right-to-left letters, which can change how the text \
                 around them is displayed. Read it with care.\n\n"
            ));
        }
        message.push_str(&format!(
            "The {noun} is between the two lines of {} backticks below. Everything between them is \
             the {noun}; nothing follows the second line.\n\n{fence}\n{shown}\n{fence}",
            fence.len()
        ));
        message
    }

    /// The commit question: the connection, the transaction, and every write statement that ran
    /// in it, each in a code block of its own built like the statement of [`message`](Self::message)
    /// (a fence longer than any backtick run in it, characters without a shape spelled out). The
    /// text around the blocks is ours; a statement cannot end its block early, so it cannot add to it.
    fn commit_message(
        request: &WriteApproval<'_>,
        transaction: &str,
        statements: &[String],
    ) -> String {
        let label = |text: &str| code_span(&spell_out(text, false).0);
        let connection_type = if request.connection_type.trim().is_empty() {
            "No type".to_owned()
        } else {
            label(request.connection_type)
        };
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let total = statements.len();
        let mut message = format!(
            "An MCP client asks to COMMIT a transaction. The {total} change{} below ran inside it \
             and none is final yet: they become final only if you accept. Nothing is committed \
             unless you accept. To commit, choose \"{COMMIT_TITLE}\" and accept.\n\nConnection: {} ({connection_type})\n\nTransaction: {}\n\n",
            plural(total),
            label(request.connection_name),
            label(transaction),
        );
        let names = [Some(request.connection_name), Some(request.connection_type)];
        if names
            .into_iter()
            .flatten()
            .any(|name| name.chars().any(is_right_to_left))
        {
            message.push_str(
                "Warning: a name above has right-to-left letters, which can change how the text \
                 around it is displayed. Read it with care.\n\n",
            );
        }
        let mut spelled_total = 0;
        let mut right_to_left = false;
        for statement in statements {
            let (_, spelled) = spell_out(statement, true);
            spelled_total += spelled;
            right_to_left |= statement.chars().any(is_right_to_left);
        }
        if spelled_total > 0 {
            message.push_str(&format!(
                "Note: the changes have {spelled_total} character{} without a shape of their own \
                 (controls, direction changes, zero-width characters), written as <U+XXXX> below. \
                 The statements that ran have the original characters.\n\n",
                plural(spelled_total)
            ));
        }
        if right_to_left {
            message.push_str(
                "Warning: a change has right-to-left letters, which can change how the text \
                 around them is displayed. Read it with care.\n\n",
            );
        }
        for (index, statement) in statements.iter().enumerate() {
            let (shown, _) = spell_out(statement, true);
            let fence = "`".repeat(longest_backtick_run(&shown).max(2) + 1);
            message.push_str(&format!(
                "Change {} of {total}: exactly one statement is between the two lines of {} \
                 backticks below.\n\n{fence}\n{shown}\n{fence}\n\n",
                index + 1,
                fence.len(),
            ));
        }
        message.truncate(message.trim_end().len());
        message
    }

    /// The requested schema: one required single choice, [`ANSWER_PROPERTY`], of [`ANSWER_RUN`] and
    /// [`ANSWER_DO_NOT_RUN`] with titles and **no default** (a client that fills in defaults has
    /// nothing to fill in, and an answer without a choice does not run anything; the safe option is
    /// listed first, for a client that preselects the first one). The `enum` /
    /// `enumNames` form is the one every version of the elicitation schema accepts.
    fn schema(commit: bool) -> Option<ElicitationSchema> {
        Self::schema_titled(if commit { COMMIT_TITLE } else { ANSWER_TITLE })
    }

    fn schema_titled(title: &'static str) -> Option<ElicitationSchema> {
        // "Do not run" first: a client that preselects the first option preselects the safe one.
        let mut choice = LegacyEnumSchema::new(vec![ANSWER_DO_NOT_RUN.to_owned(), ANSWER_RUN.to_owned()]);
        choice.title = Some(format!("{title}?").into());
        choice.enum_names = Some(vec![no_title(title).to_owned(), title.to_owned()]);
        ElicitationSchema::builder()
            .required_enum_schema(ANSWER_PROPERTY, EnumSchema::Legacy(choice))
            .build()
            .ok()
    }

    /// The question ended in a no, or in no usable answer (a client that turns a dismissal into an
    /// error or an `accept` without `run` must not make the model's asking free): counts toward
    /// [`MAX_DECLINES`] for the connection. Not a yes, and not a question that never reached the
    /// client.
    fn counts_against_the_connection(reply: &Answer) -> bool {
        let result = match reply {
            Answer::Reply(result) => result,
            // The call was cancelled after the question was sent: a user who dismisses it by
            // interrupting the tool call has said no.
            Answer::App { approved } => return !approved,
            Answer::AppRefused(_) => return false,
            Answer::Cancelled | Answer::NoReply => return true,
            Answer::NotDelivered => return false,
        };
        match result.action {
            ElicitationAction::Accept => !matches!(
                result.content.as_ref().and_then(|c| c.get(ANSWER_PROPERTY)),
                Some(Value::String(answer)) if answer == ANSWER_RUN
            ),
            // A decline, a dismissal, or an action this build does not know.
            _ => true,
        }
    }

    /// What the client's reply means for a question titled [`ANSWER_TITLE`]. See
    /// [`decide_for`](Self::decide_for).
    pub fn decide(reply: Answer) -> ApprovalDecision {
        Self::decide_for(reply, ANSWER_TITLE)
    }

    /// What the client's reply means. Only `accept` with [`ANSWER_RUN`] approves. The other answers
    /// each say what happened, so the agent can tell the user: a decline, a dismissal, the choice
    /// "Do not run it" ([`chose_not_to`]), and an `accept` with no choice ([`no_choice`]) are
    /// different things with a reason of their own.
    pub fn decide_for(reply: Answer, title: &str) -> ApprovalDecision {
        let deny = |reason: Option<&str>| ApprovalDecision::Deny {
            reason: reason.map(str::to_owned),
        };
        let unanswered = || ApprovalDecision::Unanswered {
            reason: REASON_NO_ANSWER.to_owned(),
        };
        match reply {
            Answer::App { approved: true } => ApprovalDecision::Approve,
            Answer::App { approved: false } => deny(None),
            // Too many questions open in the app is the same busy as too many open here; any
            // other refusal says what the app said.
            Answer::AppRefused(text) if text.contains("too many questions") => ApprovalDecision::Refuse {
                reason: refusal(REASON_BUSY),
            },
            Answer::AppRefused(text) => ApprovalDecision::Refuse {
                reason: refusal(&format!("the tablex app could not take the question: {text}")),
            },
            Answer::Cancelled => deny(Some(REASON_CANCELLED)),
            Answer::NotDelivered => ApprovalDecision::Refuse {
                reason: refusal(REASON_NOT_DELIVERED),
            },
            Answer::NoReply => unanswered(),
            Answer::Reply(result) => match result.action {
                ElicitationAction::Decline => deny(None),
                ElicitationAction::Cancel => deny(Some(REASON_DISMISSED)),
                ElicitationAction::Accept => match result
                    .content
                    .as_ref()
                    .and_then(|content| content.get(ANSWER_PROPERTY))
                {
                    Some(Value::String(answer)) if answer == ANSWER_RUN => ApprovalDecision::Approve,
                    Some(Value::String(answer)) if answer == ANSWER_DO_NOT_RUN => {
                        deny(Some(&chose_not_to(title)))
                    }
                    // `accept` with no choice: nothing was chosen (a form left as it was).
                    None | Some(Value::Null) => deny(Some(&no_choice(title))),
                    // Something that is not one of the two choices (the boolean of an older
                    // form, a number, another word): not an answer.
                    Some(_) => unanswered(),
                },
                // `ElicitationAction` is non-exhaustive: an action this build does not know is
                // not a yes.
                _ => unanswered(),
            },
        }
    }
}

/// The longest run of consecutive backticks in `text`.
fn longest_backtick_run(text: &str) -> usize {
    text.split(|c| c != '`').map(str::len).max().unwrap_or(0)
}

/// `text` (one line) as a Markdown code span, which renders exactly this text: no escape, link,
/// image, emphasis, autolink or math applies inside one, whatever the renderer does with the text
/// around it. The delimiter is longer than any run of backticks in the text; a space is put on each
/// side when the text starts or ends with a backtick (it would merge into the delimiter) or starts
/// and ends with a space (CommonMark strips one from each side then). A blank text has nothing to
/// show and reads `(blank)`. The same construction as the Driver Manager's `code_span`.
pub fn code_span(text: &str) -> String {
    if text.trim().is_empty() {
        return "`(blank)`".to_owned();
    }
    let fence = "`".repeat(longest_backtick_run(text) + 1);
    let padded = text.starts_with('`')
        || text.ends_with('`')
        || (text.starts_with(' ') && text.ends_with(' '));
    if padded {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// A change to the saved connections, put to the user: the text for MCP elicitation and the same
/// facts for the app.
pub struct ConnectionQuestion {
    /// The Markdown question of elicitation.
    pub message: String,
    /// The question for the app (its `id` is set when it is sent).
    pub app: ApprovalAsk,
    /// What the user can do in the tablex app instead, for the refusal when the settings do not allow changes
    /// ("add the connection in the tablex app (New Connection)").
    pub instead: &'static str,
}

/// Where a question goes.
enum Route {
    /// MCP elicitation, to the client of the call.
    Elicit(Caller),
    /// The tablex app, with the client's name when it gave one.
    App {
        client: AppClient,
        name: Option<String>,
    },
}

/// The refusal text when an older app's switch is off; `instead` (a change to the connections) adds
/// what the user can do in the app instead.
pub fn switch_off(instead: Option<&str>) -> String {
    with_instead(REASON_SWITCH_OFF, instead)
}

/// The refusal text when the settings are missing or unreadable (see [`switch_off`] for `instead`).
pub fn settings_unavailable(instead: Option<&str>) -> String {
    with_instead(REASON_SETTINGS_UNAVAILABLE, instead)
}

fn with_instead(reason: &str, instead: Option<&str>) -> String {
    match instead {
        Some(instead) => format!("{reason}, or ask the user to {instead}"),
        None => reason.to_owned(),
    }
}

/// The refusal text when the client cannot ask and the app cannot be asked either: what the user
/// can do about it. `app`: why the app cannot be asked (`None`: this platform has no data directory,
/// so there is no app to look for).
pub fn no_way_to_ask(app: Option<&ClientError>) -> String {
    match app {
        None | Some(ClientError::NotRunning) => format!(
            "{REASON_NO_ELICITATION}, and the tablex app is not running to show them. Ask the user \
             to open the tablex app and try again, or to use a client that supports MCP elicitation"
        ),
        Some(ClientError::Unsupported) => format!(
            "{REASON_NO_ELICITATION}, and approval in the tablex app is not available on Windows yet. \
             Ask the user to use a client that supports MCP elicitation, or to run the change in \
             the tablex app"
        ),
        Some(error) => format!(
            "{REASON_NO_ELICITATION}, and the tablex app could not be asked ({error}). Ask the user \
             to open the tablex app (or restart it) and try again, or to use a client that supports \
             MCP elicitation"
        ),
    }
}

impl ElicitingApprover {
    /// Where a question goes: elicitation when the client can ask, else the tablex app when it
    /// answers, else the reason (a refusal's text, without [`OUTSIDE_TABLEX`]) it goes nowhere.
    fn route(&self) -> Result<Route, String> {
        let caller = Caller::current();
        if let Some(caller) = caller.clone().filter(Caller::can_ask) {
            return Ok(Route::Elicit(caller));
        }
        let Some(client) = self.app.clone() else {
            return Err(no_way_to_ask(None));
        };
        client
            .reachable()
            .map_err(|error| no_way_to_ask(Some(&error)))?;
        Ok(Route::App {
            client,
            name: caller.and_then(|caller| caller.client_name()),
        })
    }

    /// Puts `ask` to the user in the app and waits. Nothing but the app's authenticated answer
    /// approves; a cancelled call, a closed app and a dropped channel never do.
    fn ask_app(client: &AppClient, name: Option<String>, mut ask: ApprovalAsk, cancel: &CancelToken) -> Answer {
        ask.id = new_request_id();
        // Spelled out first, then cut: a name of control characters is short only once it is shown.
        ask.client = name.map(|name| cut_spelled(&spell_out(&name, false).0, MAX_CLIENT_NAME));
        match client.ask_approval(&ask, &|| cancel.is_cancelled()) {
            Ok(approved) => Answer::App { approved },
            Err(ClientError::Cancelled) => Answer::Cancelled,
            Err(ClientError::Refused(text)) => Answer::AppRefused(text),
            Err(_) => Answer::NotDelivered,
        }
    }

    /// Puts a change to the user's saved connections to them: `question` says what will be added,
    /// copied or removed. The same conditions as for a write apply that make sense for it, in this
    /// order: the app's settings allow changes, the client can ask (elicitation, form mode) or the app can,
    /// and the connection tools have not been declined [`MAX_DECLINES`] times in this server
    /// session or are not [`MAX_OPEN_QUESTIONS`] open. A production connection is not a reason to
    /// refuse here: the message says it is one. Only a yes approves.
    pub fn ask_connection_change(
        &self,
        question: ConnectionQuestion,
        cancel: &CancelToken,
    ) -> ApprovalDecision {
        let refuse = |reason: &str| ApprovalDecision::Refuse {
            reason: refusal(reason),
        };
        if !self.switch_on() {
            return refuse(&self.not_allowed(Some(question.instead)));
        }
        let route = match self.route() {
            Ok(route) => route,
            Err(reason) => return refuse(&reason),
        };
        let open = match self.open_question(CONNECTIONS_BUDGET) {
            Ok(open) => open,
            Err(reason) => return refuse(reason),
        };
        let answer = match route {
            Route::Elicit(caller) => {
                let Some(schema) = Self::schema_titled(CONNECTION_TITLE) else {
                    return refuse(REASON_NOT_DELIVERED);
                };
                caller.ask(
                    ElicitRequestParams::FormElicitationParams {
                        meta: None,
                        message: question.message,
                        requested_schema: schema,
                    },
                    cancel,
                )
            }
            Route::App { client, name } => Self::ask_app(&client, name, question.app, cancel),
        };
        if Self::counts_against_the_connection(&answer) {
            self.record_decline(CONNECTIONS_BUDGET);
        }
        drop(open);
        Self::decide_for(answer, CONNECTION_TITLE)
    }

    /// After a yes to [`ask_connection_change`](Self::ask_connection_change): the app's settings may
    /// have changed while the question waited. `Err` is a refusal's text.
    pub fn connection_change_still_allowed(&self, instead: &str) -> Result<(), String> {
        if self.switch_on() {
            Ok(())
        } else {
            Err(refusal(&self.not_allowed(Some(instead))))
        }
    }

    /// Conditions 1-3 of the module docs (and: no dangerous statement on a production connection),
    /// in that order. Where the question goes, or the reason (not yet a [`refusal`]) a write is
    /// refused without asking.
    fn preflight(&self, request: &WriteApproval<'_>) -> Result<Route, String> {
        if request.dangerous.is_some() && request.production {
            return Err(REASON_DANGEROUS.to_owned());
        }
        if !self.switch_on() {
            return Err(self.not_allowed(None));
        }
        if request.production {
            return Err(REASON_PRODUCTION.to_owned());
        }
        self.route()
    }

    /// The question for the app: the facts of [`message`](Self::message), as fields.
    fn app_question(request: &WriteApproval<'_>) -> ApprovalAsk {
        let name = |text: &str| spell_out(text, false).0;
        let commit = request.transaction.and_then(|transaction| transaction.commit);
        let noun = match (request.language, request.script) {
            (Language::Redis | Language::Mongo, _) => "command",
            (_, Some(_)) => "script",
            (Language::Sql | Language::Other, None) => "statement",
        };
        let mut ask = ApprovalAsk {
            title: if commit.is_some() {
                COMMIT_TITLE.to_owned()
            } else {
                format!("Run this {noun}")
            },
            connection: Some(name(request.connection_name)),
            connection_type: Some(if request.connection_type.trim().is_empty() {
                "No type".to_owned()
            } else {
                name(request.connection_type)
            }),
            database: request.database.map(name),
            collection: request.collection.map(name),
            danger: request.dangerous.map(name),
            ..ApprovalAsk::default()
        };
        // A dangerous statement and a commit are approved by a mouse click only.
        ask.click_only = request.dangerous.is_some() || commit.is_some();
        let plural = |n: usize| if n == 1 { "" } else { "s" };
        let (statements, spelled): (Vec<&str>, usize) = match commit {
            Some(statements) => (
                statements.iter().map(String::as_str).collect(),
                statements.iter().map(|text| spell_out(text, true).1).sum(),
            ),
            None => (vec![request.statement], spell_out(request.statement, true).1),
        };
        ask.statements = statements.iter().map(|text| spell_out(text, true).0).collect();
        if let Some(changes) = request.script {
            ask.notes.push(format!(
                "The script is one question: the {changes} statement{} below that change{} data are \
                 listed in order, and the statements that only read are not shown. Approving runs \
                 the whole script.",
                plural(changes),
                if changes == 1 { "s" } else { "" },
            ));
        }
        match request.transaction {
            Some(TransactionApproval { id, commit: Some(_) }) => ask.notes.push(format!(
                "The {} change{} below ran inside transaction {} and none is final yet: they \
                 become final only if you approve.",
                statements.len(),
                plural(statements.len()),
                name(id)
            )),
            Some(TransactionApproval { id, commit: None }) => ask.notes.push(format!(
                "The {noun} runs inside transaction {}. It is NOT committed until you approve the \
                 commit of the transaction; a rollback undoes it.",
                name(id)
            )),
            None => {}
        }
        if spelled > 0 {
            ask.notes.push(format!(
                "Note: the text below has {spelled} character{} without a shape of its own \
                 (controls, direction changes, zero-width characters), written as <U+XXXX>. What \
                 runs has the original characters.",
                plural(spelled),
            ));
        }
        let names = [Some(request.connection_name), Some(request.connection_type), request.database, request.collection];
        if names.into_iter().flatten().any(|text| text.chars().any(is_right_to_left)) {
            ask.notes.push(
                "Warning: a name above has right-to-left letters, which can change how the text \
                 around it is displayed. Read it with care."
                    .to_owned(),
            );
        }
        if statements.iter().any(|text| text.chars().any(is_right_to_left)) {
            ask.notes.push(
                "Warning: the text below has right-to-left letters, which can change how the text \
                 around them is displayed. Read it with care."
                    .to_owned(),
            );
        }
        ask
    }
}

impl WriteApprover for ElicitingApprover {
    fn approve(&self, request: &WriteApproval<'_>, cancel: &CancelToken) -> ApprovalDecision {
        let refuse = |reason: &str| ApprovalDecision::Refuse {
            reason: refusal(reason),
        };
        let route = match self.preflight(request) {
            Ok(route) => route,
            Err(reason) => return refuse(&reason),
        };
        let commit = request.transaction.and_then(|transaction| transaction.commit);
        let reviewed = match commit {
            // Every statement of the commit is shown: the whole list has to be reviewable.
            Some(statements) => statements.join("\n"),
            None => request.statement.to_owned(),
        };
        if let Some(reason) = Self::review_problem(&reviewed) {
            return refuse(if commit.is_some() {
                REASON_COMMIT_NOT_REVIEWABLE
            } else {
                reason
            });
        }
        let open = match self.open_question(request.connection_id) {
            Ok(open) => open,
            Err(reason) => return refuse(reason),
        };
        let answer = match route {
            Route::Elicit(caller) => {
                let Some(schema) = Self::schema(commit.is_some()) else {
                    return refuse(REASON_NOT_DELIVERED);
                };
                caller.ask(
                    ElicitRequestParams::FormElicitationParams {
                        meta: None,
                        message: Self::message(request),
                        requested_schema: schema,
                    },
                    cancel,
                )
            }
            Route::App { client, name } => {
                Self::ask_app(&client, name, Self::app_question(request), cancel)
            }
        };
        if Self::counts_against_the_connection(&answer) {
            self.record_decline(request.connection_id);
        }
        // The place is given back after the decline is counted, so the two never both miss it.
        drop(open);
        Self::decide_for(answer, if commit.is_some() { COMMIT_TITLE } else { ANSWER_TITLE })
    }

    /// The commit question lists every write, so the whole list has to pass the same review as
    /// one statement does (lines, characters, line width, hidden blanks).
    fn commit_reviewable(&self, statements: &[String]) -> Result<(), String> {
        match Self::review_problem(&statements.join("\n")) {
            Some(_) => Err(refusal(REASON_COMMIT_NOT_REVIEWABLE)),
            None => Ok(()),
        }
    }

    /// Before a transaction is begun: the reasons a write would be refused without asking.
    fn check_writable(&self, request: &WriteApproval<'_>) -> Result<(), String> {
        self.preflight(request)
            .map(drop)
            .map_err(|reason| refusal(&reason))
    }

    /// The question may have waited a long time: the layer asks again, after a yes, whether the
    /// settings still allow changes, so an older app's switch turned off stops a write that was
    /// asked while it was on (audited `approved, then refused: ...`).
    fn still_allowed(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        if self.switch_on() {
            Ok(())
        } else {
            Err(refusal(&self.not_allowed(None)))
        }
    }

    /// A dangerous statement is put to the user like any write, except on a production
    /// connection, where the layer refuses it without asking.
    fn allows_dangerous(&self, connection: &ConnectionInfo) -> bool {
        !connection.connection_type.production
    }
}

/// What the exchange with the client came to.
#[derive(Debug)]
pub enum Answer {
    /// The client replied.
    Reply(ElicitResult),
    /// The user answered in the tablex app (authenticated by the channel).
    App { approved: bool },
    /// The app was reached but would not take the question (it says why).
    AppRefused(String),
    /// The call was cancelled after the question was sent.
    Cancelled,
    /// The question did not reach the client (the connection is closed or gone).
    NotDelivered,
    /// The question was sent but nothing usable came back (an error from the client, a reply of
    /// another kind, the connection ended while it waited).
    NoReply,
}

/// The client of the call being run and the runtime that drives its protocol task.
#[derive(Clone)]
pub(crate) struct Caller {
    peer: Peer<RoleServer>,
    runtime: Handle,
}

thread_local! {
    static CURRENT: RefCell<Option<Caller>> = const { RefCell::new(None) };
}

/// Clears the thread's caller on drop. Not `Send`: it belongs to the thread that made it.
pub(crate) struct CallerGuard(PhantomData<*const ()>);

impl Drop for CallerGuard {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().take());
    }
}

impl Caller {
    pub(crate) fn new(peer: Peer<RoleServer>, runtime: Handle) -> Self {
        Self { peer, runtime }
    }

    /// Makes this the caller of the current thread until the guard is dropped.
    pub(crate) fn enter(self) -> CallerGuard {
        CURRENT.with(|current| *current.borrow_mut() = Some(self));
        CallerGuard(PhantomData)
    }

    fn current() -> Option<Self> {
        CURRENT.with(|current| current.borrow().clone())
    }

    /// The name the client gave itself in the handshake (not verified).
    fn client_name(&self) -> Option<String> {
        self.peer
            .peer_info()
            .map(|info| info.client_info.name.clone())
            .filter(|name| !name.trim().is_empty())
    }

    /// The client declared the elicitation capability, form mode (the only mode used).
    fn can_ask(&self) -> bool {
        self.peer
            .supported_elicitation_modes()
            .contains(&ElicitationMode::Form)
    }

    /// Sends the question and waits for the answer on this (blocking) thread.
    fn ask(&self, params: ElicitRequestParams, cancel: &CancelToken) -> Answer {
        let (done, answered) = oneshot::channel();
        let stop = CancellationToken::new();
        // Cheap and non-blocking, so it is safe on whatever thread cancels the call. Registered
        // before the question is sent; an already cancelled token fires it at once.
        let hook = {
            let stop = stop.clone();
            cancel.on_cancel(move || stop.cancel())
        };
        self.runtime
            .spawn(Self::exchange(self.peer.clone(), params, stop, done));
        // The sender is dropped without a value only when the task is gone (the runtime shut
        // down): nothing will ever answer, and the question did not get through.
        let answer = answered.blocking_recv().unwrap_or(Answer::NotDelivered);
        drop(hook);
        answer
    }

    /// The protocol side, on the server's runtime: send, then wait for the reply or the stop.
    async fn exchange(
        peer: Peer<RoleServer>,
        params: ElicitRequestParams,
        stop: CancellationToken,
        done: oneshot::Sender<Answer>,
    ) {
        let request = ServerRequest::ElicitRequest(ElicitRequest::new(params));
        let sent = tokio::select! {
            biased;
            () = stop.cancelled() => {
                // Cancelled before the question was sent: nothing was asked.
                let _ = done.send(Answer::NotDelivered);
                return;
            }
            sent = peer.send_cancellable_request(request, PeerRequestOptions::no_options()) => sent,
        };
        let Ok(mut handle) = sent else {
            let _ = done.send(Answer::NotDelivered);
            return;
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => {
                // The waiting thread goes on first; telling the client may take as long as its
                // pipe takes.
                let _ = done.send(Answer::Cancelled);
                let _ = handle.cancel(Some(REASON_CANCELLED.to_owned())).await;
            }
            reply = &mut handle.rx => {
                // An error from the client, a reply of another kind, a connection that ended.
                let _ = done.send(match reply {
                    Ok(Ok(ClientResult::ElicitResult(result))) => Answer::Reply(result),
                    _ => Answer::NoReply,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request<'a>(statement: &'a str, database: Option<&'a str>) -> WriteApproval<'a> {
        WriteApproval {
            connection_id: "c1",
            connection_name: "Warehouse",
            connection_type: "Development",
            production: false,
            language: Language::Sql,
            database,
            collection: None,
            statement,
            script: None,
            dangerous: None,
            transaction: None,
        }
    }

    fn reply(action: ElicitationAction, content: Option<Value>) -> Answer {
        let mut result = ElicitResult::new(action);
        result.content = content;
        Answer::Reply(result)
    }

    /// What a Markdown parser makes of `message`: the value of every code block, the value of
    /// every inline code span, and the kinds of every other node (so a link, image, heading, HTML
    /// or emphasis shows up).
    fn parsed(
        message: &str,
        options: &markdown::ParseOptions,
    ) -> (Vec<String>, Vec<String>, Vec<String>) {
        use markdown::mdast::Node;
        fn walk(
            node: &Node,
            blocks: &mut Vec<String>,
            spans: &mut Vec<String>,
            kinds: &mut Vec<String>,
        ) {
            match node {
                Node::Code(block) => return blocks.push(block.value.clone()),
                Node::InlineCode(span) => return spans.push(span.value.clone()),
                _ => {}
            }
            let debug = format!("{node:?}");
            kinds.push(debug.chars().take_while(|c| c.is_alphanumeric()).collect());
            for child in node.children().into_iter().flatten() {
                walk(child, blocks, spans, kinds);
            }
        }
        let root = markdown::to_mdast(message, options).unwrap();
        let (mut blocks, mut spans, mut kinds) = (Vec::new(), Vec::new(), Vec::new());
        walk(&root, &mut blocks, &mut spans, &mut kinds);
        (blocks, spans, kinds)
    }

    /// The parsers a client may use: CommonMark, GFM (autolink literals, tables, strikethrough,
    /// footnotes, task lists) and GFM with math.
    fn parsers() -> Vec<(&'static str, markdown::ParseOptions)> {
        let mut math = markdown::ParseOptions::gfm();
        math.constructs.math_text = true;
        math.constructs.math_flow = true;
        math.math_text_single_dollar = true;
        vec![
            ("commonmark", markdown::ParseOptions::default()),
            ("gfm", markdown::ParseOptions::gfm()),
            ("gfm+math", math),
        ]
    }

    /// The message shows `statement` (as spelled) in exactly one code block, each name in exactly
    /// one inline code span whose text is the name as typed (as spelled), and nothing else of the
    /// message is anything but plain paragraphs of text, for every parser in [`parsers`].
    fn assert_literal(request: &WriteApproval<'_>) {
        let message = ElicitingApprover::message(request);
        // The block's value has `\n` line endings whatever the statement used.
        let normalised = |text: &str| text.replace("\r\n", "\n");
        let shown = normalised(&spell_out(request.statement, true).0);
        let spelled = |text: &str| spell_out(text, false).0;
        let mut names = vec![spelled(request.connection_name)];
        names.push(if request.connection_type.trim().is_empty() {
            "No type".to_owned()
        } else {
            spelled(request.connection_type)
        });
        names.extend(request.database.map(spelled));
        names.extend(request.collection.map(spelled));
        // A name with nothing to show reads `(blank)`; "No type" is plain text, not a span.
        let names: Vec<String> = names
            .into_iter()
            .filter(|name| name != "No type" || !request.connection_type.trim().is_empty())
            .map(|name| {
                if name.trim().is_empty() {
                    "(blank)".to_owned()
                } else {
                    name
                }
            })
            .collect();
        for (name, options) in parsers() {
            let (blocks, spans, kinds) = parsed(&message, &options);
            let blocks: Vec<String> = blocks.iter().map(|value| normalised(value)).collect();
            assert_eq!(
                blocks,
                std::slice::from_ref(&shown),
                "{name}: one code block with the statement\n{message}"
            );
            assert_eq!(spans, names, "{name}: each name one code span\n{message}");
            for kind in &kinds {
                assert!(
                    matches!(kind.as_str(), "Root" | "Paragraph" | "Text"),
                    "{name}: {kind} in\n{message}"
                );
            }
        }
        // The block is the last thing: the message ends with the closing fence.
        assert!(message.ends_with('`'), "{message}");
    }

    #[test]
    fn a_commit_question_shows_every_statement_in_a_code_block_of_its_own() {
        let statements: Vec<String> = [
            "INSERT INTO t VALUES ('```')",
            "UPDATE t SET a = 1\n-- ```\n```\n# a heading\n[x](http://example.com)",
            "DELETE FROM t WHERE a = 2",
        ]
        .map(str::to_owned)
        .into();
        let mut commit = request("COMMIT", None);
        commit.transaction = Some(TransactionApproval {
            id: "tx-1",
            commit: Some(&statements),
        });
        let message = ElicitingApprover::message(&commit);
        assert!(message.contains("Change 3 of 3"), "{message}");
        for (name, options) in parsers() {
            let (blocks, spans, kinds) = parsed(&message, &options);
            assert_eq!(blocks, statements, "{name}: one block per statement\n{message}");
            assert_eq!(spans, ["`Warehouse`", "`Development`", "`tx-1`"].map(|s| s.trim_matches('`')), "{name}");
            for kind in &kinds {
                assert!(
                    matches!(kind.as_str(), "Root" | "Paragraph" | "Text"),
                    "{name}: {kind} in\n{message}"
                );
            }
        }
        // A statement inside the transaction says it is not committed yet.
        let mut inside = request("INSERT INTO t VALUES (1)", None);
        inside.transaction = Some(TransactionApproval {
            id: "tx-1",
            commit: None,
        });
        let message = ElicitingApprover::message(&inside);
        assert!(message.contains("runs inside transaction `tx-1`"), "{message}");
        assert!(message.contains("NOT committed"), "{message}");
        // The commit of too many lines is refused with the roll-back reason, not asked.
        let long: Vec<String> = (0..MAX_LINES + 1).map(|n| format!("INSERT INTO t VALUES ({n})")).collect();
        assert!(ElicitingApprover::review_problem(&long.join("\n")).is_some());
    }

    #[test]
    fn the_message_is_pinned_for_a_comment_that_markdown_would_eat() {
        let statement = "UPDATE t SET a=1 WHERE 1=1 /*AND id=5*/";
        let message = ElicitingApprover::message(&request(statement, Some("main")));
        assert_eq!(
            message,
            "An MCP client asks to run a statement that changes data. Nothing runs unless you accept. To run it, choose \"Run this statement\" and accept.\n\n\
             Connection: `Warehouse` (`Development`)\n\n\
             Database: `main`\n\n\
             The statement has 1 line and 39 characters.\n\n\
             The statement is between the two lines of 3 backticks below. Everything between them is the \
             statement; nothing follows the second line.\n\n\
             ```\n\
             UPDATE t SET a=1 WHERE 1=1 /*AND id=5*/\n\
             ```"
        );
        assert_literal(&request(statement, Some("main")));
    }

    #[test]
    fn the_message_is_pinned_for_a_link_target_that_swallows_the_rest() {
        let statement = "SET note='[ok](<', is_admin=true WHERE id=1 OR name='>)'";
        let message = ElicitingApprover::message(&request(statement, None));
        assert_eq!(
            message,
            "An MCP client asks to run a statement that changes data. Nothing runs unless you accept. To run it, choose \"Run this statement\" and accept.\n\n\
             Connection: `Warehouse` (`Development`)\n\n\
             The statement has 1 line and 56 characters.\n\n\
             The statement is between the two lines of 3 backticks below. Everything between them is the \
             statement; nothing follows the second line.\n\n\
             ```\n\
             SET note='[ok](<', is_admin=true WHERE id=1 OR name='>)'\n\
             ```"
        );
        assert_literal(&request(statement, None));
    }

    #[test]
    fn a_statement_is_literal_for_a_markdown_parser_whatever_it_contains() {
        for statement in [
            "UPDATE t SET a=1 WHERE 1=1 /*AND id=5*/",
            "SET note='[ok](<', is_admin=true WHERE id=1 OR name='>)'",
            "UPDATE t SET a='<!-- hidden --> x' WHERE id = 1",
            "UPDATE t SET a='![](https://host.example/?d=secret)' WHERE id = 1",
            "# a comment line\nUPDATE t SET a = 1 WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = 1\n-- ===\n-- ---",
            "UPDATE t SET a='*b* _c_ ~~d~~ <b>e</b> &amp;' WHERE id = 1",
            "UPDATE `t` SET a = '```' WHERE b = '````'",
            "UPDATE t SET a = '``````' WHERE id = 1",
            "UPDATE t SET a = '\n```\nDROP TABLE t;\n```\n' WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = 1 -- ```sql",
            "UPDATE t SET a = '`' WHERE id = 1",
            "    UPDATE t SET a = 1\n\n\n      WHERE id = 1",
            "UPDATE t SET a = 1 WHERE id = '\u{202E}x\u{200B}'",
            "UPDATE t SET a = 1\r\nWHERE id = 1\r\n  AND b = '```'\r\n",
            "UPDATE t SET a = 'https://evil.example/x www.evil.example a@b.example $x^2$ $$'",
        ] {
            assert_literal(&request(statement, Some("main")));
        }
    }

    #[test]
    fn the_fence_is_longer_than_the_longest_run_of_backticks() {
        for (statement, fence) in [
            ("UPDATE t SET a = 1", 3),
            ("a ` b", 3),
            ("a `` b", 3),
            ("a ``` b", 4),
            ("a ```` b ``` c", 5),
            ("``````", 7),
        ] {
            let message = ElicitingApprover::message(&request(statement, None));
            let ticks = "`".repeat(fence);
            assert!(
                message.contains(&format!("between the two lines of {fence} backticks")),
                "{statement}: {message}"
            );
            assert!(
                message.ends_with(&format!("\n{ticks}\n{statement}\n{ticks}")),
                "{statement}: {message}"
            );
            assert_eq!((longest_backtick_run(statement) + 1).max(3), fence);
        }
    }

    #[test]
    fn code_span_renders_exactly_the_text() {
        assert_eq!(code_span("main"), "`main`");
        assert_eq!(code_span("a`b"), "``a`b``");
        assert_eq!(code_span("`a"), "`` `a ``");
        assert_eq!(code_span("a`"), "`` a` ``");
        assert_eq!(code_span("a``b`c"), "```a``b`c```");
        assert_eq!(code_span(" a "), "`  a  `");
        assert_eq!(code_span(" a"), "` a`");
        assert_eq!(code_span(""), "`(blank)`");
        assert_eq!(code_span("   "), "`(blank)`");
    }

    #[test]
    fn names_are_code_spans_that_read_as_typed_for_every_parser() {
        let nasty = "**prod** [x](http://e.example) ![i](http://e.example/x.png) <b>x</b> `c` _u_ &amp; ~~s~~ a|b";
        let mut named = request("UPDATE t SET a = 1 WHERE id = 1", Some(nasty));
        named.connection_name = nasty;
        named.connection_type = nasty;
        let message = ElicitingApprover::message(&named);
        assert!(
            message.contains(&format!(
                "Connection: {0} ({0})\n\nDatabase: {0}\n\n",
                code_span(nasty)
            )),
            "{message}"
        );
        assert_literal(&named);
        // Names that GFM would link or render as math, ones that are all Markdown, backticks at
        // the ends or inside, padded with spaces, a blank one and a spelled one.
        for name in [
            "https://evil.example/x",
            "http://evil.example",
            "www.evil.example",
            "a@b.example",
            "mailto:a@b.example",
            "$x^2$",
            "$$x$$",
            "prod.db / v1.5",
            "`",
            "``",
            "`a",
            "a`",
            "a``b```c",
            " a ",
            " a",
            "a b",
            "# heading",
            "- item",
            "> quote",
            "1. one",
            "***",
            "---",
            "[^note]: x",
            "   ",
            "a\u{202E}b",
            "line\nbreak\ttab",
        ] {
            let mut named = request("UPDATE t SET a = 1 WHERE id = 1", Some(name));
            named.connection_name = name;
            named.connection_type = name;
            assert_literal(&named);
        }
    }

    #[test]
    fn the_message_states_lines_and_characters_the_size_and_the_spelling() {
        let statement = "UPDATE items SET label = 'x'\nWHERE id = 1";
        let message = ElicitingApprover::message(&request(statement, Some("main")));
        assert!(
            message.contains("Connection: `Warehouse` (`Development`)\n\n"),
            "{message}"
        );
        assert!(message.contains("Database: `main`\n\n"), "{message}");
        assert!(
            message.contains("The statement has 2 lines and 41 characters.\n\n"),
            "{message}"
        );
        assert!(!message.contains("Note:") && !message.contains("Warning:"));
        let one = ElicitingApprover::message(&request("DELETE FROM t WHERE a = 1", None));
        assert!(one.contains("has 1 line and 25 characters.\n"), "{one}");
        assert!(!one.contains("Database:"), "{one}");
        // A long single line says how long it is.
        let long = format!("UPDATE t SET a = '{}'", "x".repeat(5000));
        let message = ElicitingApprover::message(&request(&long, None));
        assert!(
            message.contains("has 1 line and 5019 characters."),
            "{message}"
        );
    }

    #[test]
    fn shapeless_and_right_to_left_characters_are_spelled_and_announced() {
        let statement = "SELECT '\u{202E}x\u{200B}' -- \u{5D0}";
        let message = ElicitingApprover::message(&request(statement, None));
        assert!(message.contains("<U+202E>x<U+200B>"), "{message}");
        assert!(
            message.contains("2 characters without a shape"),
            "{message}"
        );
        assert!(message.contains("right-to-left letters"), "{message}");
        assert!(!message.contains('\u{202E}') && !message.contains('\u{200B}'));
        // A name is one line: its breaks are spelled, inside its code span.
        let mut named = request("DELETE FROM t WHERE a = 1", Some("db\nx"));
        named.connection_name = "a\u{7}b";
        let message = ElicitingApprover::message(&named);
        assert!(message.contains("Connection: `a<U+0007>b` ("), "{message}");
        assert!(message.contains("Database: `db<U+000A>x`\n"), "{message}");
    }

    #[test]
    fn only_accept_with_the_run_choice_approves() {
        use ElicitationAction::{Accept, Cancel, Decline};
        let decide = ElicitingApprover::decide;
        assert_eq!(
            decide(reply(Accept, Some(json!({"answer": "run"})))),
            ApprovalDecision::Approve
        );
        // A person's no, each with a reason of its own (a plain decline has none to add).
        for no in [
            reply(Accept, Some(json!({"answer": "do_not_run"}))),
            reply(Accept, Some(json!({}))),
            reply(Accept, None),
            reply(Accept, Some(json!({"answer": null}))),
            reply(Decline, None),
            reply(Decline, Some(json!({"answer": "run"}))),
            reply(Cancel, Some(json!({"answer": "run"}))),
        ] {
            assert!(ElicitingApprover::counts_against_the_connection(&no));
            assert!(matches!(decide(no), ApprovalDecision::Deny { .. }));
        }
        // Not a choice: not a yes either, and it counts like a decline (a client that turns a
        // dismissal into an error must not make the model's asking free).
        for odd in [
            reply(Accept, Some(json!({"answer": "yes"}))),
            reply(Accept, Some(json!({"answer": "RUN"}))),
            reply(Accept, Some(json!({"answer": true}))),
            reply(Accept, Some(json!({"answer": 1}))),
            reply(Accept, Some(json!({"answer": ["run"]}))),
            // The boolean form of an older question: never a yes.
            reply(Accept, Some(json!({"run": true}))),
            reply(Accept, Some(json!("run"))),
            reply(Accept, Some(json!([true]))),
            Answer::NoReply,
        ] {
            assert!(ElicitingApprover::counts_against_the_connection(&odd));
            let decision = decide(odd);
            assert!(!matches!(decision, ApprovalDecision::Approve), "{decision:?}");
        }
        assert_eq!(
            decide(reply(Accept, Some(json!({"answer": "yes"})))),
            ApprovalDecision::Unanswered { reason: REASON_NO_ANSWER.to_owned() }
        );
        assert!(matches!(decide(Answer::NotDelivered), ApprovalDecision::Refuse { .. }));
        // A question that never reached the client is nobody's answer.
        assert!(!ElicitingApprover::counts_against_the_connection(&Answer::NotDelivered));
        assert!(!ElicitingApprover::counts_against_the_connection(&reply(Accept, Some(json!({"answer": "run"})))));
        assert!(matches!(decide(Answer::Cancelled), ApprovalDecision::Deny { .. }));
    }

    #[test]
    fn the_requested_schema_is_one_required_choice_with_no_default() {
        let schema = serde_json::to_value(ElicitingApprover::schema(false).unwrap()).unwrap();
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "answer": {
                        "type": "string",
                        "title": "Run this statement?",
                        "enum": ["do_not_run", "run"],
                        "enumNames": ["Do not run it", "Run this statement"]
                    }
                },
                "required": ["answer"]
            })
        );
        let commit = serde_json::to_value(ElicitingApprover::schema(true).unwrap()).unwrap();
        assert_eq!(commit["properties"]["answer"]["enumNames"], json!(["Do not commit it", "Commit this transaction"]));
        let connections = serde_json::to_value(ElicitingApprover::schema_titled(CONNECTION_TITLE).unwrap()).unwrap();
        assert_eq!(
            connections["properties"]["answer"]["enumNames"],
            json!(["Do not make this change", "Make this change to your connections"])
        );
        // One property, required, and nothing for a client to pre-fill.
        for schema in [schema, commit, connections] {
            assert_eq!(schema["required"], json!(["answer"]));
            assert_eq!(schema["properties"].as_object().unwrap().len(), 1);
            assert!(schema["properties"]["answer"].get("default").is_none(), "{schema}");
        }
    }

    #[test]
    fn open_questions_are_counted_per_connection_and_in_all_and_given_back() {
        let approver = ElicitingApprover::new(None);
        // One connection: its declines plus its open questions stay under the limit.
        let mut held: Vec<_> = (0..MAX_DECLINES)
            .map(|_| approver.open_question("a").expect("under the limit"))
            .collect();
        assert_eq!(approver.open_question("a").err(), Some(REASON_BUSY));
        // With a decline recorded, one place fewer.
        drop(held.pop());
        drop(held.pop());
        approver.record_decline("a");
        held.push(approver.open_question("a").expect("one decline, two open"));
        assert_eq!(approver.open_question("a").err(), Some(REASON_BUSY));
        // The limit is reached by declines alone: the reason is the session's.
        approver.record_decline("a");
        approver.record_decline("a");
        assert_eq!(
            approver.open_question("a").err(),
            Some(REASON_DECLINED_OFTEN)
        );
        // Another connection is not touched, up to the cap in all.
        held.extend((0..MAX_OPEN_QUESTIONS - held.len()).map(|n| {
            approver
                .open_question(&format!("other-{n}"))
                .expect("under the cap")
        }));
        assert_eq!(approver.open.load(Ordering::SeqCst), MAX_OPEN_QUESTIONS);
        assert_eq!(approver.open_question("z").err(), Some(REASON_BUSY));
        drop(held);
        assert_eq!(approver.open.load(Ordering::SeqCst), 0, "all given back");
        assert!(approver.open_question("z").is_ok());
        assert_eq!(approver.budgets()["a"].open, 0);
    }

    #[test]
    fn a_statement_is_reviewable_as_it_is_shown() {
        let problem = ElicitingApprover::review_problem;
        let line = |n: usize| format!("UPDATE t SET a = '{}' WHERE id = 1", "x".repeat(n));
        assert_eq!(problem(&line(MAX_LINE_CHARS - 32)), None);
        assert_eq!(problem(&line(MAX_LINE_CHARS - 31)), Some(REASON_TOO_LONG));
        // 8000 characters in lines of 100 are fine, one more is not.
        let block = |n: usize| {
            format!(
                "{}{}",
                ("x".repeat(99) + "\n").repeat(n / 100),
                "y".repeat(n % 100)
            )
        };
        assert_eq!(block(MAX_CHARS).chars().count(), MAX_CHARS);
        assert_eq!(problem(&block(MAX_CHARS)), None);
        assert_eq!(problem(&block(MAX_CHARS + 1)), Some(REASON_TOO_LONG));
        // The limit counts the text as shown: 2000 zero-width characters are 14 000 shown.
        assert_eq!(problem(&"\u{200B}".repeat(2000)), Some(REASON_TOO_LONG));
        assert_eq!(problem(&"\u{200B}".repeat(50)), None);
    }

    #[test]
    fn blanks_cannot_push_text_out_of_sight() {
        let problem = ElicitingApprover::review_problem;
        let head = "UPDATE t SET x=1 WHERE id=5";
        let tail = "OR 1=1";
        // Spaces: a run of 24 is fine, 25 is not.
        assert_eq!(
            problem(&format!("{head}{}{tail}", " ".repeat(MAX_BLANK_RUN))),
            None
        );
        assert_eq!(
            problem(&format!("{head}{}{tail}", " ".repeat(MAX_BLANK_RUN + 1))),
            Some(REASON_HIDDEN_BLANKS)
        );
        // Tabs count 8: three are 24, a fourth is too wide, and so are three and a space.
        assert_eq!(problem(&format!("{head}{}{tail}", "\t".repeat(3))), None);
        assert_eq!(
            problem(&format!("{head}{}{tail}", "\t".repeat(4))),
            Some(REASON_HIDDEN_BLANKS)
        );
        assert_eq!(
            problem(&format!("{head}\t\t\t {tail}")),
            Some(REASON_HIDDEN_BLANKS)
        );
        // Mixed: three spaces and three tabs are 27 columns.
        assert_eq!(
            problem(&format!("{head} \t \t \t {tail}")),
            Some(REASON_HIDDEN_BLANKS)
        );
        // The review's example: about 350 blanks, then more text.
        assert_eq!(
            problem(&format!("{head}{}{tail}", " ".repeat(350))),
            Some(REASON_HIDDEN_BLANKS)
        );
        // Indentation at the start of a line is fine, however deep (up to the width), and so are
        // blanks at the end of a line, and blank runs split by text.
        assert_eq!(problem(&format!("{}{head}", " ".repeat(100))), None);
        assert_eq!(problem(&format!("{}{head}", "\t".repeat(20))), None);
        assert_eq!(problem(&format!("{head}\n{}{tail}", " ".repeat(200))), None);
        assert_eq!(problem(&format!("{head}{}", " ".repeat(100))), None);
        assert_eq!(
            problem(&format!("{head}{0}a{0}{tail}", " ".repeat(20))),
            None
        );
        // The width counts a tab as 8: 1 + 49 tabs + 7 spaces is 400 columns, one more is 401.
        let wide = |spaces: usize| format!("x{}{}", "\t".repeat(49), " ".repeat(spaces));
        assert_eq!(problem(&wide(7)), None);
        assert_eq!(problem(&wide(8)), Some(REASON_TOO_LONG));
        // The same width in spaces only.
        assert_eq!(problem(&format!("x{}", " ".repeat(399))), None);
        assert_eq!(
            problem(&format!("x{}", " ".repeat(400))),
            Some(REASON_TOO_LONG)
        );
    }

    #[test]
    fn every_kind_of_space_is_a_blank_and_zero_width_characters_cannot_split_a_run() {
        let problem = ElicitingApprover::review_problem;
        let head = "UPDATE t SET x=1 WHERE id=5 /*";
        let tail = "*/ OR 1=1";
        // The Unicode spaces draw as empty space just like U+0020 does.
        for space in [
            '\u{00A0}', '\u{1680}', '\u{2000}', '\u{2003}', '\u{200A}', '\u{202F}', '\u{205F}',
        ] {
            let run = |n: usize| format!("{head}{}{tail}", space.to_string().repeat(n));
            assert_eq!(problem(&run(MAX_BLANK_RUN)), None, "U+{:04X}", space as u32);
            assert_eq!(
                problem(&run(MAX_BLANK_RUN + 1)),
                Some(REASON_HIDDEN_BLANKS),
                "U+{:04X}",
                space as u32
            );
        }
        // The ideographic space is two columns wide: twelve are 24, a thirteenth is too many.
        let wide = |n: usize| format!("{head}{}{tail}", "\u{3000}".repeat(n));
        assert_eq!(problem(&wide(12)), None);
        assert_eq!(problem(&wide(13)), Some(REASON_HIDDEN_BLANKS));
        // The review's example with no-break spaces.
        assert_eq!(
            problem(&format!("{head}{}{tail}", "\u{00A0}".repeat(350))),
            Some(REASON_HIDDEN_BLANKS)
        );
        // A zero-width character between two runs is spelled out (it is text a reviewer sees), in
        // the supplementary planes too; what must not happen is that it hides AND splits the run.
        for hidden in ['\u{200B}', '\u{1D173}', '\u{1BCA0}', '\u{13430}'] {
            let statement = format!("{head}{0}{hidden}{0}{tail}", " ".repeat(MAX_BLANK_RUN));
            let (shown, spelled) = spell_out(&statement, true);
            assert_eq!(spelled, 1, "U+{:04X}", hidden as u32);
            assert!(
                shown.contains(&format!("<U+{:04X}>", hidden as u32)),
                "{shown}"
            );
        }
        // A `\r\n` statement: the `\r` ends the line, it is not text behind trailing blanks.
        assert_eq!(
            problem(&format!("{head}{}\r\n{tail}", " ".repeat(100))),
            None
        );
        assert_eq!(
            problem(&format!("{head}{}{tail}\r\nSELECT 1", " ".repeat(25))),
            Some(REASON_HIDDEN_BLANKS)
        );
    }

    #[test]
    fn right_to_left_letters_in_a_name_are_announced() {
        let named = |name: &'static str| WriteApproval {
            connection_name: name,
            ..request("UPDATE t SET x = 1", None)
        };
        assert!(!ElicitingApprover::message(&named("Warehouse")).contains("a name above"));
        assert!(ElicitingApprover::message(&named("מחסן"))
            .contains("Warning: a name above has right-to-left letters"));
    }

    fn switch_on_file() -> (tempfile::TempDir, std::path::PathBuf) {
        settings_file(r#"{"schema_version": 7, "ai": {"agent_access": "read_write"}}"#)
    }

    fn settings_file(json: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, json).unwrap();
        (dir, path)
    }

    #[test]
    fn the_old_switch_counts_only_for_a_file_of_an_older_app() {
        let write = request("UPDATE t SET x = 1", None);
        let refused = |json: &str, reason: String| {
            let (_dir, path) = settings_file(json);
            let approver = ElicitingApprover::new(Some(path));
            let want = ApprovalDecision::Refuse { reason: refusal(&reason) };
            assert_eq!(approver.approve(&write, &CancelToken::new()), want, "{json}");
            assert_eq!(approver.check_writable(&write), Err(refusal(&reason)), "{json}");
            assert_eq!(approver.still_allowed(&write), Err(refusal(&reason)), "{json}");
        };
        // Written by an older app: `false` still blocks, naming the switch it shows.
        refused(r#"{"schema_version": 6, "ai": {"mcp_writes": false}}"#, switch_off(None));
        refused(r#"{"schema_version": 4, "ai": {"mcp_writes": false}}"#, switch_off(None));
        // An explicit `false` at schema 7 or later (the old switch turned off after a downgrade and
        // back) is the user's "no" too.
        refused(r#"{"schema_version": 7, "ai": {"mcp_writes": false}}"#, switch_off(None));
        refused(r#"{"schema_version": 8, "ai": {"mcp_writes": false, "agent_access": "read_write"}}"#, switch_off(None));
        // No file, or one that cannot be read: the settings are named, not a switch.
        refused("{ not json", settings_unavailable(None));
        refused(r#"{"schema_version": 7, "ai": {"mcp_writes": "yes"}}"#, settings_unavailable(None));
        assert_eq!(ElicitingApprover::new(None).approve(&write, &CancelToken::new()), ApprovalDecision::Refuse {
            reason: refusal(&settings_unavailable(None))
        });
        // Schema 7 and later: no switch. Everything else is as before (here: no client to ask).
        for json in [
            r#"{"schema_version": 7, "ai": {"agent_access": "read_write"}}"#,
            r#"{"schema_version": 7, "ai": {"mcp_writes": true}}"#,
        ] {
            let (_dir, path) = settings_file(json);
            let approver = ElicitingApprover::new(Some(path));
            assert_eq!(
                approver.approve(&write, &CancelToken::new()),
                ApprovalDecision::Refuse { reason: refusal(&no_way_to_ask(None)) },
                "{json}"
            );
            assert_eq!(approver.still_allowed(&write), Ok(()), "{json}");
            // A production connection is never asked about, with or without the key.
            let mut production = request("UPDATE t SET x = 1", None);
            production.production = true;
            assert_eq!(
                approver.approve(&production, &CancelToken::new()),
                ApprovalDecision::Refuse { reason: refusal(REASON_PRODUCTION) },
                "{json}"
            );
        }
    }

    #[test]
    fn a_call_without_a_client_is_refused_even_when_changes_are_allowed() {
        let (_dir, path) = switch_on_file();
        let approver = ElicitingApprover::new(Some(path));
        assert_eq!(
            approver.approve(
                &request("DELETE FROM t WHERE a = 1", None),
                &CancelToken::new()
            ),
            ApprovalDecision::Refuse {
                reason: refusal(&no_way_to_ask(None))
            }
        );
    }

    #[test]
    fn a_dangerous_request_is_refused_on_production_by_the_approver_itself() {
        let (_dir, path) = switch_on_file();
        let approver = ElicitingApprover::new(Some(path));
        let mut dangerous = request("DELETE FROM t", None);
        dangerous.dangerous = Some("DELETE without WHERE");
        // Not of a production type: it is asked like any write (here: no client to ask).
        assert_eq!(
            approver.approve(&dangerous, &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(&no_way_to_ask(None))
            }
        );
        dangerous.production = true;
        assert_eq!(
            approver.approve(&dangerous, &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(REASON_DANGEROUS)
            }
        );
        assert_eq!(
            approver.check_writable(&dangerous),
            Err(refusal(REASON_DANGEROUS))
        );
    }

    #[test]
    fn a_dangerous_statement_is_allowed_unless_the_connection_type_is_production() {
        let approver = ElicitingApprover::new(None);
        let mut connection = ConnectionInfo {
            id: "c1".into(),
            name: "Warehouse".into(),
            kind: engine_agent::ConnectionKind::PostgreSql,
            connection_type: engine_agent::ConnectionTypeInfo {
                name: "Development".into(),
                ..Default::default()
            },
            agent_access: None,
            statement_timeout_secs: None,
            allowed_databases: None,
            endpoint: None,
            group: Vec::new(),
            notes: None,
            revision: Default::default(),
        };
        assert!(approver.allows_dangerous(&connection));
        // A connection with no type counts as not production.
        connection.connection_type = Default::default();
        assert!(approver.allows_dangerous(&connection));
        connection.connection_type.production = true;
        assert!(!approver.allows_dangerous(&connection));
    }

    #[test]
    fn the_question_for_a_dangerous_statement_says_so_and_names_the_reason() {
        let mut dangerous = request("DROP TABLE t", None);
        dangerous.dangerous = Some("DROP permanently removes database objects");
        let message = ElicitingApprover::message(&dangerous);
        assert!(message.starts_with("DANGEROUS: an MCP client asks to run a statement"), "{message}");
        assert!(
            message.contains("Why it is dangerous: DROP permanently removes database objects"),
            "{message}"
        );
        assert!(message.contains("Nothing runs unless you accept."), "{message}");
        let plain = ElicitingApprover::message(&request("DROP TABLE t", None));
        assert!(!plain.contains("DANGEROUS"), "{plain}");
    }

    fn mongo_request(statement: &str) -> WriteApproval<'_> {
        WriteApproval {
            language: Language::Mongo,
            database: Some("shop"),
            collection: Some("orders"),
            ..request(statement, None)
        }
    }

    #[test]
    fn a_mongodb_write_names_the_database_and_the_collection_and_shows_the_command() {
        let statement = "{\n  \"delete\": \"orders\",\n  \"deletes\": [\n    {\n      \"q\": {\n        \"note\": \"``` [x](y)\"\n      },\n      \"limit\": 1\n    }\n  ]\n}";
        let message = ElicitingApprover::message(&mongo_request(statement));
        assert!(message.starts_with(
            "An MCP client asks to run a command that changes data. Nothing runs unless you accept. To run it, choose \"Run this statement\" and accept.\n\n\
             Connection: `Warehouse` (`Development`)\n\n\
             Database: `shop`\n\n\
             Collection: `orders`\n\n"
        ), "{message}");
        assert!(message.contains("The command has 11 lines"), "{message}");
        assert!(message.contains("between the two lines of 4 backticks"), "{message}");
        assert_literal(&mongo_request(statement));
    }

    #[test]
    fn a_script_is_one_question_that_says_how_many_changes_it_holds() {
        let listing = "INSERT INTO items (name) VALUES ('a');\n\nUPDATE orders SET status = 'paid' WHERE id = 1;";
        let mut script = request(listing, Some("main"));
        script.script = Some(2);
        let message = ElicitingApprover::message(&script);
        assert!(message.contains("asks to run a script that changes data"), "{message}");
        assert!(message.contains("the 2 statements below that change data"), "{message}");
        assert!(message.contains("```\nINSERT INTO items"), "{message}");
        assert_literal(&script);
        script.script = Some(1);
        assert!(ElicitingApprover::message(&script).contains("the 1 statement below that changes data"));
    }

    #[test]
    fn a_mongodb_write_gets_the_same_refusals_as_any_other() {
        let command = "{\n  \"insert\": \"orders\"\n}";
        // No settings: refused without asking.
        let off = ElicitingApprover::new(None);
        assert_eq!(
            off.approve(&mongo_request(command), &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(REASON_SETTINGS_UNAVAILABLE)
            }
        );
        assert!(off.check_writable(&mongo_request(command)).is_err());
        let (_dir, path) = switch_on_file();
        let on = ElicitingApprover::new(Some(path));
        // Production: never from MCP.
        let mut production = mongo_request(command);
        production.production = true;
        assert_eq!(
            on.approve(&production, &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(REASON_PRODUCTION)
            }
        );
        // An empty filter that was allowed is a dangerous change: not through MCP on production.
        let mut every_document = mongo_request(command);
        every_document.dangerous = Some("the filter is empty");
        every_document.production = true;
        assert_eq!(
            on.approve(&every_document, &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(REASON_DANGEROUS)
            }
        );
        // A command too long to be reviewed is refused once there is a client to ask.
        let long = format!("{{\n{}}}", "  \"a\": 1,\n".repeat(MAX_LINES));
        assert_eq!(ElicitingApprover::review_problem(&long), Some(REASON_TOO_LONG));
        // With everything in order the only thing missing is a client to ask.
        assert_eq!(
            on.approve(&mongo_request(command), &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: refusal(&no_way_to_ask(None))
            }
        );
        // The settings are read again once the client says yes.
        assert_eq!(on.still_allowed(&mongo_request(command)), Ok(()));
    }

    /// Every text a refusal can have, with the sentence the connection tools add for a removal.
    fn every_reason() -> Vec<String> {
        let mut reasons = vec![
            switch_off(None),
            switch_off(Some("add the connection in the tablex app (New Connection)")),
            settings_unavailable(None),
            settings_unavailable(Some("add the connection in the tablex app (New Connection)")),
            REASON_PRODUCTION.to_owned(),
            no_way_to_ask(None),
            no_way_to_ask(Some(&ClientError::NotRunning)),
            no_way_to_ask(Some(&ClientError::Unsupported)),
            no_way_to_ask(Some(&ClientError::Failed("the tablex socket is open to other users".into()))),
        ];
        reasons.extend(
            [
                REASON_DECLINED_OFTEN,
                REASON_TOO_LONG,
                REASON_HIDDEN_BLANKS,
                REASON_COMMIT_NOT_REVIEWABLE,
                REASON_BUSY,
                REASON_DANGEROUS,
                REASON_NOT_DELIVERED,
            ]
            .map(str::to_owned),
        );
        reasons
    }

    #[test]
    fn every_refusal_ends_with_the_outside_sentence_exactly_once_and_fits_the_clip() {
        for reason in every_reason() {
            assert!(!reason.contains("outside tablex"), "added in one place only: {reason}");
            let text = refusal(&reason);
            assert_eq!(text.matches(OUTSIDE_TABLEX).count(), 1, "{text}");
            assert!(text.ends_with(OUTSIDE_TABLEX), "{text}");
            // The layer clips what the model gets at 2000 characters and adds a prefix of its own.
            assert!(text.chars().count() < 1000, "{} characters: {text}", text.chars().count());
            // Each one names something to ask the user (or to wait for).
            assert!(
                text.contains("Ask the user") || text.contains("ask the user") || text.contains("Wait until") || text.contains("Roll the transaction back"),
                "{text}"
            );
        }
        assert_eq!(
            OUTSIDE_TABLEX,
            "Do not connect to the database outside tablex (for example with the URI or a script); ask the user instead."
        );
        // Calling it twice does not repeat it, and a reason that ends in a full stop gets one only.
        assert_eq!(refusal("a reason."), format!("a reason. {OUTSIDE_TABLEX}"));
    }

    #[test]
    fn the_switch_refusal_names_the_setting_as_the_older_app_shows_it() {
        // The labels are those of the older app that wrote the file (this app has no such switch).
        for label in ["Let MCP clients run changes", "AI Assistant", "MCP server"] {
            assert!(REASON_SWITCH_OFF.contains(label), "{label}");
        }
        let writes = refusal(&switch_off(None));
        assert!(writes.contains("Settings \u{2192} AI Assistant \u{2192} MCP server"), "{writes}");
        assert!(!writes.contains("New Connection"), "{writes}");
        let connections = refusal(&switch_off(Some("add the connection in the tablex app (New Connection)")));
        assert!(
            connections.contains("or ask the user to add the connection in the tablex app (New Connection)"),
            "{connections}"
        );
        assert!(connections.contains("Let MCP clients run changes"), "{connections}");
    }

    #[test]
    fn the_client_that_cannot_ask_is_told_what_the_user_can_do() {
        let not_running = no_way_to_ask(Some(&ClientError::NotRunning));
        assert_eq!(not_running, no_way_to_ask(None));
        assert!(not_running.starts_with("this MCP client cannot show approval questions"), "{not_running}");
        assert!(
            not_running.contains("open the tablex app and try again, or to use a client that supports MCP elicitation"),
            "{not_running}"
        );
        let windows = no_way_to_ask(Some(&ClientError::Unsupported));
        assert!(windows.contains("approval in the tablex app is not available on Windows yet"), "{windows}");
        assert!(windows.contains("client that supports MCP elicitation"), "{windows}");
        assert!(!windows.contains("open the tablex app and try again"), "{windows}");
        let broken = no_way_to_ask(Some(&ClientError::Failed("the tablex socket is open to other users".into())));
        assert!(broken.contains("the tablex socket is open to other users"), "{broken}");
        assert!(broken.contains("client that supports MCP elicitation"), "{broken}");
    }

    #[test]
    fn the_production_too_long_and_declined_refusals_say_what_to_do() {
        for (reason, what) in [
            (REASON_PRODUCTION, "make this change in the tablex app"),
            (REASON_DANGEROUS, "run it in the tablex app"),
            (REASON_TOO_LONG, "run it in the tablex app"),
            (REASON_HIDDEN_BLANKS, "run it in the tablex app"),
            (REASON_COMMIT_NOT_REVIEWABLE, "run the changes in the tablex app"),
            (REASON_DECLINED_OFTEN, "restart it"),
            (REASON_BUSY, "Wait until"),
            (REASON_NOT_DELIVERED, "try again"),
        ] {
            assert!(reason.contains(what), "{reason}");
        }
    }

    #[test]
    fn the_refusals_that_leave_the_approver_all_carry_the_sentence_and_answers_do_not() {
        let ends = |text: &str| assert!(text.ends_with(OUTSIDE_TABLEX), "{text}");
        let off = ElicitingApprover::new(None);
        let write = request("DELETE FROM t WHERE a = 1", None);
        let ApprovalDecision::Refuse { reason } = off.approve(&write, &CancelToken::new()) else {
            panic!("refused");
        };
        ends(&reason);
        ends(&off.check_writable(&write).unwrap_err());
        ends(&off.still_allowed(&write).unwrap_err());
        ends(&off.commit_reviewable(&["x".repeat(MAX_CHARS + 1)]).unwrap_err());
        ends(&off.connection_change_still_allowed("remove the connection in the tablex app").unwrap_err());
        let question = ConnectionQuestion {
            message: String::new(),
            app: ApprovalAsk::default(),
            instead: "remove the connection in the tablex app",
        };
        let ApprovalDecision::Refuse { reason } = off.ask_connection_change(question, &CancelToken::new()) else {
            panic!("refused");
        };
        ends(&reason);
        assert!(reason.contains("or ask the user to remove the connection in the tablex app"), "{reason}");
        let ApprovalDecision::Refuse { reason } = ElicitingApprover::decide(Answer::NotDelivered) else {
            panic!("refused");
        };
        ends(&reason);
        // A user's answer, and a question that got none, are not refusals: no sentence.
        for decision in [
            ElicitingApprover::decide(Answer::Cancelled),
            ElicitingApprover::decide(Answer::NoReply),
            ElicitingApprover::decide(Answer::App { approved: false }),
        ] {
            let text = format!("{decision:?}");
            assert!(!text.contains("outside tablex"), "{text}");
        }
        assert_eq!(ElicitingApprover::decide(Answer::App { approved: true }), ApprovalDecision::Approve);
    }

    #[test]
    fn the_app_question_carries_what_the_elicitation_shows() {
        let mut dangerous = request("DROP TABLE t\u{200B}", Some("main"));
        dangerous.dangerous = Some("DROP permanently removes database objects");
        let ask = ElicitingApprover::app_question(&dangerous);
        assert_eq!(ask.title, "Run this statement");
        assert_eq!(ask.connection.as_deref(), Some("Warehouse"));
        assert_eq!(ask.connection_type.as_deref(), Some("Development"));
        assert_eq!(ask.database.as_deref(), Some("main"));
        assert_eq!(ask.danger.as_deref(), Some("DROP permanently removes database objects"));
        // Characters without a shape of their own are spelled, and said.
        assert_eq!(ask.statements, ["DROP TABLE t<U+200B>"]);
        assert!(ask.notes.iter().any(|note| note.contains("has 1 character without a shape of its own")), "{:?}", ask.notes);
        assert!(ask.check().is_err(), "the id is set when it is sent");
        // The commit lists every change, one block each.
        let changes: Vec<String> = ["INSERT INTO t VALUES (1)", "DELETE FROM t WHERE a = 2"].map(str::to_owned).into();
        let mut commit = request("COMMIT", None);
        commit.transaction = Some(TransactionApproval { id: "tx-1", commit: Some(&changes) });
        let ask = ElicitingApprover::app_question(&commit);
        assert_eq!(ask.title, "Commit this transaction");
        assert_eq!(ask.statements, changes);
        assert!(ask.notes.iter().any(|note| note.contains("transaction tx-1")), "{:?}", ask.notes);
        // A Mongo command and a script name what they are.
        let command = ElicitingApprover::app_question(&mongo_request("{}"));
        assert_eq!((command.title.as_str(), command.collection.as_deref()), ("Run this command", Some("orders")));
    }

    /// What a client's answer can be, and what the agent is told for each. Statements are
    /// generic: nothing here names a real table.
    #[test]
    fn every_kind_of_answer_has_its_own_outcome_and_text() {
        use ElicitationAction::{Accept, Cancel, Decline};
        let deny = |answer: Answer| match ElicitingApprover::decide(answer) {
            ApprovalDecision::Deny { reason } => reason,
            other => panic!("a deny, got {other:?}"),
        };
        // 1. accept with the run value: the only yes.
        assert_eq!(
            ElicitingApprover::decide(reply(Accept, Some(json!({"answer": "run"})))),
            ApprovalDecision::Approve
        );
        // 2. accept with do_not_run: the user chose not to.
        let chose = deny(reply(Accept, Some(json!({"answer": "do_not_run"})))).expect("a reason");
        assert_eq!(chose, chose_not_to(ANSWER_TITLE));
        assert_eq!(chose, "the user chose \"Do not run it\"");
        // 3. accept with no value: the form was submitted as it was.
        let none = deny(reply(Accept, Some(json!({})))).expect("a reason");
        assert_eq!(deny(reply(Accept, None)).as_deref(), Some(none.as_str()));
        assert_eq!(none, no_choice(ANSWER_TITLE));
        assert!(none.contains("accepted without choosing \"Run this statement\" or \"Do not run it\""), "{none}");
        assert!(none.contains("so nothing ran"), "{none}");
        assert!(none.contains("Ask the user whether to go ahead, and tell them to choose \"Run this statement\" and accept"), "{none}");
        assert!(none.contains("counts as a decline"), "{none}");
        assert!(none.contains(&format!("after {MAX_DECLINES} declines")), "{none}");
        // 4. accept with the old boolean shape (or any other value): never a yes, not an answer.
        for old in [json!({"run": true}), json!({"answer": true}), json!({"answer": "true"}), json!({"answer": "Run"})] {
            assert_eq!(
                ElicitingApprover::decide(reply(Accept, Some(old.clone()))),
                match old.get("answer") {
                    None => ApprovalDecision::Deny { reason: Some(none.clone()) },
                    Some(_) => ApprovalDecision::Unanswered { reason: REASON_NO_ANSWER.to_owned() },
                },
                "{old}"
            );
        }
        // 5. a decline: the user did not approve, with nothing to add (the layer says so).
        assert_eq!(deny(reply(Decline, None)), None);
        // 6. a dismissal.
        assert_eq!(deny(reply(Cancel, None)).as_deref(), Some(REASON_DISMISSED));
        // The texts differ, and none carries the refusal sentence (these are answers, not refusals).
        let texts = [chose, none, REASON_DISMISSED.to_owned(), REASON_NO_ANSWER.to_owned()];
        for (n, text) in texts.iter().enumerate() {
            assert!(!text.contains("outside tablex"), "{text}");
            for other in &texts[n + 1..] {
                assert_ne!(text, other);
            }
        }
        // The choice is named by the title of the question: the commit's and the connections' too.
        for (title, no) in [(COMMIT_TITLE, "Do not commit it"), (CONNECTION_TITLE, "Do not make this change")] {
            let ApprovalDecision::Deny { reason: Some(text) } =
                ElicitingApprover::decide_for(reply(Accept, Some(json!({}))), title)
            else {
                panic!("a deny with a reason");
            };
            assert!(text.contains(&format!("choose \"{title}\" and accept")) && text.contains(no), "{text}");
            let ApprovalDecision::Deny { reason: Some(text) } =
                ElicitingApprover::decide_for(reply(Accept, Some(json!({"answer": "do_not_run"}))), title)
            else {
                panic!("a deny with a reason");
            };
            assert_eq!(text, format!("the user chose \"{no}\""));
        }
        // An answer without a choice counts toward the decline budget, like a decline.
        assert!(ElicitingApprover::counts_against_the_connection(&reply(Accept, Some(json!({})))));
    }

    #[test]
    fn the_question_says_to_choose_to_run_it_and_accept_near_the_top() {
        let message = ElicitingApprover::message(&request("CREATE INDEX CONCURRENTLY ix ON items (label)", None));
        let first_paragraph = message.split("\n\n").next().unwrap();
        assert!(first_paragraph.ends_with("To run it, choose \"Run this statement\" and accept."), "{first_paragraph}");
        let mut dangerous = request("DROP TABLE items", None);
        dangerous.dangerous = Some("DROP permanently removes database objects");
        let message = ElicitingApprover::message(&dangerous);
        assert!(message.split("\n\n").next().unwrap().ends_with("To run it, choose \"Run this statement\" and accept."), "{message}");
        let changes: Vec<String> = vec!["INSERT INTO items VALUES (1)".to_owned()];
        let mut commit = request("COMMIT", None);
        commit.transaction = Some(TransactionApproval { id: "tx-1", commit: Some(&changes) });
        let message = ElicitingApprover::message(&commit);
        assert!(message.split("\n\n").next().unwrap().ends_with("To commit, choose \"Commit this transaction\" and accept."), "{message}");
        // The instruction names the option the form shows.
        let schema = serde_json::to_value(ElicitingApprover::schema(false).unwrap()).unwrap();
        assert_eq!(schema["properties"]["answer"]["enumNames"][1], ANSWER_TITLE);
    }

    #[test]
    fn the_app_refusals_have_reasons_and_never_count_as_declines() {
        let refused = |text: &str| ElicitingApprover::decide(Answer::AppRefused(text.to_owned()));
        assert_eq!(
            refused("tablex has too many questions waiting"),
            ApprovalDecision::Refuse { reason: refusal(REASON_BUSY) }
        );
        let ApprovalDecision::Refuse { reason } = refused("tablex has no window to ask in") else {
            panic!("a refusal");
        };
        assert!(reason.contains("tablex has no window to ask in") && reason.ends_with(OUTSIDE_TABLEX), "{reason}");
        assert!(!ElicitingApprover::counts_against_the_connection(&Answer::AppRefused("x".into())));
    }

    #[test]
    fn a_long_client_name_is_cut_after_it_is_spelled_and_never_inside_a_spelled_character() {
        assert_eq!(cut_spelled("short", 80), "short");
        assert_eq!(cut_spelled(&"a".repeat(80), 80), "a".repeat(80));
        assert_eq!(cut_spelled(&"a".repeat(81), 80), format!("{}\u{2026}", "a".repeat(80)));
        // Controls spell out to `<U+XXXX>`: 20 of them are 160 characters, and the cut falls between two.
        let spelled = spell_out(&"\u{7}".repeat(20), false).0;
        assert_eq!(spelled.chars().count(), 20 * "<U+0007>".len());
        let cut = cut_spelled(&spelled, 80);
        assert!(cut.ends_with(">\u{2026}"), "{cut}");
        assert_eq!(cut.trim_end_matches('\u{2026}'), "<U+0007>".repeat(10));
        let odd = cut_spelled(&format!("{}<U+202E>x", "a".repeat(75)), 80);
        assert_eq!(odd, format!("{}\u{2026}", "a".repeat(75)), "a unit that does not fit is dropped whole");
    }

    #[test]
    fn a_dangerous_statement_and_a_commit_are_click_only_in_the_app_and_a_plain_write_is_not() {
        assert!(!ElicitingApprover::app_question(&request("INSERT INTO t VALUES (1)", None)).click_only);
        let mut dangerous = request("DROP TABLE t", None);
        dangerous.dangerous = Some("DROP removes objects");
        assert!(ElicitingApprover::app_question(&dangerous).click_only);
        let changes: Vec<String> = vec!["INSERT INTO t VALUES (1)".to_owned()];
        let mut commit = request("COMMIT", None);
        commit.transaction = Some(TransactionApproval { id: "tx-1", commit: Some(&changes) });
        assert!(ElicitingApprover::app_question(&commit).click_only);
    }
}
