//! GPUI adapter of the editor autocomplete core (`crate::completion`).
//!
//! A [`TabCompletion`] is installed as the `CompletionProvider` of one editor.
//! When the menu asks for items it snapshots a bounded window of the text,
//! then reads the tab, the navigator cache and the result columns of
//! [`AppRoot`] in a foreground task (after the keystroke's own update has
//! ended, so the root is never read while it is being updated). Only caches
//! that are already loaded are consulted: no I/O, no session lock.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{App, Context, Focusable, Task, WeakEntity, Window};
use gpui_component::input::{CompletionProvider, EditorState, InputEditsEvent, Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    CompletionTriggerKind, InsertTextFormat, Range as LspRange, TextEdit,
};

use super::AppRoot;
use crate::catalog::TableProperties;
use crate::completion::{
    self, Catalog, Column, Completions, Editor, ForeignKey, Fragment, Kind, MongoEditor, MongoRole,
    Relation, SqlEditor, SqlFlavor,
};
use crate::connections::ConnectionKind;
use crate::navigator::{NavigatorNodeId, NavigatorState, ObjectGroup};
use crate::properties_model::PropertiesKey;
use crate::workspace::{QueryModeExt, Tab};

/// Text kept around the cursor: statements and shell scripts that matter for
/// completion are far smaller; a huge script is cut at a line boundary.
const WINDOW_BEFORE: usize = 48 * 1024;
const WINDOW_AFTER: usize = 8 * 1024;

/// Which editor of a tab a provider serves.
#[derive(Clone, Copy, Debug)]
pub(super) enum Role {
    /// The query editor: SQL, or a Mongo shell script on Mongo tabs.
    Query,
    /// The `WHERE` (or, with `order_by`, `ORDER BY`) fragment editor of a SQL table-data tab.
    TableFragment { order_by: bool },
    /// Expanded Mongo filter / sort (strict JSON) editors.
    Filter,
    Sort,
    /// The aggregation pipeline text editor.
    Pipeline,
    /// The body editor of one pipeline stage.
    Stage(u64),
}

pub(super) struct TabCompletion {
    root: WeakEntity<AppRoot>,
    tab: u64,
    role: Role,
    /// The editor this provider serves, set by [`EditorCompletion::completion`].
    editor: Option<WeakEntity<EditorState>>,
    /// The last answer's items that leave the caret inside what they insert.
    calls: Rc<RefCell<Vec<CaretStop>>>,
}

/// An offered insertion that puts the caret `back` bytes before its end:
/// `sum()` with the caret between the parentheses.
struct CaretStop {
    /// Where the inserted text starts.
    start: usize,
    text: String,
    back: usize,
}

impl TabCompletion {
    /// Provider for the `role` editor of `tab`, created from the root's context.
    pub(super) fn for_tab(cx: &gpui::Context<AppRoot>, tab: u64, role: Role) -> Self {
        Self {
            root: cx.entity().downgrade(),
            tab,
            role,
            editor: None,
            calls: Rc::default(),
        }
    }
}

/// The "suggestions while typing" setting, published for the providers
/// (`AppRoot::apply_settings` sets it).
pub(super) struct CompletionPrefs {
    pub(super) auto_trigger: bool,
}

impl gpui::Global for CompletionPrefs {}

/// Builder hook, called inside `cx.new`: `EditorState::new(..).language(..).completion(provider, window, cx)`.
pub(super) trait EditorCompletion: Sized {
    fn completion(
        self,
        provider: TabCompletion,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> Self;
}

impl EditorCompletion for EditorState {
    fn completion(
        mut self,
        mut provider: TabCompletion,
        window: &mut Window,
        cx: &mut Context<EditorState>,
    ) -> Self {
        provider.editor = Some(cx.weak_entity());
        // The pinned editor takes `insert_text_format` SNIPPET as plain text and has no tab stops,
        // and its menu accepts an item with a bare text edit: the caret lands after the inserted
        // text. An item that wants it elsewhere (`sum(|)`) is recognised by the edit it causes.
        let calls = provider.calls.clone();
        cx.subscribe_in(
            &cx.entity(),
            window,
            move |editor, _, event: &InputEditsEvent, window, cx| {
                place_caret(editor, &calls, event, window, cx);
            },
        )
        .detach();
        self.lsp_mut().completion_provider = Some(Rc::new(provider));
        self
    }
}

/// Moves the caret into the text of an accepted completion that asked for it. Whether an edit is
/// that completion is told by where it starts and what it inserts, so a stale offer matches
/// nothing but the same text at the same place.
fn place_caret(
    editor: &mut EditorState,
    calls: &RefCell<Vec<CaretStop>>,
    event: &InputEditsEvent,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let mut calls = calls.borrow_mut();
    let [(edit, after)] = event.edits() else { return };
    let end = edit.new_end_byte;
    let Some(stop) = calls.iter().find(|stop| {
        stop.start == edit.start_byte
            && end == stop.start + stop.text.len()
            && after.slice(stop.start..end) == stop.text
    }) else {
        return;
    };
    if editor.cursor() == end {
        editor.set_cursor_position(after.offset_to_position(end - stop.back), window, cx);
    }
    calls.clear();
}

/// The items of `answer` whose insertion asks for a caret position, with where they start.
fn caret_stops(answer: &[CompletionItem], rope: &Rope) -> Vec<CaretStop> {
    answer
        .iter()
        .filter_map(|item| {
            let back = usize::try_from(item.data.as_ref()?.as_u64()?).ok()?;
            let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else { return None };
            Some(CaretStop {
                start: rope.position_to_offset(&edit.range.start),
                text: edit.new_text.clone(),
                back,
            })
        })
        .collect()
}

impl CompletionProvider for TabCompletion {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        context: CompletionContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<gpui::Result<CompletionResponse>> {
        let request = Request {
            root: self.root.clone(),
            tab: self.tab,
            role: self.role,
            editor: self.editor.clone(),
            calls: self.calls.clone(),
            window: window.window_handle(),
            rope: text.clone(),
            offset,
            // Ctrl+Space (`show_completions`), as opposed to a typed character.
            manual: context.trigger_kind == CompletionTriggerKind::INVOKED,
            progress: Rc::default(),
        };
        cx.spawn(async move |cx| Ok(CompletionResponse::Array(request.answer(cx).await)))
    }

    /// Any single typed character (or a deletion) re-queries; the core decides
    /// whether there is anything to show, which also closes a stale menu.
    fn is_completion_trigger(&self, _: usize, new_text: &str, cx: &mut App) -> bool {
        // A global, not a read of the root: the editor may type into itself
        // from inside a root update.
        cx.try_global::<CompletionPrefs>()
            .is_none_or(|prefs| prefs.auto_trigger)
            && new_text.chars().nth(1).is_none()
    }
}

/// One completion request: where in which editor, and how it was asked for.
#[derive(Clone)]
struct Request {
    root: WeakEntity<AppRoot>,
    tab: u64,
    role: Role,
    editor: Option<WeakEntity<EditorState>>,
    calls: Rc<RefCell<Vec<CaretStop>>>,
    window: gpui::AnyWindowHandle,
    /// The document and caret the request was made at.
    rope: Rope,
    offset: usize,
    /// Asked for by hand (Ctrl+Space), not by typing.
    manual: bool,
    /// Shared by the clones of one request: whether it is between starting the loads its answer
    /// lacked and returning its answer, and whether what it waits for arrived in that time.
    progress: Rc<Cell<Progress>>,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Progress {
    #[default]
    Idle,
    /// Starting loads: one that is answered at once is picked up by the answer being made.
    Starting,
    /// All that was waited for arrived while starting.
    Arrived,
}

/// Times an answer is made again because what it waited for arrived at once.
const MAX_ANSWER_ROUNDS: usize = 6;

impl Request {
    /// The items to show. Reads the caches in a foreground task (after the keystroke's own update
    /// has ended, so the root is never read while it is being updated), starts the loads the
    /// answer lacked, and inserts a single proposal asked for by hand right away (then nothing is shown).
    async fn answer(&self, cx: &mut gpui::AsyncApp) -> Vec<CompletionItem> {
        let (base, window_text) = window_around(&self.rope, self.offset);
        let mut rounds = 0;
        let items = loop {
            let (items, warm) = self
                .root
                .read_with(cx, |root, _| {
                    let at = Snapshot {
                        text: &window_text,
                        cursor: self.offset - base,
                        base,
                        rope: &self.rope,
                        manual: self.manual,
                    };
                    suggest(root, self.tab, self.role, at)
                })
                .ok()
                .flatten()
                .unwrap_or_default();
            // Also when nothing is lacking: a newer request replaces what the last one waits for.
            self.progress.set(Progress::Starting);
            self.root.update(cx, |root, cx| root.warm_columns(warm, self, cx)).ok();
            // A load that answered at once (a cached one) is part of this answer: asking again here
            // beats a separate refresh, which would be shown before this answer replaces it.
            if self.progress.replace(Progress::Idle) != Progress::Arrived {
                break items;
            }
            rounds += 1;
            if rounds > MAX_ANSWER_ROUNDS {
                self.clone().refresh_later(cx);
                break items;
            }
        };
        self.calls.replace(caret_stops(&items, &self.rope));
        if let (true, [only], Some(editor)) = (self.manual, items.as_slice(), &self.editor) {
            let inserted = self
                .window
                .update(cx, |_, window, cx| {
                    editor.update(cx, |editor, cx| {
                        let unchanged = self.is_current(editor);
                        if unchanged {
                            editor.insert_completion(only, self.offset..self.offset, window, cx);
                        }
                        unchanged
                    })
                })
                .is_ok_and(|updated| updated.unwrap_or(false));
            if inserted {
                return Vec::new();
            }
        }
        items
    }

    /// The editor still shows the document with the caret where the request was made.
    fn is_current(&self, editor: &EditorState) -> bool {
        editor.cursor() == self.offset && *editor.text() == self.rope
    }

    /// Answers the request again, now that what it waited for is loaded, and shows the answer
    /// the way the first one would have been: a menu that is open takes the new items, a
    /// request by hand with one proposal inserts it. Nothing happens once the editor lost the
    /// focus, the caret moved or the text changed. What arrived while the answer was still
    /// being made goes into that answer instead.
    fn refresh(self, cx: &mut Context<AppRoot>) {
        if self.progress.get() == Progress::Starting {
            self.progress.set(Progress::Arrived);
            return;
        }
        cx.spawn(async move |_, cx| self.show_again(cx).await).detach();
    }

    fn refresh_later(self, cx: &mut gpui::AsyncApp) {
        cx.spawn(async move |cx| self.show_again(cx).await).detach();
    }

    async fn show_again(self, cx: &mut gpui::AsyncApp) {
        let Some(editor) = self.editor.clone() else { return };
        let items = self.answer(cx).await;
        self.window
            .update(cx, |_, window, cx| {
                editor
                    .update(cx, |editor, cx| {
                        if !self.is_current(editor) || !editor.focus_handle(cx).is_focused(window) {
                            return;
                        }
                        // The word the menu is for, as `show_completions` finds it.
                        let word = editor
                            .text()
                            .chars_at(self.offset)
                            .reversed()
                            .take_while(|ch| ch.is_alphanumeric() || *ch == '_')
                            .map(char::len_utf8)
                            .sum::<usize>();
                        let start = self.offset - word;
                        let query = editor.text().slice(start..self.offset).to_string();
                        editor.present_completion_items(start, query, items, cx);
                    })
                    .ok();
            })
            .ok();
    }
}

/// `(window start, window text)` around `offset`, cut at char boundaries and
/// starting on a line boundary when the document is cut.
fn window_around(text: &Rope, offset: usize) -> (usize, String) {
    let mut start = offset.saturating_sub(WINDOW_BEFORE);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (offset + WINDOW_AFTER).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut window = text.slice(start..end).to_string();
    if start > 0 {
        if let Some(newline) = window
            .find('\n')
            .filter(|&newline| start + newline < offset)
        {
            window.drain(..=newline);
            start += newline + 1;
        }
    }
    (start, window)
}

fn flavor_of(kind: Option<ConnectionKind>) -> SqlFlavor {
    match kind {
        Some(ConnectionKind::Sqlite) => SqlFlavor::Sqlite,
        Some(ConnectionKind::Mysql) => SqlFlavor::Mysql,
        Some(ConnectionKind::Mariadb) => SqlFlavor::Mariadb,
        Some(ConnectionKind::Clickhouse) => SqlFlavor::Clickhouse,
        Some(ConnectionKind::Duckdb) => SqlFlavor::Duckdb,
        Some(ConnectionKind::Mssql) => SqlFlavor::Mssql,
        _ => SqlFlavor::Postgres,
    }
}

/// Tables in scope whose columns or foreign keys are not cached yet, and the schemas whose table
/// lists are not.
#[derive(Default)]
struct Warm {
    connection: String,
    objects: Option<ObjectNeed>,
    nodes: Vec<NavigatorNodeId>,
    foreign_keys: Vec<PropertiesKey>,
}

/// The table and view lists a completion needs: of the default schema and of the schemas it names.
struct ObjectNeed {
    connection: String,
    /// Tab database; empty = every loaded database.
    database: String,
    default_namespace: Option<String>,
    /// `None`: the default schema.
    namespaces: Vec<Option<String>>,
}

impl ObjectNeed {
    fn wants(&self, namespace: &str) -> bool {
        self.namespaces.iter().any(|wanted| match wanted {
            Some(wanted) => wanted.eq_ignore_ascii_case(namespace),
            None => self.default_namespace.as_deref().is_some_and(|default| default.eq_ignore_ascii_case(namespace)),
        })
    }

    /// The nodes to load next, without the sidebar: the first level of the path
    /// connection > database > schema > Tables / Views that is not loaded yet. Once those are
    /// answered it is called again for the level below. Both tree shapes: the typed SQL tree and
    /// provider nodes of kind `database` (tables below them).
    fn frontier(&self, nav: &NavigatorState) -> Vec<NavigatorNodeId> {
        let root = NavigatorNodeId::Connection { connection: self.connection.clone() };
        let Some(databases) = nav.loaded_children(&root) else {
            return vec![root];
        };
        let wants_database = |name: &str| self.database.is_empty() || self.database == name;
        let mut next = Vec::new();
        for database in databases {
            match database {
                NavigatorNodeId::Database { connection, database: name, .. }
                    if wants_database(name) && !nav.database_denied(database) =>
                {
                    let Some(schemas) = nav.loaded_children(database) else {
                        next.push(database.clone());
                        continue;
                    };
                    for schema in schemas {
                        let NavigatorNodeId::Schema { schema, .. } = schema else { continue };
                        if !self.wants(schema) {
                            continue;
                        }
                        for group in [ObjectGroup::Tables, ObjectGroup::Views] {
                            let group = NavigatorNodeId::ObjectGroup {
                                connection: connection.clone(),
                                database: name.clone(),
                                schema: schema.clone(),
                                group,
                            };
                            if nav.loaded_children(&group).is_none() {
                                next.push(group);
                            }
                        }
                    }
                }
                NavigatorNodeId::Provider { .. } => {
                    let Some(node) = nav.provider_node(database).filter(|node| node.kind == "database") else {
                        continue;
                    };
                    let name = node.payload.get("database").and_then(|name| name.as_str());
                    if name.is_some_and(|name| wants_database(name) && self.wants(name))
                        && nav.loaded_children(database).is_none()
                    {
                        next.push(database.clone());
                    }
                }
                _ => {}
            }
        }
        next
    }
}

/// Most nodes the loads that one request starts may total, the chain of levels included.
const MAX_OBJECT_LOADS: usize = 8;

/// What the last request that started background loads waits for.
struct CompletionWait {
    request: Request,
    objects: Option<ObjectNeed>,
    nodes: Vec<NavigatorNodeId>,
    keys: Vec<PropertiesKey>,
    /// Loads the chain may still start.
    budget: usize,
}

/// Background loads that completion started, and the request waiting for them.
#[derive(Default)]
pub(super) struct CompletionLoads {
    /// Navigator nodes in flight that completion started (not the ones a sidebar click started).
    pub(super) started: std::collections::HashSet<NavigatorNodeId>,
    wait: Option<CompletionWait>,
}

#[cfg(test)]
impl CompletionLoads {
    /// No load of completion is in flight and no request waits for one.
    pub(super) fn is_idle(&self) -> bool {
        self.started.is_empty() && self.wait.is_none()
    }
}

/// One foreign key, names unqualified: `table(columns)` references `referenced(referenced_columns)`.
pub(super) struct StoredForeignKey {
    pub(super) name: String,
    pub(super) table: String,
    pub(super) columns: Vec<String>,
    pub(super) referenced: String,
    pub(super) referenced_columns: Vec<String>,
}

pub(super) enum ForeignKeys {
    Loading,
    Failed,
    Ready(Vec<StoredForeignKey>),
}

/// The foreign keys of the tables the completion asked about, loaded in the background the way
/// the Properties view loads them (`describe_object`) and consulted without I/O. Never loaded
/// for a table nobody joins: only a `JOIN` or `ON` position asks.
#[derive(Default)]
pub(super) struct ForeignKeyCache(pub(super) HashMap<PropertiesKey, ForeignKeys>);

impl ForeignKeyCache {
    /// Drops what is cached for the tables `covered` selects (a sidebar Refresh).
    pub(super) fn forget(&mut self, covered: impl Fn(&PropertiesKey) -> bool) {
        self.0.retain(|key, _| !covered(key));
    }
}

/// `schema.table` / `"schema"."table"` as the bare table name.
fn bare_name(name: &str) -> &str {
    let last = name.rsplit('.').next().unwrap_or(name).trim();
    last.trim_matches(|c| matches!(c, '"' | '`' | '[' | ']'))
}

/// What a table's properties say about its keys: the ones it declares and the ones that point at it.
fn foreign_keys_of(properties: &TableProperties, object: &str) -> Vec<StoredForeignKey> {
    let declared = properties.foreign_keys.iter().flatten();
    let pointing = properties.references.iter().flatten();
    declared
        .chain(pointing)
        .map(|fk| StoredForeignKey {
            name: fk.name.clone(),
            table: Some(bare_name(&fk.table)).filter(|name| !name.is_empty()).unwrap_or(object).to_owned(),
            columns: fk.columns.clone(),
            referenced: bare_name(&fk.referenced_table).to_owned(),
            referenced_columns: fk.referenced_columns.clone(),
        })
        .collect()
}

/// Most column loads one completion may start.
const MAX_WARM: usize = 4;

impl AppRoot {
    /// Loads, through the regular navigator load path (no sidebar expansion), what the answer to
    /// `request` lacked: the table lists, the columns and the foreign keys of the tables the
    /// user is querying. The request is answered again when they have all arrived
    /// ([`Self::completion_settled`]). Only for connections that are already connected.
    fn warm_columns(&mut self, warm: Warm, request: &Request, cx: &mut Context<Self>) {
        self.completion_loads.wait = None;
        if !self.sessions.is_connected(&warm.connection) {
            return;
        }
        let mut wait = CompletionWait {
            request: request.clone(),
            objects: warm.objects,
            nodes: Vec::new(),
            keys: Vec::new(),
            budget: MAX_OBJECT_LOADS,
        };
        let frontier = wait.objects.as_ref().map(|need| need.frontier(&self.navigator)).unwrap_or_default();
        let nodes = frontier.into_iter().take(MAX_WARM).chain(warm.nodes.into_iter().take(MAX_WARM));
        let requests = self.start_completion_loads(&mut wait, nodes);
        for key in warm.foreign_keys.into_iter().take(MAX_WARM) {
            self.load_foreign_keys(key.clone(), cx);
            if matches!(self.completion_foreign_keys.0.get(&key), Some(ForeignKeys::Loading)) {
                wait.keys.push(key);
            }
        }
        if wait.nodes.is_empty() && wait.keys.is_empty() {
            return;
        }
        self.completion_loads.wait = Some(wait);
        // After the wait is in place: a load may answer at once.
        for request in requests {
            self.ensure_connected_then_load(request, cx);
        }
    }

    /// Begins the loads of `nodes` that nothing else loads, and notes that `wait` waits for them
    /// and for those of them that completion started earlier and are still in flight.
    fn start_completion_loads(
        &mut self,
        wait: &mut CompletionWait,
        nodes: impl Iterator<Item = NavigatorNodeId>,
    ) -> Vec<crate::navigator::LoadRequest> {
        let mut requests = Vec::new();
        for node in nodes {
            if wait.nodes.contains(&node) {
                continue;
            }
            if self.completion_loads.started.contains(&node) {
                wait.nodes.push(node);
            } else if wait.budget > 0 {
                if let Some(request) = self.navigator.begin_background_load(&node) {
                    wait.budget -= 1;
                    self.completion_loads.started.insert(node.clone());
                    wait.nodes.push(node);
                    requests.push(request);
                }
            }
        }
        requests
    }

    /// A load that completion started has been answered (or failed): starts the next level of the
    /// table lists, and once nothing is awaited any more answers the request again.
    pub(super) fn completion_node_loaded(&mut self, node: &NavigatorNodeId, cx: &mut Context<Self>) {
        if !self.completion_loads.started.remove(node) {
            return;
        }
        let Some(mut wait) = self.completion_loads.wait.take() else { return };
        wait.nodes.retain(|waiting| waiting != node);
        let requests = if wait.nodes.is_empty() {
            let frontier = wait.objects.as_ref().map(|need| need.frontier(&self.navigator)).unwrap_or_default();
            self.start_completion_loads(&mut wait, frontier.into_iter().take(MAX_WARM))
        } else {
            Vec::new()
        };
        self.completion_settle(wait, requests, cx);
    }

    /// The foreign keys of `key`'s table have been answered (or failed).
    fn completion_keys_loaded(&mut self, key: &PropertiesKey, cx: &mut Context<Self>) {
        let Some(mut wait) = self.completion_loads.wait.take() else { return };
        wait.keys.retain(|waiting| waiting != key);
        self.completion_settle(wait, Vec::new(), cx);
    }

    fn completion_settle(
        &mut self,
        wait: CompletionWait,
        requests: Vec<crate::navigator::LoadRequest>,
        cx: &mut Context<Self>,
    ) {
        if wait.nodes.is_empty() && wait.keys.is_empty() {
            wait.request.refresh(cx);
            return;
        }
        self.completion_loads.wait = Some(wait);
        for request in requests {
            self.ensure_connected_then_load(request, cx);
        }
    }

    /// Fetches the foreign keys of `key`'s table once, off the UI thread; the request that
    /// asked for them is answered again with them.
    fn load_foreign_keys(&mut self, key: PropertiesKey, cx: &mut Context<Self>) {
        if self.completion_foreign_keys.0.contains_key(&key) {
            return;
        }
        if !self.driver_describes_tables(&key.connection) {
            self.completion_foreign_keys.0.insert(key, ForeignKeys::Failed);
            return;
        }
        self.completion_foreign_keys.0.insert(key.clone(), ForeignKeys::Loading);
        self.describe_table(key.clone(), cx, move |this, result, cx| {
            let state = match result {
                Ok(properties) => ForeignKeys::Ready(foreign_keys_of(&properties, &key.object)),
                Err(_) => ForeignKeys::Failed,
            };
            this.completion_foreign_keys.0.insert(key.clone(), state);
            this.completion_keys_loaded(&key, cx);
        });
    }
}

/// The text around the caret a completion is asked for.
struct Snapshot<'a> {
    /// The window of the document that is looked at, and the caret in it.
    text: &'a str,
    cursor: usize,
    /// Where the window starts in the document.
    base: usize,
    /// The whole document, to turn offsets into positions.
    rope: &'a Rope,
    /// Asked for by hand (Ctrl+Space), not by typing.
    manual: bool,
}

fn suggest(
    root: &AppRoot,
    tab_id: u64,
    role: Role,
    at: Snapshot<'_>,
) -> Option<(Vec<CompletionItem>, Warm)> {
    let Snapshot { text, cursor, base, rope, manual } = at;
    let tab = root.workspace.tab(tab_id)?;
    let runtime = root.workspace.runtime(tab_id);
    let kind = root
        .saved_connections
        .iter()
        .find(|saved| saved.matches_id(&tab.connection_id))
        .map(|saved| saved.kind);
    if tab.query_mode.is_redis_command() && matches!(role, Role::Query) {
        // Command names, then the keys already loaded in the sidebar.
        let keys = root.navigator.redis_loaded_keys(&tab.connection_id, &tab.database);
        let (range, candidates) = crate::redis::complete(
            text,
            cursor,
            keys,
            root.settings.editor.autocomplete_max_items as usize,
        );
        let range = LspRange {
            start: rope.offset_to_position(base + range.start),
            end: rope.offset_to_position(base + range.end),
        };
        let items = candidates
            .into_iter()
            .map(|candidate| CompletionItem {
                filter_text: Some(candidate.label.clone()),
                detail: Some(candidate.detail).filter(|detail| !detail.is_empty()),
                kind: Some(match candidate.kind {
                    crate::redis::CandidateKind::Command => CompletionItemKind::FUNCTION,
                    crate::redis::CandidateKind::Key => CompletionItemKind::FIELD,
                }),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range,
                    new_text: candidate.label.clone(),
                })),
                insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
                label: candidate.label,
                ..Default::default()
            })
            .collect();
        return Some((items, Warm::default()));
    }
    let collection = Some(tab.collection.as_str()).filter(|name| !name.is_empty());
    let sql = |fragment_of| {
        let flavor = flavor_of(kind);
        let identifier_quote = root.sql_dialect_for_connection(&tab.connection_id).map_or(
            if matches!(
                flavor,
                SqlFlavor::Mysql | SqlFlavor::Mariadb | SqlFlavor::Clickhouse
            ) {
                '`'
            } else {
                '"'
            },
            |dialect| dialect.identifier_quote,
        );
        Editor::Sql(SqlEditor {
            flavor,
            identifier_quote,
            keyword_case: root.settings.editor.format_keyword_case,
            manual,
            fragment_of,
        })
    };
    let mongo = |role, json| {
        Editor::Mongo(MongoEditor {
            role,
            collection,
            json,
            manual,
        })
    };
    let editor = match role {
        Role::Query
            if tab.query_mode.is_mongo_script() || kind == Some(ConnectionKind::Mongodb) =>
        {
            mongo(MongoRole::Script, false)
        }
        Role::Query => sql(None),
        Role::TableFragment { order_by } => sql(Some(Fragment {
            table: Relation {
                namespace: tab.schema.as_deref().filter(|schema| !schema.is_empty()),
                name: tab.collection.as_str(),
                view: false,
            },
            order_by,
        })),
        Role::Filter => mongo(MongoRole::Filter, true),
        Role::Sort => mongo(MongoRole::Sort, true),
        Role::Pipeline => mongo(MongoRole::Pipeline, false),
        Role::Stage(id) => {
            let draft = root.aggregate_draft(tab_id)?;
            let stage = draft.stages.iter().find(|stage| stage.id == id)?;
            mongo(MongoRole::Stage(stage.operator.as_str()), false)
        }
    };
    let catalog = NavigatorCatalog {
        nav: &root.navigator,
        foreign_keys: &root.completion_foreign_keys,
        schema_columns: &root.schema_columns,
        session: root.driver_runtime.connection_ids.get(&tab.connection_id).copied().unwrap_or(0),
        uncached_keys: RefCell::new(Vec::new()),
        connection: &tab.connection_id,
        database: &tab.database,
        default_namespace: default_namespace(kind, tab),
        structure: matches!(role, Role::TableFragment { .. })
            .then(|| {
                runtime.map(|runtime| {
                    (
                        tab.collection.as_str(),
                        runtime.structure_columns.as_slice(),
                    )
                })
            })
            .flatten(),
        fields: runtime.map_or(&[], |runtime| runtime.mongo.known_columns.as_slice()),
        collections: OnceCell::new(),
        uncached: RefCell::new(Vec::new()),
        wanted: RefCell::new(Vec::new()),
    };
    let completions = completion::complete(&editor, text, cursor, &catalog);
    let items = to_lsp(completions, base, rope, &root.settings.editor);
    let namespaces = catalog.wanted.take();
    let objects = (!namespaces.is_empty()).then(|| ObjectNeed {
        connection: tab.connection_id.clone(),
        database: tab.database.clone(),
        default_namespace: default_namespace(kind, tab).map(str::to_owned),
        namespaces,
    });
    let warm = Warm {
        connection: tab.connection_id.clone(),
        objects,
        nodes: catalog.uncached.take(),
        foreign_keys: catalog.uncached_keys.take(),
    };
    Some((items, warm))
}

fn default_namespace(kind: Option<ConnectionKind>, tab: &Tab) -> Option<&str> {
    let schema = tab.schema.as_deref().filter(|schema| !schema.is_empty());
    let database = Some(tab.database.as_str()).filter(|database| !database.is_empty());
    match kind {
        Some(ConnectionKind::Sqlite) => schema.or(Some("main")),
        Some(ConnectionKind::Mysql | ConnectionKind::Mariadb | ConnectionKind::Clickhouse) => {
            schema.or(database)
        }
        Some(ConnectionKind::Mongodb | ConnectionKind::Redis) => None,
        // DuckDB's default schema, `main`, in the default catalog.
        Some(ConnectionKind::Duckdb) => schema.or(Some("main")),
        Some(ConnectionKind::Mssql) => schema.or(Some("dbo")),
        _ => schema.or(Some("public")),
    }
}

fn lsp_kind(kind: Kind) -> CompletionItemKind {
    match kind {
        Kind::Snippet => CompletionItemKind::SNIPPET,
        Kind::Keyword => CompletionItemKind::KEYWORD,
        Kind::Function => CompletionItemKind::FUNCTION,
        Kind::Method => CompletionItemKind::METHOD,
        Kind::Table | Kind::Collection => CompletionItemKind::CLASS,
        Kind::View => CompletionItemKind::INTERFACE,
        Kind::Column | Kind::Field => CompletionItemKind::FIELD,
        Kind::Schema => CompletionItemKind::MODULE,
        Kind::Alias => CompletionItemKind::VARIABLE,
        Kind::Operator => CompletionItemKind::OPERATOR,
        Kind::Value => CompletionItemKind::VALUE,
    }
}

/// The menu replaces `text_edit.range` with `new_text`, highlights the first
/// `filter_text.len()` bytes of the label and shows `detail` beside it.
fn to_lsp(
    completions: Completions<'_>,
    base: usize,
    rope: &Rope,
    settings: &crate::settings::EditorSettings,
) -> Vec<CompletionItem> {
    let range_of = |range: &std::ops::Range<usize>| LspRange {
        start: rope.offset_to_position(base + range.start),
        end: rope.offset_to_position(base + range.end),
    };
    let range = range_of(&completions.replace);
    completions
        .items
        .into_iter()
        .take(settings.autocomplete_max_items as usize)
        .map(|item| CompletionItem {
            filter_text: Some(item.label[..item.matched].to_owned()),
            detail: Some(item.detail.into_owned()).filter(|detail| !detail.is_empty()),
            kind: Some(lsp_kind(item.kind)),
            text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                range: item.replace.as_ref().map_or(range, range_of),
                new_text: {
                    let mut text = item.insert.into_owned();
                    if settings.autocomplete_insert_space
                        && matches!(item.kind, Kind::Keyword)
                        && !text.ends_with(' ')
                    {
                        text.push(' ');
                    }
                    text
                },
            })),
            insert_text_format: Some(InsertTextFormat::PLAIN_TEXT),
            data: (item.caret_back > 0).then(|| item.caret_back.into()),
            label: item.label.into_owned(),
            ..Default::default()
        })
        .collect()
}

/// The navigator tree cache seen as a completion [`Catalog`].
///
/// Standard SQL connections (PostgreSQL, SQLite) store `Database > Schema >
/// ObjectGroup > Object > Column` nodes; driver-backed ones (MySQL, MariaDB)
/// store provider nodes of kind `database` / `table` / `view` / `column`.
/// Both are walked read-only; nodes that were never expanded simply yield
/// nothing.
struct NavigatorCatalog<'n> {
    nav: &'n NavigatorState,
    foreign_keys: &'n ForeignKeyCache,
    /// The columns of whole schemas, read once from the driver (checked before a table's own Columns folder).
    schema_columns: &'n super::schema_columns::SchemaColumnsCache,
    /// Engine connection id of the tab (0 = not connected), part of the key foreign keys are cached by.
    session: u64,
    /// Tables whose foreign keys were asked for but are not cached.
    uncached_keys: RefCell<Vec<PropertiesKey>>,
    connection: &'n str,
    /// Tab database; empty = every loaded database.
    database: &'n str,
    default_namespace: Option<&'n str>,
    /// Columns (with types) of the table a table-data tab shows.
    structure: Option<(&'n str, &'n [crate::catalog::CatalogColumn])>,
    /// Observed Mongo fields (current result columns).
    fields: &'n [String],
    collections: OnceCell<Vec<(NavigatorNodeId, String)>>,
    /// Tables whose columns were asked for but are not cached.
    uncached: RefCell<Vec<NavigatorNodeId>>,
    /// Schemas (`None`: the default one) whose table and view lists the completion needed.
    wanted: RefCell<Vec<Option<String>>>,
}

/// One table / view found in the cache.
struct Entry<'n> {
    namespace: &'n str,
    name: &'n str,
    view: bool,
    /// Node whose children are the columns.
    node: &'n NavigatorNodeId,
}

impl<'n> NavigatorCatalog<'n> {
    fn connection_node(&self) -> NavigatorNodeId {
        NavigatorNodeId::Connection {
            connection: self.connection.to_owned(),
        }
    }

    fn children(&self, id: &NavigatorNodeId) -> &'n [NavigatorNodeId] {
        self.nav.loaded_children(id).unwrap_or_default()
    }

    fn wants_database(&self, name: &str) -> bool {
        self.database.is_empty() || self.database == name
    }

    /// Provider payload string.
    fn payload<'a>(&'a self, id: &NavigatorNodeId, key: &str) -> Option<&'n str> {
        self.nav.provider_node(id)?.payload.get(key)?.as_str()
    }

    fn provider_kind(&self, id: &NavigatorNodeId) -> Option<&'n str> {
        self.nav.provider_node(id).map(|node| node.kind.as_str())
    }

    fn entries(&self, mut visit: impl FnMut(Entry<'n>)) {
        for database in self.children(&self.connection_node()) {
            match database {
                NavigatorNodeId::Database { database: name, .. } if self.wants_database(name) => {
                    for schema in self.children(database) {
                        let NavigatorNodeId::Schema {
                            connection,
                            database,
                            schema,
                            ..
                        } = schema
                        else {
                            continue;
                        };
                        for group in [
                            ObjectGroup::Tables,
                            ObjectGroup::Views,
                            ObjectGroup::MaterializedViews,
                            // MariaDB's sequences are objects that can be selected from.
                            ObjectGroup::Sequences,
                        ] {
                            let group_id = NavigatorNodeId::ObjectGroup {
                                connection: connection.clone(),
                                database: database.clone(),
                                schema: schema.clone(),
                                group,
                            };
                            for object in self.children(&group_id) {
                                if let NavigatorNodeId::Object { object: name, .. } = object {
                                    visit(Entry {
                                        namespace: schema,
                                        name,
                                        view: matches!(group, ObjectGroup::Views | ObjectGroup::MaterializedViews),
                                        node: object,
                                    });
                                }
                            }
                        }
                    }
                }
                NavigatorNodeId::Provider { .. }
                    if self.provider_kind(database) == Some("database") =>
                {
                    let Some(name) = self
                        .payload(database, "database")
                        .filter(|name| self.wants_database(name))
                    else {
                        continue;
                    };
                    for object in self.children(database) {
                        let view = match self.provider_kind(object) {
                            Some("table") => false,
                            Some("view") => true,
                            _ => continue,
                        };
                        if let Some(object_name) = self.payload(object, "object") {
                            visit(Entry {
                                namespace: name,
                                name: object_name,
                                view,
                                node: object,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn want_objects(&self, namespace: Option<&str>) {
        let mut wanted = self.wanted.borrow_mut();
        let known = wanted.iter().any(|known| match (known, namespace) {
            (None, None) => true,
            (Some(known), Some(namespace)) => known.eq_ignore_ascii_case(namespace),
            _ => false,
        });
        if !known {
            wanted.push(namespace.map(str::to_owned));
        }
    }

    /// The node of the table / view `relation` of `namespace` (the default one when `None`).
    fn node_of(&self, namespace: Option<&str>, relation: &str) -> Option<&'n NavigatorNodeId> {
        let mut node = None;
        self.entries(|entry| {
            let same_namespace = match namespace {
                Some(namespace) => entry.namespace.eq_ignore_ascii_case(namespace),
                None => self.is_default(Some(entry.namespace)),
            };
            if node.is_none() && same_namespace && entry.name.eq_ignore_ascii_case(relation) {
                node = Some(entry.node);
            }
        });
        node
    }

    /// The key the Properties view describes the table of `node` by.
    fn describe_key(&self, node: &NavigatorNodeId) -> Option<PropertiesKey> {
        let (database, schema, object) = match node {
            NavigatorNodeId::Object {
                database,
                schema,
                object,
                ..
            } => (database.as_str(), schema.as_str(), object.as_str()),
            // Driver-backed trees: the database is the namespace, there is no schema.
            NavigatorNodeId::Provider { .. } => (
                self.payload(node, "database")?,
                "",
                self.payload(node, "object")?,
            ),
            _ => return None,
        };
        Some(PropertiesKey {
            connection: self.connection.to_owned(),
            database: database.to_owned(),
            schema: schema.to_owned(),
            object: object.to_owned(),
            session: self.session,
        })
    }

    fn is_default(&self, namespace: Option<&str>) -> bool {
        match (namespace, self.default_namespace) {
            (None, _) => true,
            (Some(namespace), Some(default)) => namespace.eq_ignore_ascii_case(default),
            (Some(_), None) => false,
        }
    }

    fn column_of(&self, node: &'n NavigatorNodeId) -> Option<Column<'n>> {
        match node {
            NavigatorNodeId::Column { column, .. } => Some(Column {
                name: column,
                data_type: None,
            }),
            NavigatorNodeId::Provider { .. } if self.provider_kind(node) == Some("column") => {
                let provider = self.nav.provider_node(node)?;
                let name = provider.payload.get("column")?.as_str()?;
                let data_type = provider
                    .payload
                    .get("details")?
                    .get("data_type")
                    .and_then(|ty| ty.as_str());
                Some(Column { name, data_type })
            }
            _ => None,
        }
    }
}

impl Catalog for NavigatorCatalog<'_> {
    fn expect_relations(&self, namespace: Option<&str>) {
        self.want_objects(namespace);
    }

    fn relations<'a>(&'a self, visit: &mut dyn FnMut(Relation<'a>)) {
        self.entries(|entry| {
            visit(Relation {
                namespace: Some(entry.namespace),
                name: entry.name,
                view: entry.view,
            });
        });
    }

    fn namespaces<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        for database in self.children(&self.connection_node()) {
            match database {
                NavigatorNodeId::Database { database: name, .. } if self.wants_database(name) => {
                    for schema in self.children(database) {
                        if let NavigatorNodeId::Schema { schema, .. } = schema {
                            visit(schema);
                        }
                    }
                }
                NavigatorNodeId::Provider { .. } => {
                    if let Some(name) = self
                        .payload(database, "database")
                        .filter(|_| self.provider_kind(database) == Some("database"))
                    {
                        visit(name);
                    }
                }
                _ => {}
            }
        }
    }

    fn default_namespace(&self) -> Option<&str> {
        self.default_namespace
    }

    fn columns<'a>(
        &'a self,
        namespace: Option<&str>,
        relation: &str,
        visit: &mut dyn FnMut(Column<'a>),
    ) -> bool {
        if let Some((table, columns)) = self.structure {
            if table.eq_ignore_ascii_case(relation) && self.is_default(namespace) {
                for column in columns {
                    visit(Column {
                        name: &column.name,
                        data_type: Some(&column.data_type),
                    });
                }
                return true;
            }
        }
        let Some(node) = self.node_of(namespace, relation) else {
            // Maybe only because the schema's tables were never loaded.
            self.want_objects(namespace);
            return false;
        };
        if let NavigatorNodeId::Object { database, schema, object, .. } = node {
            let stored = self.schema_columns.table(self.connection, database, schema, self.session, object);
            if let Some(columns) = stored {
                for column in columns {
                    visit(Column { name: &column.name, data_type: Some(&*column.data_type).filter(|data_type| !data_type.is_empty()) });
                }
                return true;
            }
        }
        // A table's columns are the children of its Columns folder; a provider node's are its own.
        let columns = node.table_folder(crate::navigator::TableFolder::Columns);
        let columns = columns.as_ref().unwrap_or(node);
        let Some(children) = self.nav.loaded_children(columns) else {
            let mut uncached = self.uncached.borrow_mut();
            if !uncached.contains(columns) {
                uncached.push(columns.clone());
            }
            return false;
        };
        children
            .iter()
            .filter_map(|child| self.column_of(child))
            .for_each(visit);
        true
    }

    fn foreign_keys<'a>(
        &'a self,
        namespace: Option<&str>,
        relation: &str,
        visit: &mut dyn FnMut(ForeignKey<'a>),
    ) -> bool {
        let Some(key) = self
            .node_of(namespace, relation)
            .and_then(|node| self.describe_key(node))
        else {
            return false;
        };
        match self.foreign_keys.0.get(&key) {
            Some(ForeignKeys::Ready(keys)) => {
                for fk in keys {
                    visit(ForeignKey {
                        name: &fk.name,
                        table: &fk.table,
                        columns: &fk.columns,
                        referenced: &fk.referenced,
                        referenced_columns: &fk.referenced_columns,
                    });
                }
                true
            }
            Some(ForeignKeys::Failed) => false,
            // Asked for again so a request that waits on it knows; it is not loaded twice.
            Some(ForeignKeys::Loading) | None => {
                let mut uncached = self.uncached_keys.borrow_mut();
                if !uncached.contains(&key) {
                    uncached.push(key);
                }
                false
            }
        }
    }

    fn collections<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        let collections = self.collections.get_or_init(|| {
            self.nav
                .provider_collection_nodes(self.connection, self.database)
        });
        collections.iter().for_each(|(_, name)| visit(name));
    }

    fn fields<'a>(&'a self, visit: &mut dyn FnMut(&'a str)) {
        self.fields.iter().for_each(|field| visit(field));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use engine_driver_api::{NavigatorNode, NavigatorPage};
    use serde_json::json;

    use super::*;
    use crate::connections::SavedConnection;
    use crate::navigator::{self, LoadedChild};

    const CONN: &str = "11111111-1111-1111-1111-111111111111";

    static NO_FOREIGN_KEYS: std::sync::LazyLock<ForeignKeyCache> =
        std::sync::LazyLock::new(ForeignKeyCache::default);
    static NO_SCHEMA_COLUMNS: std::sync::LazyLock<crate::app::schema_columns::SchemaColumnsCache> =
        std::sync::LazyLock::new(Default::default);

    fn nav(kind: ConnectionKind) -> NavigatorState {
        NavigatorState::new(vec![SavedConnection {
            id: uuid::Uuid::parse_str(CONN).unwrap(),
            name: "test".into(),
            uri: "postgres://h/app".into(),
            kind,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }])
    }

    /// Expands `id` and answers its load with `children`.
    fn expand(nav: &mut NavigatorState, id: &NavigatorNodeId, children: Vec<LoadedChild>) {
        let request = nav.toggle_expansion(id).request.expect("load request");
        let generation = nav.load_generation(&request).expect("generation");
        nav.apply_load(&request, generation, Ok(children));
    }

    fn postgres_nav(tables: usize) -> NavigatorState {
        let mut nav = nav(ConnectionKind::Postgres);
        let connection = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        expand(
            &mut nav,
            &connection,
            vec![navigator::database_child(CONN, "app")],
        );
        let database = NavigatorNodeId::Database {
            connection: CONN.into(),
            database: "app".into(),
        };
        expand(
            &mut nav,
            &database,
            vec![navigator::schema_child(CONN, "app", "public")],
        );
        let schema = NavigatorNodeId::Schema {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
        };
        assert!(nav.toggle_expansion(&schema).request.is_none());
        let group = navigator::object_group_id(CONN, "app", "public", ObjectGroup::Tables);
        let objects = (0..tables)
            .map(|i| {
                navigator::object_child(
                    CONN,
                    "app",
                    "public",
                    &format!("table_{i:05}"),
                    navigator::KindOfObject::Table,
                )
            })
            .chain([navigator::object_child(
                CONN,
                "app",
                "public",
                "orders",
                navigator::KindOfObject::Table,
            )])
            .collect();
        expand(&mut nav, &group, objects);
        let orders = NavigatorNodeId::Object {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
            object: "orders".into(),
        };
        let columns = ["id", "total", "status"]
            .iter()
            .map(|column| navigator::column_child(CONN, "app", "public", "orders", column))
            .collect();
        expand(&mut nav, &orders.table_folder(navigator::TableFolder::Columns).unwrap(), columns);
        nav
    }

    fn catalog<'n>(
        nav: &'n NavigatorState,
        database: &'n str,
        default: Option<&'n str>,
    ) -> NavigatorCatalog<'n> {
        NavigatorCatalog {
            nav,
            foreign_keys: &NO_FOREIGN_KEYS,
            schema_columns: &NO_SCHEMA_COLUMNS,
            session: 1,
            uncached_keys: RefCell::new(Vec::new()),
            connection: CONN,
            database,
            default_namespace: default,
            structure: None,
            fields: &[],
            collections: OnceCell::new(),
            uncached: RefCell::new(Vec::new()),
            wanted: RefCell::new(Vec::new()),
        }
    }

    fn sql_editor(flavor: SqlFlavor) -> Editor<'static> {
        Editor::Sql(SqlEditor {
            flavor,
            identifier_quote: '"',
            keyword_case: crate::settings::KeywordCase::Upper,
            manual: false,
            fragment_of: None,
        })
    }

    fn labels(editor: &Editor<'_>, marked: &str, catalog: &NavigatorCatalog<'_>) -> Vec<String> {
        let cursor = marked.find('|').unwrap();
        let text = marked.replacen('|', "", 1);
        let completions = completion::complete(editor, &text, cursor, catalog);
        completions
            .items
            .iter()
            .map(|item| item.label.to_string())
            .collect()
    }

    #[test]
    fn a_mariadb_sequence_completes_like_a_table_and_a_postgres_one_does_not() {
        let tree = |kind, schema: &str, sequence: LoadedChild| {
            let mut nav = nav(kind);
            let connection = NavigatorNodeId::Connection { connection: CONN.into() };
            expand(&mut nav, &connection, vec![navigator::database_child(CONN, "app")]);
            let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
            expand(&mut nav, &database, vec![navigator::schema_child(CONN, "app", schema)]);
            let schema = NavigatorNodeId::Schema { connection: CONN.into(), database: "app".into(), schema: schema.into() };
            nav.toggle_expansion(&schema);
            let group = |group| navigator::object_group_id(CONN, "app", schema_name(&schema), group);
            fn schema_name(id: &NavigatorNodeId) -> &str {
                match id {
                    NavigatorNodeId::Schema { schema, .. } => schema,
                    _ => "",
                }
            }
            let sequences = group(ObjectGroup::Sequences);
            expand(&mut nav, &sequences, vec![sequence]);
            nav
        };
        let editor = sql_editor(SqlFlavor::Postgres);
        // MariaDB: the sequence is an object that can be selected from.
        let mariadb = tree(
            ConnectionKind::Mariadb,
            "app",
            navigator::object_child(CONN, "app", "app", "ticket_seq", navigator::KindOfObject::Sequence),
        );
        let found = labels(&editor, "select * from ticket|", &catalog(&mariadb, "app", Some("app")));
        assert_eq!(found, ["ticket_seq"]);
        // PostgreSQL: a sequence row has a source only, so it is no table name.
        let reference = navigator::SchemaObjectRef {
            database: Some("app".into()),
            schema: Some("public".into()),
            kind: navigator::SchemaObjectKind::Sequence,
            name: "ticket_seq".into(),
            arguments: None,
            table: None,
            identity: None,
        };
        let postgres = tree(
            ConnectionKind::Postgres,
            "public",
            navigator::schema_object_child(CONN, "app", "public", ObjectGroup::Sequences, "sequence:ticket_seq", "ticket_seq", reference),
        );
        assert!(labels(&editor, "select * from ticket|", &catalog(&postgres, "app", Some("public"))).is_empty());
    }

    #[test]
    fn standard_tree_yields_tables_and_cached_columns_only() {
        let nav = postgres_nav(3);
        let catalog = catalog(&nav, "app", Some("public"));
        let editor = sql_editor(SqlFlavor::Postgres);
        assert_eq!(labels(&editor, "select * from ord|", &catalog)[0], "orders");
        assert_eq!(
            labels(&editor, "select o.| from orders o", &catalog),
            ["id", "total", "status"]
        );
        // table_00001 was never expanded: no columns, but the table itself is known.
        assert!(labels(&editor, "select t.| from table_00001 t", &catalog).is_empty());
        assert!(labels(&editor, "select * from table_0000|", &catalog).len() >= 3);
        // The database filter hides other databases' tables.
        let other = self::catalog(&nav, "other", Some("public"));
        assert!(labels(&editor, "select * from ord|", &other).is_empty());
    }

    /// Starts the load of `id` the way a completion does and answers it with `children`.
    fn land(nav: &mut NavigatorState, id: &NavigatorNodeId, children: Vec<LoadedChild>) {
        let request = nav.begin_background_load(id).expect("not loaded yet");
        let generation = nav.load_generation(&request).expect("generation");
        nav.apply_load(&request, generation, Ok(children));
        assert!(
            nav.visible_rows().iter().all(|row| !row.expanded),
            "a background load never expands the sidebar"
        );
    }

    fn need(database: &str, default: Option<&str>, namespaces: &[Option<&str>]) -> ObjectNeed {
        ObjectNeed {
            connection: CONN.into(),
            database: database.into(),
            default_namespace: default.map(str::to_owned),
            namespaces: namespaces.iter().map(|namespace| namespace.map(str::to_owned)).collect(),
        }
    }

    #[test]
    fn table_lists_are_loaded_level_by_level_down_to_the_schema_without_the_sidebar() {
        let mut nav = nav(ConnectionKind::Postgres);
        let connection = NavigatorNodeId::Connection { connection: CONN.into() };
        let need = need("app", Some("public"), &[None]);
        assert_eq!(need.frontier(&nav), std::slice::from_ref(&connection));
        land(&mut nav, &connection, vec![navigator::database_child(CONN, "other"), navigator::database_child(CONN, "app")]);
        let database = NavigatorNodeId::Database { connection: CONN.into(), database: "app".into() };
        assert_eq!(need.frontier(&nav), std::slice::from_ref(&database), "only the tab's database");
        land(
            &mut nav,
            &database,
            vec![navigator::schema_child(CONN, "app", "public"), navigator::schema_child(CONN, "app", "sales")],
        );
        let group = |schema: &str, group| navigator::object_group_id(CONN, "app", schema, group);
        assert_eq!(
            need.frontier(&nav),
            [group("public", ObjectGroup::Tables), group("public", ObjectGroup::Views)],
            "the default schema only"
        );
        // A qualifier names another schema; the default one is no longer wanted when it is loaded.
        land(&mut nav, &group("public", ObjectGroup::Tables), Vec::new());
        land(&mut nav, &group("public", ObjectGroup::Views), Vec::new());
        assert!(need.frontier(&nav).is_empty());
        let qualified = self::need("app", Some("public"), &[None, Some("SALES")]);
        assert_eq!(
            qualified.frontier(&nav),
            [group("sales", ObjectGroup::Tables), group("sales", ObjectGroup::Views)]
        );
        // Everything above is loaded: a catalog reads the lists and the sidebar never opened.
        assert!(nav.visible_rows().iter().all(|row| !row.expanded));
    }

    #[test]
    fn table_lists_of_a_driver_provider_tree_are_loaded_from_the_database_nodes() {
        let mut nav = nav(ConnectionKind::Mysql);
        nav.enable_provider(CONN);
        let node = |id: &str, kind: &str, payload: serde_json::Value| NavigatorNode {
            id: id.into(),
            kind: kind.into(),
            label: id.into(),
            subtitle: None,
            icon: None,
            has_children: true,
            payload,
            actions: Vec::new(),
        };
        let land_page = |nav: &mut NavigatorState, id: &NavigatorNodeId, nodes: Vec<NavigatorNode>| {
            let request = nav.begin_background_load(id).expect("not loaded yet");
            let generation = nav.load_generation(&request).unwrap();
            nav.apply_provider_page(&request, generation, Ok(NavigatorPage { nodes, next_cursor: None }));
        };
        let connection = NavigatorNodeId::Connection { connection: CONN.into() };
        let need = need("shop", Some("shop"), &[None]);
        assert_eq!(need.frontier(&nav), std::slice::from_ref(&connection));
        land_page(
            &mut nav,
            &connection,
            vec![
                node("database:shop", "database", json!({"database": "shop"})),
                node("database:other", "database", json!({"database": "other"})),
            ],
        );
        let database = NavigatorNodeId::Provider { connection: CONN.into(), node: "database:shop".into() };
        assert_eq!(need.frontier(&nav), std::slice::from_ref(&database));
        land_page(&mut nav, &database, vec![node("table:shop:a", "table", json!({"database": "shop", "object": "a"}))]);
        assert!(need.frontier(&nav).is_empty());
    }

    #[test]
    fn a_completion_notes_the_schemas_whose_tables_it_needs() {
        let cold = nav(ConnectionKind::Postgres);
        let loaded = postgres_nav(3);
        let editor = sql_editor(SqlFlavor::Postgres);
        let wanted_in = |nav: &NavigatorState, marked: &str| {
            let catalog = catalog(nav, "app", Some("public"));
            labels(&editor, marked, &catalog);
            catalog.wanted.take()
        };
        let nav = &cold;
        let wanted = |marked: &str| wanted_in(nav, marked);
        assert_eq!(wanted("select * from ord|"), [None]);
        // Not a known schema yet: a table of the default one, whose lists are what is missing.
        assert_eq!(wanted("select * from sales.|"), [None]);
        // A schema the tree lists: its own tables are listed.
        assert_eq!(wanted_in(&loaded, "select * from public.|"), [Some("public".to_owned())]);
        assert_eq!(wanted("select o.| from sales.orders o"), [Some("sales".to_owned())]);
        assert!(wanted("select |").is_empty(), "no table is involved");
    }

    #[test]
    fn driver_provider_tree_is_read_for_mysql() {
        let mut nav = nav(ConnectionKind::Mysql);
        nav.enable_provider(CONN);
        let node = |id: &str, kind: &str, label: &str, payload: serde_json::Value| NavigatorNode {
            id: id.into(),
            kind: kind.into(),
            label: label.into(),
            subtitle: None,
            icon: None,
            has_children: true,
            payload,
            actions: Vec::new(),
        };
        let expand_provider =
            |nav: &mut NavigatorState, id: &NavigatorNodeId, nodes: Vec<NavigatorNode>| {
                let request = nav.toggle_expansion(id).request.expect("provider load");
                let generation = nav.load_generation(&request).unwrap();
                nav.apply_provider_page(
                    &request,
                    generation,
                    Ok(NavigatorPage {
                        nodes,
                        next_cursor: None,
                    }),
                );
            };
        let root = NavigatorNodeId::Connection {
            connection: CONN.into(),
        };
        expand_provider(
            &mut nav,
            &root,
            vec![node(
                "database:shop",
                "database",
                "shop",
                json!({"database": "shop"}),
            )],
        );
        let database = NavigatorNodeId::Provider {
            connection: CONN.into(),
            node: "database:shop".into(),
        };
        let object = json!({"database": "shop", "object": "customers"});
        expand_provider(
            &mut nav,
            &database,
            vec![
                node("table:shop:customers", "table", "customers", object),
                node(
                    "view:shop:vip",
                    "view",
                    "vip",
                    json!({"database": "shop", "object": "vip"}),
                ),
            ],
        );
        let table = NavigatorNodeId::Provider {
            connection: CONN.into(),
            node: "table:shop:customers".into(),
        };
        let column = |name: &str, ty: &str| {
            node(
                &format!("column:shop:customers:{name}"),
                "column",
                name,
                json!({"object": {"database": "shop", "object": "customers"}, "column": name, "details": {"name": name, "data_type": ty}}),
            )
        };
        expand_provider(
            &mut nav,
            &table,
            vec![column("id", "int"), column("email", "varchar(255)")],
        );

        let catalog = catalog(&nav, "shop", Some("shop"));
        let editor = Editor::Sql(SqlEditor {
            flavor: SqlFlavor::Mysql,
            identifier_quote: '`',
            keyword_case: crate::settings::KeywordCase::Upper,
            manual: false,
            fragment_of: None,
        });
        assert_eq!(
            labels(&editor, "select * from c|", &catalog)[0],
            "customers"
        );
        assert!(labels(&editor, "select * from v|", &catalog).contains(&"vip".to_string()));
        assert_eq!(
            labels(&editor, "select c.| from customers c", &catalog),
            ["id", "email"]
        );
        let typed = completion::complete(&editor, "select c.em from customers c", 11, &catalog);
        assert_eq!(typed.items[0].detail, "varchar(255)");
    }

    /// The whole path of a keystroke over a 10k-table navigator cache.
    #[test]
    fn navigator_cache_with_ten_thousand_tables_completes_fast() {
        let nav = postgres_nav(10_000);
        let catalog = catalog(&nav, "app", Some("public"));
        let editor = sql_editor(SqlFlavor::Postgres);
        for (marked, what) in [
            ("select * from |", "empty prefix"),
            ("select * from t|", "one letter"),
            ("select * from table_09|", "prefix"),
            ("select * from ords|", "orders by subsequence"),
            ("select o.| from orders o", "columns via alias"),
        ] {
            let cursor = marked.find('|').unwrap();
            let text = marked.replacen('|', "", 1);
            let run =
                || std::hint::black_box(completion::complete(&editor, &text, cursor, &catalog));
            (0..20).for_each(|_| drop(run()));
            let runs = 200;
            let start = Instant::now();
            (0..runs).for_each(|_| drop(run()));
            let micros = start.elapsed().as_secs_f64() * 1e6 / f64::from(runs);
            println!("nav-cache completion latency, 10k tables, {what:<22}: {micros:>8.1} us");
            // The 1 ms budget is for optimized builds; unoptimized runs under a
            // loaded test machine only guard against pathological slowdowns.
            let budget = if cfg!(debug_assertions) {
                250_000.0
            } else {
                1_000.0
            };
            assert!(micros < budget, "{what}: {micros} us");
        }
    }
    fn cached_foreign_keys(object: &str, keys: Vec<StoredForeignKey>) -> ForeignKeyCache {
        let key = PropertiesKey {
            connection: CONN.into(),
            database: "app".into(),
            schema: "public".into(),
            object: object.into(),
            session: 1,
        };
        ForeignKeyCache(HashMap::from([(key, ForeignKeys::Ready(keys))]))
    }

    fn orders_to_table_00005() -> StoredForeignKey {
        StoredForeignKey {
            name: "orders_ref_fk".into(),
            table: "orders".into(),
            columns: vec!["total".into()],
            referenced: "table_00005".into(),
            referenced_columns: vec!["id".into()],
        }
    }

    #[test]
    fn foreign_keys_are_read_from_the_cache_and_asked_for_when_missing() {
        let nav = postgres_nav(10);
        let editor = sql_editor(SqlFlavor::Postgres);
        let marked = "select * from orders o join |";

        // Not cached: nothing foreign key based, and the table is queued to be described.
        let catalog = catalog(&nav, "app", Some("public"));
        assert_eq!(labels(&editor, marked, &catalog)[0], "table_00000");
        let queued = catalog.uncached_keys.take();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            (queued[0].database.as_str(), queued[0].schema.as_str(), queued[0].object.as_str()),
            ("app", "public", "orders")
        );

        // Cached: the partner is first, and the condition is offered after ON.
        let cache = cached_foreign_keys("orders", vec![orders_to_table_00005()]);
        let cached = NavigatorCatalog { foreign_keys: &cache, ..self::catalog(&nav, "app", Some("public")) };
        assert_eq!(labels(&editor, marked, &cached)[0], "table_00005");
        assert!(cached.uncached_keys.take().is_empty());
        assert_eq!(
            labels(&editor, "select * from orders o join table_00005 t on |", &cached)[0],
            "o.total = t.id"
        );
        // A table that is still loading offers nothing and is reported so a request can wait for it;
        // one that failed offers nothing and is not asked for again.
        let loading = ForeignKeyCache(HashMap::from([(
            PropertiesKey {
                connection: CONN.into(),
                database: "app".into(),
                schema: "public".into(),
                object: "orders".into(),
                session: 1,
            },
            ForeignKeys::Loading,
        )]));
        let waiting = NavigatorCatalog { foreign_keys: &loading, ..self::catalog(&nav, "app", Some("public")) };
        assert_eq!(labels(&editor, marked, &waiting)[0], "table_00000");
        assert_eq!(waiting.uncached_keys.take().len(), 1);
        let failed = ForeignKeyCache(HashMap::from([(
            PropertiesKey {
                connection: CONN.into(),
                database: "app".into(),
                schema: "public".into(),
                object: "orders".into(),
                session: 1,
            },
            ForeignKeys::Failed,
        )]));
        let failed = NavigatorCatalog { foreign_keys: &failed, ..self::catalog(&nav, "app", Some("public")) };
        assert_eq!(labels(&editor, marked, &failed)[0], "table_00000");
        assert!(failed.uncached_keys.take().is_empty());
    }

    #[test]
    fn a_table_is_described_by_the_name_the_properties_view_uses() {
        assert_eq!(bare_name("public.orders"), "orders");
        assert_eq!(bare_name("\"my schema\".\"Order Items\""), "Order Items");
        assert_eq!(bare_name("`shop`.`orders`"), "orders");
        assert_eq!(bare_name("orders"), "orders");
    }

    /// A join over a 10k-table cache: the partner scan is one pass over the names, the
    /// foreign keys read are those of the tables in scope.
    #[test]
    fn join_completion_with_foreign_keys_over_ten_thousand_tables_stays_fast() {
        let nav = postgres_nav(10_000);
        let cache = cached_foreign_keys("orders", vec![orders_to_table_00005()]);
        let catalog = NavigatorCatalog { foreign_keys: &cache, ..catalog(&nav, "app", Some("public")) };
        let editor = sql_editor(SqlFlavor::Postgres);
        for (marked, first) in [
            ("select * from orders o join |", "table_00005"),
            ("select * from orders o join table_00005 t on |", "o.total = t.id"),
        ] {
            let cursor = marked.find('|').unwrap();
            let text = marked.replacen('|', "", 1);
            let run = || std::hint::black_box(completion::complete(&editor, &text, cursor, &catalog));
            assert_eq!(run().items[0].label, first);
            (0..20).for_each(|_| drop(run()));
            let runs = 200;
            let start = Instant::now();
            (0..runs).for_each(|_| drop(run()));
            let micros = start.elapsed().as_secs_f64() * 1e6 / f64::from(runs);
            println!("nav-cache completion latency, 10k tables, {marked:<48}: {micros:>8.1} us");
            let budget = if cfg!(debug_assertions) { 250_000.0 } else { 1_000.0 };
            assert!(micros < budget, "{marked}: {micros} us");
        }
    }
}
