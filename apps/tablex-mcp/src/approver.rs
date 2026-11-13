//! The write approver of `tablex-mcp`: a statement that changes data is put to the user through MCP
//! **elicitation** (`elicitation/create`), and runs only when the client answers `accept` with the
//! boolean `run` set to `true` while the app's switch is still on.
//!
//! The client is the user's own agent host and may answer without showing the question; TableX
//! cannot see that. So conditions must all hold before anything is asked, checked in this order,
//! each a [`ApprovalDecision::Refuse`] with a fixed reason the model can act on:
//! 1. the switch "Let MCP clients run changes" in the app's Settings → AI is on (read from
//!    `settings.json` at the time of each write, and **again when the client says yes**: a switch
//!    turned off while a question waited stops that write; unreadable = off);
//! 2. the connection is not of a production type;
//! 3. the client declared the elicitation capability (form mode);
//! 4. the statement is small enough to be reviewed, as shown ([`MAX_LINES`], [`MAX_CHARS`],
//!    [`MAX_LINE_CHARS`]);
//! 5. the connection's declines plus its open questions stay under [`MAX_DECLINES`] (a decline is
//!    a no, a dismissal, no usable answer, or the call cancelled after the question was sent;
//!    counted for this server session), and fewer than [`MAX_OPEN_QUESTIONS`] questions are open in all.
//!
//! Dangerous statements never get here (`allows_dangerous` is false: the layer refuses them); the
//! approver refuses one itself as well.
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
    ApprovalDecision, CancelToken, Language, TransactionApproval, WriteApproval, WriteApprover,
};
use engine_connect::profile::mcp_writes_enabled;
use rmcp::model::{
    ClientResult, ElicitRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
    ElicitationSchema, ServerRequest,
};
use rmcp::service::{ElicitationMode, Peer, PeerRequestOptions, RoleServer};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// Refusal: the app's switch is off (or its file cannot be read), when asking or when the client
/// said yes.
pub const REASON_SWITCH_OFF: &str =
    "changes through the MCP server are turned off in TableX: Settings → AI";
/// Refusal: the connection's type is a production one.
pub const REASON_PRODUCTION: &str =
    "a production connection can only be changed from the TableX app";
/// Refusal: the client did not declare the elicitation capability.
pub const REASON_NO_ELICITATION: &str = "this MCP client cannot ask the user";
/// Refusal: [`MAX_DECLINES`] questions about the connection ended in a no or in no usable answer.
pub const REASON_DECLINED_OFTEN: &str =
    "declined, or left without a usable answer, several times in this server session; changes \
     to this connection through MCP are off until the MCP server is restarted";
/// Refusal: too big to be reviewed in a question.
pub const REASON_TOO_LONG: &str = "too long to be shown for approval through MCP (more than 200 \
     lines, 8000 characters in all or 400 in one line); the user can run it in the TableX app";
/// Refusal: a line hides text behind a run of blanks.
pub const REASON_HIDDEN_BLANKS: &str = "a line has a run of more than 24 spaces or tabs (a tab counts \
     as 8) followed by more text, which a code block can hide off-screen; the user can run it in the \
     TableX app";
/// Refusal: the writes of a transaction cannot be reviewed in one commit question.
pub const REASON_COMMIT_NOT_REVIEWABLE: &str = "the changes in this transaction are too long to be \
     shown for approval through MCP (more than 200 lines, 8000 characters in all or 400 in one line, \
     or a line hides text behind a run of blanks); roll the transaction back, the user can run the \
     changes in the TableX app";
/// Refusal: [`MAX_OPEN_QUESTIONS`] questions are open.
pub const REASON_BUSY: &str = "other changes are waiting for the user's answer";
/// Refusal: the layer should never send a dangerous statement here.
pub const REASON_DANGEROUS: &str =
    "dangerous statements are never run through MCP; the user can run it in the TableX app";
/// Refusal: the question did not reach the client.
pub const REASON_NOT_DELIVERED: &str = "the question could not be delivered to the MCP client";
/// Not approved: the question was sent but nothing usable came back.
pub const REASON_NO_ANSWER: &str = "no usable answer came back from the MCP client";
/// The reason in the audit line and for the model when the user dismissed the question.
pub const REASON_DISMISSED: &str = "the question was dismissed";
/// The reason when the call was cancelled while the question was open (the layer turns a decision
/// under a cancelled token into its own `Cancelled`).
const REASON_CANCELLED: &str = "cancelled";

/// The one property of the requested schema.
pub const ANSWER_PROPERTY: &str = "run";
/// Its title, as the client shows it next to the control.
pub const ANSWER_TITLE: &str = "Run this statement";
/// The title when the question is the COMMIT of a transaction.
pub const COMMIT_TITLE: &str = "Commit this transaction";

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
    /// `settings_path`: the app's `settings.json` (`None`: unknown, so the switch reads as off).
    pub fn new(settings_path: Option<PathBuf>) -> Self {
        Self {
            settings_path,
            budgets: Mutex::default(),
            open: AtomicUsize::new(0),
        }
    }

    fn switch_on(&self) -> bool {
        mcp_writes_enabled(self.settings_path.as_deref())
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
        let noun = match request.language {
            Language::Redis => "command",
            Language::Sql | Language::Other => "statement",
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
        let mut message = format!(
            "An MCP client asks to run a {noun} that changes data. Nothing runs unless you accept.\n\n\
             Connection: {} ({connection_type})\n\n",
            label(request.connection_name),
        );
        if let Some(database) = request.database {
            message.push_str(&format!("Database: {}\n\n", label(database)));
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
             unless you accept.\n\nConnection: {} ({connection_type})\n\nTransaction: {}\n\n",
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

    /// The requested schema: one required boolean, [`ANSWER_PROPERTY`], whose default is `false`
    /// (a client that fills in defaults declines).
    fn schema(commit: bool) -> Option<ElicitationSchema> {
        let title = if commit { COMMIT_TITLE } else { ANSWER_TITLE };
        ElicitationSchema::builder()
            .required_bool_with(ANSWER_PROPERTY, |boolean| {
                boolean.title(title).with_default(false)
            })
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
            Answer::Cancelled | Answer::NoReply => return true,
            Answer::NotDelivered => return false,
        };
        match result.action {
            ElicitationAction::Accept => !matches!(
                result.content.as_ref().and_then(|c| c.get(ANSWER_PROPERTY)),
                Some(Value::Bool(true))
            ),
            // A decline, a dismissal, or an action this build does not know.
            _ => true,
        }
    }

    /// What the client's reply means. Only `accept` with `run: true` approves.
    pub fn decide(reply: Answer) -> ApprovalDecision {
        let deny = |reason: Option<&str>| ApprovalDecision::Deny {
            reason: reason.map(str::to_owned),
        };
        let unanswered = || ApprovalDecision::Unanswered {
            reason: REASON_NO_ANSWER.to_owned(),
        };
        match reply {
            Answer::Cancelled => deny(Some(REASON_CANCELLED)),
            Answer::NotDelivered => ApprovalDecision::Refuse {
                reason: REASON_NOT_DELIVERED.to_owned(),
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
                    Some(Value::Bool(true)) => ApprovalDecision::Approve,
                    Some(Value::Bool(false)) => deny(None),
                    // `accept` without the answer, or with something that is not a boolean.
                    _ => unanswered(),
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

impl ElicitingApprover {
    /// Conditions 1-3 of the module docs (and: no dangerous statement), in that order. The caller
    /// that can be asked, or the fixed reason a write is refused without asking.
    fn preflight(&self, request: &WriteApproval<'_>) -> Result<Caller, &'static str> {
        if request.dangerous.is_some() {
            return Err(REASON_DANGEROUS);
        }
        if !self.switch_on() {
            return Err(REASON_SWITCH_OFF);
        }
        if request.production {
            return Err(REASON_PRODUCTION);
        }
        Caller::current()
            .filter(Caller::can_ask)
            .ok_or(REASON_NO_ELICITATION)
    }
}

impl WriteApprover for ElicitingApprover {
    fn approve(&self, request: &WriteApproval<'_>, cancel: &CancelToken) -> ApprovalDecision {
        let refuse = |reason: &str| ApprovalDecision::Refuse {
            reason: reason.to_owned(),
        };
        let caller = match self.preflight(request) {
            Ok(caller) => caller,
            Err(reason) => return refuse(reason),
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
        let Some(schema) = Self::schema(commit.is_some()) else {
            return refuse(REASON_NOT_DELIVERED);
        };
        let params = ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: Self::message(request),
            requested_schema: schema,
        };
        let answer = caller.ask(params, cancel);
        if Self::counts_against_the_connection(&answer) {
            self.record_decline(request.connection_id);
        }
        // The place is given back after the decline is counted, so the two never both miss it.
        drop(open);
        Self::decide(answer)
    }

    /// The commit question lists every write, so the whole list has to pass the same review as
    /// one statement does (lines, characters, line width, hidden blanks).
    fn commit_reviewable(&self, statements: &[String]) -> Result<(), String> {
        match Self::review_problem(&statements.join("\n")) {
            Some(_) => Err(REASON_COMMIT_NOT_REVIEWABLE.to_owned()),
            None => Ok(()),
        }
    }

    /// Before a transaction is begun: the reasons a write would be refused without asking.
    fn check_writable(&self, request: &WriteApproval<'_>) -> Result<(), String> {
        self.preflight(request)
            .map(drop)
            .map_err(str::to_owned)
    }

    /// The question may have waited a long time: the layer asks again, after a yes, whether the
    /// switch is still on, so turning it off in the app stops a write that was asked while it was
    /// on (audited `approved, then refused: ...`).
    fn still_allowed(&self, _request: &WriteApproval<'_>) -> Result<(), String> {
        if self.switch_on() {
            Ok(())
        } else {
            Err(REASON_SWITCH_OFF.to_owned())
        }
    }

    // Dangerous statements are refused by the layer without asking.
    fn allows_dangerous(&self) -> bool {
        false
    }
}

/// What the exchange with the client came to.
#[derive(Debug)]
pub enum Answer {
    /// The client replied.
    Reply(ElicitResult),
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
            statement,
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
            "An MCP client asks to run a statement that changes data. Nothing runs unless you accept.\n\n\
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
            "An MCP client asks to run a statement that changes data. Nothing runs unless you accept.\n\n\
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
    fn only_accept_with_run_true_approves() {
        use ElicitationAction::{Accept, Cancel, Decline};
        let decide = ElicitingApprover::decide;
        assert_eq!(
            decide(reply(Accept, Some(json!({"run": true})))),
            ApprovalDecision::Approve
        );
        // A person's no.
        for no in [
            reply(Accept, Some(json!({"run": false}))),
            reply(Decline, None),
            reply(Decline, Some(json!({"run": true}))),
            reply(Cancel, Some(json!({"run": true}))),
        ] {
            assert!(ElicitingApprover::counts_against_the_connection(&no));
            assert!(matches!(decide(no), ApprovalDecision::Deny { .. }));
        }
        // Nothing usable: not a yes either, and it counts like a decline (a client that turns a
        // dismissal into an error must not make asking free).
        for odd in [
            reply(Accept, None),
            reply(Accept, Some(json!({}))),
            reply(Accept, Some(json!({"run": "true"}))),
            reply(Accept, Some(json!({"run": 1}))),
            reply(Accept, Some(json!({"run": null}))),
            reply(Accept, Some(json!({"other": true}))),
            reply(Accept, Some(json!("run"))),
            reply(Accept, Some(json!([true]))),
            Answer::NoReply,
        ] {
            assert!(ElicitingApprover::counts_against_the_connection(&odd));
            assert_eq!(
                decide(odd),
                ApprovalDecision::Unanswered {
                    reason: REASON_NO_ANSWER.to_owned()
                }
            );
        }
        assert!(matches!(
            decide(Answer::NotDelivered),
            ApprovalDecision::Refuse { .. }
        ));
        // A question that never reached the client is nobody's answer.
        assert!(!ElicitingApprover::counts_against_the_connection(
            &Answer::NotDelivered
        ));
        assert!(!ElicitingApprover::counts_against_the_connection(&reply(
            Accept,
            Some(json!({"run": true}))
        )));
        assert!(matches!(
            decide(Answer::Cancelled),
            ApprovalDecision::Deny { .. }
        ));
    }

    #[test]
    fn the_requested_schema_is_one_required_boolean_that_defaults_to_no() {
        let schema = serde_json::to_value(ElicitingApprover::schema(false).unwrap()).unwrap();
        assert_eq!(
            schema,
            json!({
                "type": "object",
                "properties": {
                    "run": {"type": "boolean", "title": "Run this statement", "default": false}
                },
                "required": ["run"]
            })
        );
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
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"schema_version": 4, "ai": {"mcp_writes": true}}"#,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn a_call_without_a_client_is_refused_even_when_the_switch_is_on() {
        let (_dir, path) = switch_on_file();
        let approver = ElicitingApprover::new(Some(path));
        assert_eq!(
            approver.approve(
                &request("DELETE FROM t WHERE a = 1", None),
                &CancelToken::new()
            ),
            ApprovalDecision::Refuse {
                reason: REASON_NO_ELICITATION.to_owned()
            }
        );
    }

    #[test]
    fn a_dangerous_request_is_refused_by_the_approver_itself() {
        let (_dir, path) = switch_on_file();
        let approver = ElicitingApprover::new(Some(path));
        let mut dangerous = request("DELETE FROM t", None);
        dangerous.dangerous = Some("DELETE without WHERE");
        assert_eq!(
            approver.approve(&dangerous, &CancelToken::new()),
            ApprovalDecision::Refuse {
                reason: REASON_DANGEROUS.to_owned()
            }
        );
        assert!(!approver.allows_dangerous());
    }
}
