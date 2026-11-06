//! Redis in the app: the key tab (typed value, TTL header, edits through the
//! driver's key mutations), the command console, and the key-browser menus of
//! the sidebar. The wire shapes live in `crate::redis`; the SCAN tree in
//! `navigator::redis_tree`.
//!
//! Everything goes through the driver route the SQL kinds use: the value page
//! is a `redis.key` query run by `run_sql` (so paging, cancel, the run
//! gate, the query timeout and the Error Log are shared), a console run is a
//! `redis.command` query (the driver's `query.confirmation_required` opens the
//! shared confirm-and-resend dialog), and an edit is one `MutationRequest`
//! with a host-allocated operation id.

use super::*;
use crate::navigator::redis_tree::{DATABASE, KEY, NAMESPACE};
use crate::redis::{self, Edit, KeyFilter, KeyInfo, KeyType, StringView, Ttl};
use gpui_component::button::ButtonVariant;
use gpui_component::select::{Select, SelectState};
use gpui_component::IndexPath;

/// Redis state of the app root (per-tab editors and the TTL ticker).
#[derive(Default)]
pub(super) struct RedisUi {
    /// The editor of each string key tab.
    pub(super) string_editors: HashMap<u64, gpui::Entity<EditorState>>,
    /// `(string revision, view)` each editor was last filled for.
    string_synced: HashMap<u64, (u64, StringView)>,
    /// The TTL label last painted per tab (a repaint only when it changes).
    pub(super) ttl_labels: HashMap<u64, String>,
    /// Counts the TTL of the visible key tabs down; absent while none counts.
    ticker: Option<Task<()>>,
}

impl RedisUi {
    pub(super) fn tab_removed(&mut self, tab_id: u64) {
        self.string_editors.remove(&tab_id);
        self.string_synced.remove(&tab_id);
        self.ttl_labels.remove(&tab_id);
    }
}

/// What a sidebar row of a Redis connection stands for.
#[derive(Debug, Clone)]
pub(super) enum RedisTarget {
    Database {
        connection: String,
        db: String,
        node: NavigatorNodeId,
    },
    Namespace {
        connection: String,
        db: String,
        prefix: String,
    },
    Key {
        connection: String,
        db: String,
        key_raw: String,
        display: String,
        node: NavigatorNodeId,
    },
}

/// One input of a Redis dialog.
struct FormField {
    label: &'static str,
    kind: FieldKind,
    initial: String,
    placeholder: &'static str,
}

enum FieldKind {
    Line,
    Text,
    Choice(Vec<String>),
}

impl FormField {
    fn line(label: &'static str, initial: impl Into<String>, placeholder: &'static str) -> Self {
        Self { label, kind: FieldKind::Line, initial: initial.into(), placeholder }
    }

    fn text(label: &'static str, initial: impl Into<String>, placeholder: &'static str) -> Self {
        Self { label, kind: FieldKind::Text, initial: initial.into(), placeholder }
    }

    fn choice(label: &'static str, options: &[&str]) -> Self {
        Self {
            label,
            kind: FieldKind::Choice(options.iter().map(|option| (*option).to_owned()).collect()),
            initial: String::new(),
            placeholder: "",
        }
    }
}

#[derive(Clone)]
enum FieldEntity {
    Line(gpui::Entity<InputState>),
    Text(gpui::Entity<TextareaState>),
    Choice(gpui::Entity<SelectState<Vec<String>>>),
}

/// What runs with the staged key's new revision once an edit is saved.
type EditDone = Box<dyn FnOnce(&mut AppRoot, u64, &mut Context<AppRoot>)>;
/// A menu item's action on the app root.
type MenuAction = Box<dyn Fn(&mut AppRoot, &mut Window, &mut Context<AppRoot>)>;

/// The answer of a dialog's OK: `Err` keeps the dialog open and says why.
type FormOk = std::rc::Rc<dyn Fn(&mut AppRoot, Vec<String>, &mut Context<AppRoot>) -> Result<(), String>>;

/// Colors of the key-type glyph and badge (the navigator's object colors).
pub(super) fn type_color(key_type: Option<KeyType>) -> gpui::Rgba {
    match key_type {
        Some(KeyType::String) => theme::icon_schema(),
        Some(KeyType::Hash) => theme::icon_collection(),
        Some(KeyType::List) => theme::icon_view(),
        Some(KeyType::Set) => theme::icon_folder(),
        Some(KeyType::ZSet) => theme::icon_database(),
        Some(KeyType::Stream) => theme::icon_bucket(),
        None => theme::text_muted(),
    }
}

/// The db index in a Redis URI path (`redis://host/3`), `0` without one.
pub(super) fn default_db(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.path().trim_start_matches('/').parse::<u32>().ok())
        .unwrap_or(0)
        .to_string()
}

impl AppRoot {
    // ---- sidebar targets -------------------------------------------------

    /// The Redis meaning of a sidebar row.
    pub(super) fn redis_node_target(&self, id: &NavigatorNodeId) -> Option<RedisTarget> {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return None;
        };
        if !self.navigator.connection(connection).is_some_and(|saved| saved.kind == ConnectionKind::Redis) {
            return None;
        }
        let node = self.navigator.provider_node(id)?;
        let text = |name: &str| node.payload.get(name).and_then(serde_json::Value::as_str).map(str::to_owned);
        let db = text("database")?;
        match node.kind.as_str() {
            DATABASE => Some(RedisTarget::Database { connection: connection.clone(), db, node: id.clone() }),
            NAMESPACE => Some(RedisTarget::Namespace {
                connection: connection.clone(),
                db,
                prefix: text("prefix")?,
            }),
            KEY => Some(RedisTarget::Key {
                connection: connection.clone(),
                db,
                key_raw: text("key_raw")?,
                display: text("key_display").unwrap_or_else(|| {
                    redis::decode_key(&text("key_raw").unwrap_or_default())
                        .map(|bytes| redis::display_key(&bytes))
                        .unwrap_or_default()
                }),
                node: id.clone(),
            }),
            _ => None,
        }
    }

    // ---- tabs --------------------------------------------------------------

    /// A click on a key row opens its value tab.
    pub(super) fn open_redis_key_node(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) -> bool {
        let Some(RedisTarget::Key { connection, db, key_raw, display, .. }) = self.redis_node_target(id) else {
            return false;
        };
        self.open_redis_key_tab(&connection, &db, &key_raw, &display, cx);
        true
    }

    /// Opens (or activates) the value tab of a key and reads it when new.
    pub(super) fn open_redis_key_tab(
        &mut self,
        connection: &str,
        db: &str,
        key_raw: &str,
        display: &str,
        cx: &mut Context<Self>,
    ) -> u64 {
        let (tab_id, created) = self.workspace.open_or_activate_redis_key_tab(
            connection,
            db,
            key_raw,
            &redis::tab_label(display, db),
        );
        if created {
            self.run_tab_query(tab_id, cx);
            self.load_redis_key_info(tab_id, cx);
        }
        self.notify_chrome(cx);
        tab_id
    }

    /// Opens a command console on database `db`.
    pub(super) fn open_redis_console_tab(&mut self, connection: &str, db: &str) -> u64 {
        self.workspace.open_query_tab_with_metadata(
            format!("console@db{db}"),
            TabMetadata {
                connection_id: connection.to_owned(),
                database: db.to_owned(),
                query_mode: Some(QueryMode::RedisCommand),
                ..Default::default()
            },
        )
    }

    /// New Query on a Redis connection / database row.
    pub(super) fn open_redis_console_for_node(&mut self, id: &NavigatorNodeId) -> bool {
        match id {
            NavigatorNodeId::Connection { connection } => {
                let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)) else {
                    return false;
                };
                let db = default_db(&saved.uri);
                self.open_redis_console_tab(connection, &db);
                true
            }
            other => match self.redis_node_target(other) {
                Some(RedisTarget::Database { connection, db, .. }) => {
                    self.open_redis_console_tab(&connection, &db);
                    true
                }
                _ => false,
            },
        }
    }

    /// The key a Redis key tab shows: `(connection, db, key_raw)`.
    fn redis_tab_key(&self, tab_id: u64) -> Option<(String, String, String)> {
        let tab = self.workspace.tab(tab_id).filter(|tab| tab.query_mode.is_redis_key())?;
        Some((tab.connection_id.clone(), tab.database.clone(), tab.collection.clone()))
    }

    /// The type of the key a tab shows, from the page it holds.
    pub(super) fn redis_tab_key_type(&self, tab_id: u64) -> Option<KeyType> {
        let grid = self.grid(tab_id);
        let names: Vec<&str> = (0..grid.column_count()).filter_map(|column| grid.column_name(column)).collect();
        match names.as_slice() {
            ["value"] => Some(KeyType::String),
            ["field", "value"] => Some(KeyType::Hash),
            ["index", "value"] => Some(KeyType::List),
            ["member"] => Some(KeyType::Set),
            ["member", "score"] => Some(KeyType::ZSet),
            ["id", "fields"] => Some(KeyType::Stream),
            _ => self
                .workspace
                .runtime(tab_id)
                .and_then(|runtime| runtime.redis.info.as_ref())
                .and_then(KeyInfo::key_type),
        }
    }

    // ---- key info -----------------------------------------------------------

    /// Reads `redis.key_info` of the tab's key (type, TTL, length, encoding).
    pub(super) fn load_redis_key_info(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let Some((connection, db, key_raw)) = self.redis_tab_key(tab_id) else {
            return;
        };
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        if runtime.redis.info_loading {
            return;
        }
        let prepared = match self.begin_driver_query(&connection) {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                    runtime.redis.info_error = Some(error.to_string());
                }
                return;
            }
        };
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.redis.info_loading = true;
        }
        let PreparedDriverQuery { session, runner, operation } = prepared;
        let request = engine_driver_api::QueryRequest {
            kind: redis::KIND_KEY_INFO.into(),
            operation_id: runner.operation_id(),
            database: Some(db),
            object: None,
            offset: 0,
            limit: 0,
            payload: redis::key_payload(&key_raw),
        };
        let request_json = serde_json::to_string(&request).expect("key info request serializes");
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                // Keeps the host-allocated operation registered until done.
                let _operation = operation;
                let session_for_guard = Arc::clone(&session);
                let page = runner.open_query(&request_json)?;
                let mut session = session.lock().map_err(|_| {
                    runner.release_page(page.page_id);
                    DriverQueryError::SessionPoisoned
                })?;
                session.adopt_page(&runner, page.page_id)?;
                let _guard = DriverPageGuard::new(session_for_guard, page.page_id);
                let lease = session.acquire_arrow_page(page.page_id, 0, page.row_count)?;
                // SAFETY: the lease holds the Arrow C Data pair the driver
                // exported for this page; import consumes it once.
                let imported = unsafe { lease.into_record_batch() }?;
                Ok::<_, DriverQueryError>(KeyInfo::from_batch(&imported.batch))
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_redis_key_info(tab_id, &key_raw, result, cx);
            });
        })
        .detach();
    }

    fn finish_redis_key_info(
        &mut self,
        tab_id: u64,
        key_raw: &str,
        result: Result<Option<KeyInfo>, DriverQueryError>,
        cx: &mut Context<Self>,
    ) {
        // The tab may have closed, or been repointed, while the call ran.
        if self.redis_tab_key(tab_id).is_none_or(|(_, _, current)| current != key_raw) {
            return;
        }
        let Some(runtime) = self.workspace.runtime_mut(tab_id) else {
            return;
        };
        runtime.redis.info_loading = false;
        match result {
            Ok(Some(info)) => {
                runtime.redis.info = Some(info);
                runtime.redis.info_read_at = Some(Instant::now());
                runtime.redis.info_error = None;
            }
            Ok(None) => runtime.redis.info_error = Some("The server returned no key information".into()),
            Err(error) => {
                let gone = matches!(&error, DriverQueryError::Driver(engine_driver_host::LoaderError::DriverRejected { code, .. }) if code == "query.key_not_found");
                runtime.redis.info = None;
                runtime.redis.info_error = Some(if gone {
                    "This key no longer exists".to_owned()
                } else {
                    crate::sessions::redact_secrets(&error.to_string())
                });
            }
        }
        self.start_redis_ticker(cx);
        self.notify_tab(tab_id, cx);
    }

    // ---- TTL countdown ------------------------------------------------------

    /// Starts the one-second timer that repaints the TTL of the key tabs on
    /// screen. It runs only while one of them is counting down and repaints a
    /// tab only when its label changes (once a second below an hour, once a
    /// minute above).
    pub(super) fn start_redis_ticker(&mut self, cx: &mut Context<Self>) {
        if self.redis_ui.ticker.is_some() || self.scenario_ui.service.is_some() {
            return;
        }
        let counting = self.counting_redis_tabs();
        if counting.is_empty() {
            return;
        }
        self.redis_ui.ticker = Some(cx.spawn(async move |this, cx| loop {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let keep = this
                .update(cx, |this, cx| this.tick_redis_ttl(cx))
                .unwrap_or(false);
            if !keep {
                let _ = this.update(cx, |this, _| this.redis_ui.ticker = None);
                break;
            }
        }));
    }

    /// Visible key tabs whose TTL is counting down.
    fn counting_redis_tabs(&self) -> Vec<u64> {
        let now = Instant::now();
        self.visible_tab_ids()
            .into_iter()
            .filter(|tab_id| {
                self.workspace.tab(*tab_id).is_some_and(|tab| tab.query_mode.is_redis_key())
                    && self.workspace.runtime(*tab_id).is_some_and(|runtime| runtime.redis.counting_down(now))
            })
            .collect()
    }

    /// One tick. `false` when nothing counts any more.
    pub(super) fn tick_redis_ttl(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        let counting = self.counting_redis_tabs();
        for tab_id in &counting {
            let label = self
                .workspace
                .runtime(*tab_id)
                .and_then(|runtime| runtime.redis.ttl_left(now))
                .map(redis::format_ttl)
                .unwrap_or_default();
            let changed = self.redis_ui.ttl_labels.get(tab_id).is_none_or(|shown| *shown != label);
            if changed {
                self.redis_ui.ttl_labels.insert(*tab_id, label);
                self.notify_tab(*tab_id, cx);
            }
        }
        // A key that just expired: ask the server what is left of it.
        let expired: Vec<u64> = self
            .visible_tab_ids()
            .into_iter()
            .filter(|tab_id| {
                self.workspace.runtime(*tab_id).is_some_and(|runtime| {
                    !runtime.redis.info_loading
                        && runtime.redis.ttl_left(now).is_some_and(|left| left <= 0)
                })
            })
            .collect();
        for tab_id in expired {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                // One refresh: the answer replaces the info (or says the key is gone).
                if let Some(info) = runtime.redis.info.as_mut() {
                    info.ttl = None;
                }
            }
            self.load_redis_key_info(tab_id, cx);
        }
        !counting.is_empty()
    }

    // ---- value page hooks -------------------------------------------------------

    /// A value page landed in the grid of `tab_id`: keep the string value of
    /// a string key (the string view edits it) and refresh the key's header.
    pub(super) fn after_redis_key_page(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let string = if self.redis_tab_key_type(tab_id) == Some(KeyType::String) {
            match self.grid(tab_id).source() {
                GridContentSource::Arrow(source) => source.binary_cell(0, 0).map(|bytes| bytes.into()),
                _ => None,
            }
        } else {
            None
        };
        let truncated = self
            .workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.pagination.has_next());
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            let previous = runtime.redis.string.as_ref().map(|value| value.bytes.clone());
            runtime.redis.string = string.map(|bytes| redis::StringValue { bytes, truncated });
            let changed = runtime.redis.string.as_ref().map(|value| value.bytes.clone()) != previous;
            if changed {
                runtime.redis.revision += 1;
                if let Some(value) = &runtime.redis.string {
                    runtime.redis.view = value.default_view();
                }
            }
        }
        self.load_redis_key_info(tab_id, cx);
    }

    /// Fills the editor of a string key tab with its value in the chosen view.
    pub(super) fn sync_redis_string(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        let Some(value) = runtime.redis.string.clone() else {
            return;
        };
        let want = (runtime.redis.revision, runtime.redis.view);
        let editor = self.redis_string_editor(tab_id, window, cx);
        if self.redis_ui.string_synced.get(&tab_id) == Some(&want) {
            return;
        }
        let text = match want.1 {
            StringView::Text => String::from_utf8_lossy(&value.bytes).into_owned(),
            StringView::Json => {
                let text = String::from_utf8_lossy(&value.bytes);
                redis::json_pretty(&text).unwrap_or_else(|| text.into_owned())
            }
            StringView::Hex => redis::hex_dump(&value.bytes),
        };
        editor.update(cx, |state, cx| state.set_value(text, window, cx));
        self.redis_ui.string_synced.insert(tab_id, want);
    }

    fn redis_string_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self.redis_ui.string_editors.get(&tab_id) {
            return editor.clone();
        }
        let settings = self.settings.editor.clone();
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("plaintext")
                .line_number(settings.line_numbers)
                .soft_wrap(true)
        });
        self.redis_ui.string_editors.insert(tab_id, editor.clone());
        editor
    }

    // ---- rendering -----------------------------------------------------------------

    /// Whether the body of a key tab is the string editor rather than a grid.
    pub(super) fn redis_string_view_active(&self, tab_id: u64) -> bool {
        self.workspace
            .runtime(tab_id)
            .is_some_and(|runtime| runtime.redis.string.is_some())
            && self.workspace.tab(tab_id).is_some_and(|tab| tab.query_mode.is_redis_key())
    }

    /// The header of a key tab: type, name, TTL, size, encoding and actions.
    pub(super) fn render_redis_key_toolbar(
        &mut self,
        tab_id: u64,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // A key tab came into view: its TTL may need counting down.
        self.start_redis_ticker(cx);
        let Some(tab) = self.workspace.tab(tab_id) else {
            return div().into_any_element();
        };
        let display = redis::decode_key(&tab.collection)
            .map(|bytes| redis::display_key(&bytes))
            .unwrap_or_else(|| tab.collection.clone());
        let db = tab.database.clone();
        let runtime = self.workspace.runtime(tab_id);
        let info = runtime.and_then(|runtime| runtime.redis.info.clone());
        let error = runtime.and_then(|runtime| runtime.redis.info_error.clone());
        let now = Instant::now();
        let ttl_left = runtime.and_then(|runtime| runtime.redis.ttl_left(now));
        let key_type = info.as_ref().and_then(KeyInfo::key_type).or_else(|| self.redis_tab_key_type(tab_id));
        let type_name = info
            .as_ref()
            .map(|info| info.type_name.clone())
            .or_else(|| key_type.map(|key_type| key_type.as_str().to_owned()))
            .unwrap_or_else(|| "key".to_owned());
        let ttl_text = match (&info, ttl_left) {
            (None, _) => String::new(),
            (Some(_), None) => "No expiry".to_owned(),
            (Some(_), Some(left)) => format!("TTL {}", redis::format_ttl(left)),
        };
        let facts: Vec<String> = info
            .iter()
            .flat_map(|info| {
                [
                    info.length.map(|length| match key_type {
                        Some(KeyType::String) => format!("{length} bytes"),
                        Some(kind) => redis::count_label(length, kind),
                        None => format!("{length} items"),
                    }),
                    info.encoding.clone(),
                    info.memory.map(|memory| format_bytes(memory as u64)),
                ]
            })
            .flatten()
            .collect();
        let badge = div()
            .debug_selector(|| "redis-key-type".into())
            .px(px(6.0))
            .py(px(1.0))
            .rounded(px(4.0))
            .border_1()
            .border_color(type_color(key_type))
            .text_color(type_color(key_type))
            .text_size(px(10.5))
            .child(type_name);
        let ttl_button = Button::new(("redis-ttl", tab_id))
            .debug_selector(|| "redis-key-ttl".into())
            .compact()
            .ghost()
            .label(ttl_text)
            .tooltip("Set or remove the expiry")
            .disabled(info.is_none())
            .on_click(cx.listener(move |this, _, window, cx| this.open_redis_ttl_dialog(tab_id, window, cx)));
        let rename = Button::new(("redis-rename", tab_id))
            .debug_selector(|| "redis-key-rename".into())
            .compact()
            .ghost()
            .label("Rename")
            .disabled(info.is_none())
            .on_click(cx.listener(move |this, _, window, cx| this.open_redis_rename_dialog(tab_id, window, cx)));
        let delete = Button::new(("redis-delete", tab_id))
            .debug_selector(|| "redis-key-delete".into())
            .compact()
            .ghost()
            .label("Delete")
            .disabled(info.is_none())
            .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete_redis_key(tab_id, window, cx)));
        div()
            .debug_selector(|| "redis-key-toolbar".into())
            .h(px(theme::RESULT_TOOLBAR_HEIGHT))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .text_size(px(12.0))
            .child(badge)
            .child(
                div()
                    .debug_selector(|| "redis-key-name".into())
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(theme::text())
                    .child(format!("{display}  @db{db}")),
            )
            .child(div().flex_1())
            .children(error.map(|error| {
                div()
                    .debug_selector(|| "redis-key-error".into())
                    .text_color(theme::danger())
                    .child(error)
            }))
            .when(!facts.is_empty(), |this| {
                this.child(
                    div()
                        .debug_selector(|| "redis-key-facts".into())
                        .text_color(theme::text_muted())
                        .child(facts.join(" \u{00B7} ")),
                )
            })
            .child(ttl_button)
            .child(rename)
            .child(delete)
            .into_any_element()
    }

    /// The body of a string key tab: view switch, Save / Revert and the editor.
    pub(super) fn render_redis_string(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editor = self.redis_string_editor(tab_id, window, cx);
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return div().into_any_element();
        };
        let view = runtime.redis.view;
        let truncated = runtime.redis.string.as_ref().is_some_and(|value| value.truncated);
        let binary = runtime
            .redis
            .string
            .as_ref()
            .is_some_and(|value| std::str::from_utf8(&value.bytes).is_err());
        let editable = view != StringView::Hex && !truncated && !binary;
        let mode_button = |mode: StringView, label: &'static str, selector: &'static str| {
            Button::new((selector, tab_id))
                .debug_selector(move || selector.into())
                .compact()
                .ghost()
                .selected(view == mode)
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_redis_string_view(tab_id, mode, cx)))
        };
        let save = Button::new(("redis-string-save", tab_id))
            .debug_selector(|| "redis-string-save".into())
            .compact()
            .primary()
            .label("Save")
            .disabled(!editable)
            .on_click(cx.listener(move |this, _, window, cx| this.save_redis_string(tab_id, window, cx)));
        let revert = Button::new(("redis-string-revert", tab_id))
            .debug_selector(|| "redis-string-revert".into())
            .compact()
            .ghost()
            .label("Revert")
            .disabled(!editable)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.redis_ui.string_synced.remove(&tab_id);
                this.notify_tab(tab_id, cx);
            }));
        let note = if truncated {
            Some("Showing the first part of a long value: editing is disabled")
        } else if binary {
            Some("Not valid UTF-8: shown as hex, editing is disabled")
        } else {
            None
        };
        div()
            .debug_selector(|| "redis-string-view".into())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .h(px(32.0))
                    .flex_shrink_0()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .border_b_1()
                    .border_color(theme::divider())
                    .child(mode_button(StringView::Text, "Text", "redis-view-text"))
                    .child(mode_button(StringView::Json, "JSON", "redis-view-json"))
                    .child(mode_button(StringView::Hex, "Hex", "redis-view-hex"))
                    .children(note.map(|note| {
                        div().ml(px(8.0)).text_size(px(11.0)).text_color(theme::text_muted()).child(note)
                    }))
                    .child(div().flex_1())
                    .child(revert)
                    .child(save),
            )
            .child(
                gpui_component::input::Editor::new(&editor)
                    .flex_1()
                    .min_h_0()
                    .appearance(false)
                    .readonly(!editable),
            )
            .into_any_element()
    }

    fn set_redis_string_view(&mut self, tab_id: u64, view: StringView, cx: &mut Context<Self>) {
        if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
            runtime.redis.view = view;
        }
        self.notify_tab(tab_id, cx);
    }

    // ---- edits --------------------------------------------------------------

    /// Why a Redis edit on `connection` cannot start, if it cannot.
    fn redis_edit_refusal(&self, connection: &str, edit: &Edit) -> Option<String> {
        if let Some(message) = self.read_only_connection_message(connection, "Editing is") {
            return Some(message);
        }
        let supported = self
            .driver_capabilities(connection, ConnectionKind::Redis.driver_id())
            .is_none_or(|capabilities| capabilities.has(edit.capability()));
        (!supported).then(|| format!("The installed Redis driver cannot {} (update it in the Driver Manager)", edit.kind()))
    }

    /// Tells the user something about a Redis action (a toast, and the Error
    /// Log for warnings).
    pub(super) fn redis_notice(&mut self, connection: &str, message: String, warn: bool, cx: &mut Context<Self>) {
        if warn {
            self.report_warning(Area::Mutation, Some(connection), &message);
        }
        if let Some(window) = self.driver_runtime.window {
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| window.push_notification(message, cx));
            });
        }
    }

    /// Sends one edit of `key_raw` and, when the server applied it, refreshes
    /// what shows the key. `done` runs with the affected row count. A
    /// connection type that confirms data changes (Production) asks first for
    /// the edits that do not already ask (a removal does).
    pub(super) fn run_redis_edit(
        &mut self,
        connection: &str,
        db: &str,
        key_raw: &str,
        edit: Edit,
        done: EditDone,
        cx: &mut Context<Self>,
    ) {
        if let Some(message) = self.redis_edit_refusal(connection, &edit) {
            self.redis_notice(connection, message, true, cx);
            return;
        }
        let effective = self.effective_for(connection);
        if effective.confirm_data_change && !edit.destructive() {
            let Some(window) = self.driver_runtime.window else {
                self.redis_notice(connection, "Could not ask for confirmation: no window to ask in".into(), true, cx);
                return;
            };
            let subject = self.connection_subject(connection, &effective);
            let (connection, db, key_raw) = (connection.to_owned(), db.to_owned(), key_raw.to_owned());
            let what = format!("{} on `{}`", edit.kind(), redis::decode_key(&key_raw).map(|k| redis::display_key(&k)).unwrap_or_default());
            let slot = std::rc::Rc::new(std::cell::RefCell::new(Some((edit, done))));
            let root = cx.entity().downgrade();
            // The window cannot be updated while this entity is: ask once the
            // current update has finished.
            cx.defer(move |cx| {
                let _ = window.update(cx, |_, window, cx| {
                    let on_ok: FormOk = std::rc::Rc::new(move |this, _, cx| {
                        if let Some((edit, done)) = slot.borrow_mut().take() {
                            this.send_redis_edit(&connection, &db, &key_raw, edit, done, cx);
                        }
                        Ok(())
                    });
                    let _ = root.update(cx, |this, cx| {
                        this.confirm_redis_data_change(subject, what, on_ok, window, cx);
                    });
                });
            });
            return;
        }
        self.send_redis_edit(connection, db, key_raw, edit, done, cx);
    }

    fn confirm_redis_data_change(
        &mut self,
        subject: String,
        what: String,
        on_ok: FormOk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let on_ok = on_ok.clone();
            dialog
                .width(px(460.0))
                .title("Apply this change?")
                .child(
                    div()
                        .debug_selector(|| "redis-data-change".into())
                        .child(format!("{what} will run on {subject}.")),
                )
                .confirm()
                .footer(dialog_footer("Apply", false))
                .on_ok(move |_, _, cx| {
                    let on_ok = on_ok.clone();
                    let _ = root.update(cx, |this, cx| on_ok(this, Vec::new(), cx));
                    true
                })
        });
    }

    fn send_redis_edit(
        &mut self,
        connection: &str,
        db: &str,
        key_raw: &str,
        edit: Edit,
        done: EditDone,
        cx: &mut Context<Self>,
    ) {
        // The UI scenario / tests without a server: the fixture applies the edit.
        if self.scenario_ui.service.is_some() {
            let name = redis::decode_key(key_raw)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            let result = self
                .scenario_ui
                .service
                .as_mut()
                .and_then(|service| service.fixture_mut().redis_mut())
                .map_or(Err("the Redis fixture is not enabled".to_owned()), |redis| {
                    redis.apply(db.parse().unwrap_or(0), &name, &edit)
                });
            match result {
                Ok(affected) => done(self, affected, cx),
                Err(error) => {
                    self.report_error(Area::Mutation, Some(connection), &error);
                    self.redis_notice(connection, error, false, cx);
                }
            }
            return;
        }
        let Some(session) = self.driver_runtime.sessions.get(connection).cloned() else {
            self.redis_notice(connection, "The Redis connection is not open".into(), true, cx);
            return;
        };
        let operation = match session.begin_operation() {
            Ok(operation) => operation,
            Err(error) => {
                self.redis_notice(connection, error.to_string(), true, cx);
                return;
            }
        };
        let request = engine_driver_api::MutationRequest {
            operation_id: operation.id(),
            kind: edit.kind().into(),
            database: Some(db.to_owned()),
            object: None,
            identity: redis::key_identity(db, key_raw),
            payload: edit.payload(),
        };
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || {
                session
                    .mutate(&operation, request)
                    .map(|result| result.affected_rows)
                    .map_err(|error| error.to_string())
            })
            .await
        });
        let connection = connection.to_owned();
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(affected) => done(this, affected, cx),
                Err(error) => {
                    let message = crate::sessions::redact_secrets(&error);
                    this.report_error(Area::Mutation, Some(&connection), &message);
                    this.note_navigator_connection_lost(&connection, &message);
                    this.redis_notice(&connection, message, false, cx);
                }
            });
        })
        .detach();
    }

    /// The value tabs of `key_raw` on `connection` re-read the key.
    fn refresh_redis_key_tabs(&mut self, connection: &str, db: &str, key_raw: &str, cx: &mut Context<Self>) {
        let tabs: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.connection_id == connection
                    && tab.database == db
                    && tab.collection == key_raw
                    && tab.query_mode.is_redis_key()
            })
            .map(|tab| tab.id)
            .collect();
        for tab_id in tabs {
            if let Some(runtime) = self.workspace.runtime_mut(tab_id) {
                runtime.redis.info_loading = false;
            }
            self.refresh_tab_when_seen(tab_id, cx);
        }
    }

    /// Closes the value tabs of a key that no longer exists (deleted, renamed).
    fn close_redis_key_tabs(&mut self, connection: &str, db: &str, key_raw: &str, cx: &mut Context<Self>) {
        let tabs: Vec<u64> = self
            .workspace
            .tabs()
            .iter()
            .filter(|tab| {
                tab.connection_id == connection
                    && tab.database == db
                    && tab.collection == key_raw
                    && tab.query_mode.is_redis_key()
            })
            .map(|tab| tab.id)
            .collect();
        for tab_id in tabs {
            self.close_tab_unremembered(tab_id, cx);
        }
    }

    /// Applies an element edit made from a key tab and re-reads the key.
    fn edit_redis_element(&mut self, tab_id: u64, edit: Edit, cx: &mut Context<Self>) {
        let Some((connection, db, key_raw)) = self.redis_tab_key(tab_id) else {
            return;
        };
        let (c, d, k) = (connection.clone(), db.clone(), key_raw.clone());
        self.run_redis_edit(
            &connection,
            &db,
            &key_raw,
            edit,
            Box::new(move |this, _, cx| this.refresh_redis_key_tabs(&c, &d, &k, cx)),
            cx,
        );
    }

    /// Saves the string editor's text as the key's value.
    pub(super) fn save_redis_string(&mut self, tab_id: u64, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.redis_ui.string_editors.get(&tab_id).cloned() else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        self.edit_redis_element(tab_id, Edit::SetString { value: text }, cx);
    }

    // ---- dialogs ---------------------------------------------------------------

    #[allow(clippy::too_many_arguments)] // each parameter is one input of the operation; a parameter struct would only rename the list
    /// Opens a form dialog: `fields` in order, `on_ok` receives their values.
    fn open_redis_form(
        &mut self,
        title: String,
        intro: Option<String>,
        ok_text: &'static str,
        danger: bool,
        fields: Vec<FormField>,
        on_ok: FormOk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entities: Vec<(&'static str, FieldEntity)> = fields
            .into_iter()
            .map(|field| {
                let entity = match field.kind {
                    FieldKind::Line => FieldEntity::Line(cx.new(|cx| {
                        InputState::new(window, cx)
                            .placeholder(field.placeholder)
                            .default_value(field.initial.clone())
                    })),
                    FieldKind::Text => FieldEntity::Text(cx.new(|cx| {
                        TextareaState::new(window, cx)
                            .auto_grow(2, 8)
                            .soft_wrap(true)
                            .placeholder(field.placeholder)
                            .default_value(field.initial.clone())
                    })),
                    FieldKind::Choice(options) => FieldEntity::Choice(cx.new(|cx| {
                        let row = options.iter().position(|option| *option == field.initial).unwrap_or(0);
                        SelectState::new(options, Some(IndexPath::default().row(row)), window, cx)
                    })),
                };
                (field.label, entity)
            })
            .collect();
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let on_ok = on_ok.clone();
            let read = entities.clone();
            let mut body = div().debug_selector(|| "redis-form".into()).flex().flex_col().gap(px(10.0));
            if let Some(intro) = &intro {
                body = body.child(div().text_size(px(12.0)).text_color(theme::text_muted()).child(intro.clone()));
            }
            for (index, (label, entity)) in entities.iter().enumerate() {
                let control = match entity {
                    FieldEntity::Line(state) => Input::new(state).into_any_element(),
                    FieldEntity::Text(state) => Textarea::new(state).into_any_element(),
                    FieldEntity::Choice(state) => Select::new(state).into_any_element(),
                };
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(4.0))
                        .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(*label))
                        .child(
                            div()
                                .debug_selector(move || format!("redis-field-{index}"))
                                .child(control),
                        ),
                );
            }
            dialog
                .width(px(460.0))
                .title(title.clone())
                .child(body)
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(ok_text)
                        .ok_variant(if danger { ButtonVariant::Danger } else { ButtonVariant::Primary }),
                )
                .confirm()
                .footer(dialog_footer(ok_text, danger))
                .on_ok(move |_, window, cx| {
                    let values: Vec<String> = read
                        .iter()
                        .map(|(_, entity)| match entity {
                            FieldEntity::Line(state) => state.read(cx).value().to_string(),
                            FieldEntity::Text(state) => state.read(cx).value().to_string(),
                            FieldEntity::Choice(state) => {
                                state.read(cx).selected_value().cloned().unwrap_or_default()
                            }
                        })
                        .collect();
                    let on_ok = on_ok.clone();
                    match root.update(cx, |this, cx| on_ok(this, values, cx)) {
                        Ok(Err(message)) => {
                            window.push_notification(message, cx);
                            false
                        }
                        _ => true,
                    }
                })
        });
    }

    /// A destructive action asks first.
    fn confirm_redis(
        &mut self,
        title: String,
        message: String,
        ok_text: &'static str,
        on_ok: FormOk,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let on_ok = on_ok.clone();
            dialog
                .width(px(460.0))
                .title(title.clone())
                .child(div().debug_selector(|| "redis-confirm".into()).child(message.clone()))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(ok_text)
                        .ok_variant(ButtonVariant::Danger),
                )
                .confirm()
                .footer(dialog_footer(ok_text, true))
                .on_ok(move |_, _, cx| {
                    let on_ok = on_ok.clone();
                    let _ = root.update(cx, |this, cx| on_ok(this, Vec::new(), cx));
                    true
                })
        });
    }

    pub(super) fn open_redis_ttl_dialog(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, db, key_raw)) = self.redis_tab_key(tab_id) else {
            return;
        };
        let current = self
            .workspace
            .runtime(tab_id)
            .and_then(|runtime| runtime.redis.ttl_left(Instant::now()))
            .filter(|left| *left > 0)
            .map(|left| left.to_string())
            .unwrap_or_default();
        let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
            let ttl = redis::parse_ttl(values.first().map_or("", String::as_str))?;
            let edit = Edit::SetTtl(ttl);
            if let Some(message) = this.redis_edit_refusal(&connection, &edit) {
                return Err(message);
            }
            let (c, d, k) = (connection.clone(), db.clone(), key_raw.clone());
            this.run_redis_edit(
                &connection,
                &db,
                &key_raw,
                edit,
                Box::new(move |this, affected, cx| {
                    if affected == 0 {
                        this.redis_notice(&c, "The key no longer exists".into(), true, cx);
                    }
                    this.refresh_redis_key_tabs(&c, &d, &k, cx);
                }),
                cx,
            );
            Ok(())
        });
        self.open_redis_form(
            "Expiry".into(),
            Some("Seconds, or 5m, 2h, 1d. Empty removes the expiry.".into()),
            "Set",
            false,
            vec![FormField::line("Time to live", current, "e.g. 300 or 1h; empty = no expiry")],
            on_ok,
            window,
            cx,
        );
    }

    pub(super) fn open_redis_rename_dialog(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, db, key_raw)) = self.redis_tab_key(tab_id) else {
            return;
        };
        let display = redis::decode_key(&key_raw).map(|bytes| redis::display_key(&bytes)).unwrap_or_default();
        self.open_redis_rename_form(&connection, &db, &key_raw, &display, window, cx);
    }

    /// Rename dialog for a key given by its sidebar row or its tab.
    pub(super) fn open_redis_rename_form(
        &mut self,
        connection: &str,
        db: &str,
        key_raw: &str,
        display: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (connection, db, key_raw) = (connection.to_owned(), db.to_owned(), key_raw.to_owned());
        let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
            let new_key = values.first().cloned().unwrap_or_default();
            if new_key.is_empty() {
                return Err("Enter the new key name".into());
            }
            let edit = Edit::Rename { new_key: new_key.clone(), overwrite: false };
            if let Some(message) = this.redis_edit_refusal(&connection, &edit) {
                return Err(message);
            }
            let (c, d, k) = (connection.clone(), db.clone(), key_raw.clone());
            this.run_redis_edit(
                &connection,
                &db,
                &key_raw,
                edit,
                Box::new(move |this, affected, cx| {
                    if affected == 0 {
                        this.redis_notice(&c, format!("A key named `{new_key}` already exists"), true, cx);
                        return;
                    }
                    this.close_redis_key_tabs(&c, &d, &k, cx);
                    this.reload_redis_database(&c, &d, cx);
                    this.open_redis_key_tab(&c, &d, &redis::encode_key(new_key.as_bytes()), &new_key, cx);
                }),
                cx,
            );
            Ok(())
        });
        self.open_redis_form(
            "Rename key".into(),
            Some("An existing key with the new name is left alone.".into()),
            "Rename",
            false,
            vec![FormField::line("New name", display, "key name")],
            on_ok,
            window,
            cx,
        );
    }

    pub(super) fn confirm_delete_redis_key(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((connection, db, key_raw)) = self.redis_tab_key(tab_id) else {
            return;
        };
        let display = redis::decode_key(&key_raw).map(|bytes| redis::display_key(&bytes)).unwrap_or_default();
        self.confirm_delete_redis_key_named(&connection, &db, &key_raw, &display, window, cx);
    }

    pub(super) fn confirm_delete_redis_key_named(
        &mut self,
        connection: &str,
        db: &str,
        key_raw: &str,
        display: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(message) = self.read_only_connection_message(connection, "Deleting is") {
            self.redis_notice(connection, message, true, cx);
            return;
        }
        let message = format!("`{display}` in db{db} will be deleted. This cannot be undone.");
        let (connection, db, key_raw) = (connection.to_owned(), db.to_owned(), key_raw.to_owned());
        let on_ok: FormOk = std::rc::Rc::new(move |this, _, cx| {
            let (c, d, k) = (connection.clone(), db.clone(), key_raw.clone());
            this.run_redis_edit(
                &connection,
                &db,
                &key_raw,
                Edit::DeleteKey,
                Box::new(move |this, affected, cx| {
                    let node = NavigatorNodeId::Provider { connection: c.clone(), node: format!("key:{d}:{k}") };
                    this.navigator.remove_redis_key(&node);
                    this.navigator_ui.generation = this.navigator.generation();
                    if affected == 0 {
                        this.redis_notice(&c, "The key was already gone".into(), false, cx);
                    }
                    this.close_redis_key_tabs(&c, &d, &k, cx);
                    cx.notify();
                }),
                cx,
            );
            Ok(())
        });
        self.confirm_redis(
            "Delete key?".into(),
            message,
            "Delete",
            on_ok,
            window,
            cx,
        );
    }

    // ---- elements of a collection -------------------------------------------------

    /// The selected row of a key tab as its page row index.
    fn redis_selected_row(&self, tab_id: u64) -> Option<usize> {
        let display = self
            .view(tab_id)
            .grid_ui
            .selection
            .borrow()
            .range
            .map(|range| range.normalized().start.row)?;
        Some(self.grid(tab_id).source_row(display))
    }

    fn redis_cell_bytes(&self, tab_id: u64, row: usize, column: usize) -> Option<Vec<u8>> {
        match self.grid(tab_id).source() {
            GridContentSource::Arrow(source) => source.binary_cell(row, column).map(<[u8]>::to_vec),
            _ => None,
        }
    }

    /// `text` of a byte cell when it is UTF-8 (a form edits text only).
    fn redis_cell_text(&self, tab_id: u64, row: usize, column: usize) -> Option<String> {
        match self.redis_cell_bytes(tab_id, row, column) {
            Some(bytes) => String::from_utf8(bytes).ok(),
            // Text columns (a stream entry id) are not byte strings.
            None => self.grid(tab_id).full_cell_text(CellPosition { row, column }),
        }
    }

    /// The footer's Add button.
    pub(super) fn open_redis_add_dialog(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key_type) = self.redis_tab_key_type(tab_id) else {
            return;
        };
        let connection = self.redis_tab_key(tab_id).map(|(connection, ..)| connection).unwrap_or_default();
        if let Some(message) = self.read_only_connection_message(&connection, "Editing is") {
            self.redis_notice(&connection, message, true, cx);
            return;
        }
        let (title, fields): (&str, Vec<FormField>) = match key_type {
            KeyType::String => {
                self.redis_notice(&connection, "A string has a single value: edit it in the editor".into(), false, cx);
                return;
            }
            KeyType::Hash => (
                "Add field",
                vec![FormField::line("Field", "", "field name"), FormField::text("Value", "", "value")],
            ),
            KeyType::List => (
                "Push item",
                vec![FormField::choice("Position", &["Tail (RPUSH)", "Head (LPUSH)"]), FormField::text("Value", "", "value")],
            ),
            KeyType::Set => ("Add member", vec![FormField::text("Member", "", "member")]),
            KeyType::ZSet => (
                "Add member",
                vec![FormField::line("Member", "", "member"), FormField::line("Score", "0", "number")],
            ),
            KeyType::Stream => (
                "Add entry",
                vec![
                    FormField::line("Entry ID", "*", "* = generate"),
                    FormField::text("Fields", "", "one field=value per line"),
                ],
            ),
        };
        let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
            let edit = match key_type {
                KeyType::String => return Ok(()),
                KeyType::Hash => {
                    if values[0].is_empty() {
                        return Err("Enter the field name".into());
                    }
                    Edit::HashSet { field: values[0].clone(), value: values[1].clone() }
                }
                KeyType::List => Edit::ListPush { value: values[1].clone(), left: values[0].starts_with("Head") },
                KeyType::Set => {
                    if values[0].is_empty() {
                        return Err("Enter a member".into());
                    }
                    Edit::SetAdd { member: values[0].clone() }
                }
                KeyType::ZSet => {
                    let score: f64 = values[1].trim().parse().map_err(|_| "The score must be a number".to_owned())?;
                    if values[0].is_empty() || score.is_nan() {
                        return Err("Enter a member and a numeric score".into());
                    }
                    Edit::ZAdd { member: values[0].clone(), score }
                }
                KeyType::Stream => {
                    let fields = parse_stream_fields(&values[1])?;
                    Edit::StreamAdd { fields }
                }
            };
            if let Some(message) = this.redis_edit_refusal(&this_connection(this, tab_id), &edit) {
                return Err(message);
            }
            this.edit_redis_element(tab_id, edit, cx);
            Ok(())
        });
        self.open_redis_form(title.into(), None, "Add", false, fields, on_ok, window, cx);
    }

    /// The footer's Edit button / a double click on a cell.
    pub(super) fn open_redis_element_editor(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key_type) = self.redis_tab_key_type(tab_id) else {
            return;
        };
        let connection = this_connection(self, tab_id);
        if let Some(message) = self.read_only_connection_message(&connection, "Editing is") {
            self.redis_notice(&connection, message, true, cx);
            return;
        }
        if key_type == KeyType::String {
            return;
        }
        let Some(row) = self.redis_selected_row(tab_id) else {
            self.redis_notice(&connection, "Select a row to edit".into(), false, cx);
            return;
        };
        let not_text = |this: &mut Self, cx: &mut Context<Self>| {
            this.redis_notice(&connection, "This value is not valid UTF-8: it cannot be edited as text".into(), true, cx);
        };
        match key_type {
            KeyType::Hash => {
                let (Some(field), Some(value)) = (self.redis_cell_text(tab_id, row, 0), self.redis_cell_text(tab_id, row, 1)) else {
                    return not_text(self, cx);
                };
                let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
                    this.edit_redis_element(tab_id, Edit::HashSet { field: field.clone(), value: values[0].clone() }, cx);
                    Ok(())
                });
                let title = "Edit field".to_string();
                let intro = Some(format!("Field: {}", self.redis_cell_text(tab_id, row, 0).unwrap_or_default()));
                self.open_redis_form(title, intro, "Save", false, vec![FormField::text("Value", value, "value")], on_ok, window, cx);
            }
            KeyType::List => {
                let Some(value) = self.redis_cell_text(tab_id, row, 1) else {
                    return not_text(self, cx);
                };
                let index = row as i64
                    + self
                        .workspace
                        .runtime(tab_id)
                        .map_or(0, |runtime| runtime.pagination.offset_of(runtime.pagination.page()));
                let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
                    this.edit_redis_element(tab_id, Edit::ListSet { index, value: values[0].clone() }, cx);
                    Ok(())
                });
                self.open_redis_form(
                    "Edit item".into(),
                    Some(format!("Index: {index}")),
                    "Save",
                    false,
                    vec![FormField::text("Value", value, "value")],
                    on_ok,
                    window,
                    cx,
                );
            }
            KeyType::ZSet => {
                let (Some(member), Some(score)) = (self.redis_cell_text(tab_id, row, 0), self.grid(tab_id).cell_text(CellPosition { row, column: 1 })) else {
                    return not_text(self, cx);
                };
                let shown = member.clone();
                let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
                    let score: f64 = values[0].trim().parse().map_err(|_| "The score must be a number".to_owned())?;
                    if score.is_nan() {
                        return Err("The score must be a number".into());
                    }
                    this.edit_redis_element(tab_id, Edit::ZAdd { member: member.clone(), score }, cx);
                    Ok(())
                });
                self.open_redis_form(
                    "Edit score".into(),
                    Some(format!("Member: {shown}")),
                    "Save",
                    false,
                    vec![FormField::line("Score", score.to_string(), "number")],
                    on_ok,
                    window,
                    cx,
                );
            }
            KeyType::Set | KeyType::Stream | KeyType::String => {
                self.redis_notice(
                    &connection,
                    format!("A {} {} is replaced by removing it and adding another", key_type.as_str(), key_type.element()),
                    false,
                    cx,
                );
            }
        }
    }

    /// The footer's Delete button: removes the selected element.
    pub(super) fn confirm_delete_redis_element(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key_type) = self.redis_tab_key_type(tab_id) else {
            return;
        };
        let connection = this_connection(self, tab_id);
        if let Some(message) = self.read_only_connection_message(&connection, "Deleting is") {
            self.redis_notice(&connection, message, true, cx);
            return;
        }
        let Some(row) = self.redis_selected_row(tab_id) else {
            self.redis_notice(&connection, "Select a row to remove".into(), false, cx);
            return;
        };
        let (edit, what) = match key_type {
            KeyType::String => {
                self.confirm_delete_redis_key(tab_id, window, cx);
                return;
            }
            KeyType::Hash => match self.redis_cell_bytes(tab_id, row, 0) {
                Some(field) => (
                    Edit::HashDelete { field_raw: redis::encode_key(&field) },
                    format!("field `{}`", redis::display_key(&field)),
                ),
                None => return,
            },
            KeyType::List => match self.redis_cell_bytes(tab_id, row, 1) {
                Some(value) => (
                    Edit::ListRemove { value_raw: redis::encode_key(&value) },
                    format!("the first item equal to `{}`", redis::display_key(&value)),
                ),
                None => return,
            },
            KeyType::Set => match self.redis_cell_bytes(tab_id, row, 0) {
                Some(member) => (
                    Edit::SetRemove { member_raw: redis::encode_key(&member) },
                    format!("member `{}`", redis::display_key(&member)),
                ),
                None => return,
            },
            KeyType::ZSet => match self.redis_cell_bytes(tab_id, row, 0) {
                Some(member) => (
                    Edit::ZRemove { member_raw: redis::encode_key(&member) },
                    format!("member `{}`", redis::display_key(&member)),
                ),
                None => return,
            },
            KeyType::Stream => match self.redis_cell_text(tab_id, row, 0) {
                Some(id) => (Edit::StreamDelete { entry_id: id.clone() }, format!("entry {id}")),
                None => return,
            },
        };
        let on_ok: FormOk = std::rc::Rc::new(move |this, _, cx| {
            this.edit_redis_element(tab_id, edit.clone(), cx);
            Ok(())
        });
        self.confirm_redis(
            format!("Remove {}?", key_type.element()),
            format!("This removes {what} from the key."),
            "Remove",
            on_ok,
            window,
            cx,
        );
    }

    // ---- new key ------------------------------------------------------------------

    /// "New Key..." on a database row.
    pub(super) fn open_redis_new_key_dialog(
        &mut self,
        connection: &str,
        db: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(message) = self.read_only_connection_message(connection, "Creating a key is") {
            self.redis_notice(connection, message, true, cx);
            return;
        }
        let (connection, db) = (connection.to_owned(), db.to_owned());
        let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
            let (key, edit, ttl) = new_key_edit(&values)?;
            if let Some(message) = this.redis_edit_refusal(&connection, &edit) {
                return Err(message);
            }
            let key_raw = redis::encode_key(key.as_bytes());
            let (c, d, k, shown) = (connection.clone(), db.clone(), key_raw.clone(), key.clone());
            this.run_redis_edit(
                &connection,
                &db,
                &key_raw,
                edit,
                Box::new(move |this, _, cx| {
                    let finish = {
                        let (c, d, k, shown) = (c.clone(), d.clone(), k.clone(), shown.clone());
                        move |this: &mut AppRoot, cx: &mut Context<AppRoot>| {
                            this.reload_redis_database(&c, &d, cx);
                            this.open_redis_key_tab(&c, &d, &k, &shown, cx);
                        }
                    };
                    match ttl {
                        Some(Ttl::Seconds(seconds)) => this.run_redis_edit(
                            &c,
                            &d,
                            &k,
                            Edit::SetTtl(Ttl::Seconds(seconds)),
                            Box::new(move |this, _, cx| finish(this, cx)),
                            cx,
                        ),
                        _ => finish(this, cx),
                    }
                }),
                cx,
            );
            Ok(())
        });
        self.open_redis_form(
            "New key".into(),
            Some("string: Value. hash: Name = field. list: Value = first item. set: Name = member. sorted set: Name = member, Value = score. stream: Value = field=value lines.".into()),
            "Create",
            false,
            vec![
                FormField::line("Key", "", "key name, e.g. user:42"),
                FormField::choice("Type", &["string", "hash", "list", "set", "zset", "stream"]),
                FormField::line("Name (field / member)", "", "hash, set and sorted set"),
                FormField::text("Value", "", "value"),
                FormField::line("Expiry (optional)", "", "e.g. 1h"),
            ],
            on_ok,
            window,
            cx,
        );
    }

    // ---- key filter -----------------------------------------------------------------

    /// "Filter Keys..." on a database row: pattern, type and grouping.
    pub(super) fn open_redis_filter_dialog(
        &mut self,
        database: NavigatorNodeId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.navigator.redis_filter(&database);
        let types: Vec<&str> = std::iter::once("any").chain(KeyType::ALL.iter().map(|kind| kind.as_str())).collect();
        let target = database.clone();
        let on_ok: FormOk = std::rc::Rc::new(move |this, values, cx| {
            let filter = KeyFilter {
                pattern: values[0].trim().to_owned(),
                key_type: KeyType::parse(&values[1]),
                group: values[2] == "Group by ':'",
            };
            this.apply_redis_filter(&target, filter, cx);
            Ok(())
        });
        // Each select reopens on the current choice.
        let mut kind = FormField::choice("Type", &types);
        kind.initial = current.key_type.map_or("any", KeyType::as_str).to_owned();
        let mut show = FormField::choice("Show keys", &["Group by ':'", "Flat list"]);
        show.initial = if current.group { "Group by ':'" } else { "Flat list" }.to_owned();
        let fields = vec![
            FormField::line("Pattern (glob)", current.pattern.clone(), "e.g. user:*  (empty = all keys)"),
            kind,
            show,
        ];
        self.open_redis_form(
            "Filter keys".into(),
            Some("The server matches the pattern while scanning; the type is checked by the server too.".into()),
            "Apply",
            false,
            fields,
            on_ok,
            window,
            cx,
        );
    }

    /// Stores a database's filter and reloads its keys.
    pub(super) fn apply_redis_filter(&mut self, database: &NavigatorNodeId, filter: KeyFilter, cx: &mut Context<Self>) {
        self.navigator.set_redis_filter(database, filter);
        self.reload_redis_database_node(database, cx);
    }

    /// Reloads the keys of database `db` (after a create / rename).
    pub(super) fn reload_redis_database(&mut self, connection: &str, db: &str, cx: &mut Context<Self>) {
        let node = NavigatorNodeId::Provider { connection: connection.to_owned(), node: format!("database:{db}") };
        self.reload_redis_database_node(&node, cx);
    }

    fn reload_redis_database_node(&mut self, node: &NavigatorNodeId, cx: &mut Context<Self>) {
        if !self.navigator.is_redis_database(node) {
            return;
        }
        let was_open = self.navigator.visible_index(node).is_some();
        self.navigator.reset_node(node);
        if was_open {
            if let Some(index) = self.navigator.visible_index(node) {
                if let Some(row) = self.navigator.visible_row_at(index) {
                    self.handle_navigator_disclosure(&row, cx);
                }
            }
        }
        self.navigator_ui.generation = self.navigator.generation();
        self.notify_chrome(cx);
    }
}

/// Cancel / OK of a Redis dialog, with selectors the interaction tests click.
fn dialog_footer(ok_text: &'static str, danger: bool) -> gpui_component::dialog::DialogFooter {
    let ok = Button::new("redis-form-ok").label(ok_text);
    gpui_component::dialog::DialogFooter::new()
        .child(
            div()
                .debug_selector(|| "redis-form-cancel".into())
                .child(gpui_component::dialog::DialogClose::new().trigger(|button| button.label("Cancel"))),
        )
        .child(
            div()
                .debug_selector(|| "redis-form-ok".into())
                .child(gpui_component::dialog::DialogAction::new().child(if danger {
                    ok.danger()
                } else {
                    ok.primary()
                })),
        )
}

/// The connection a tab belongs to (`""` for no tab).
fn this_connection(root: &AppRoot, tab_id: u64) -> String {
    root.workspace.tab(tab_id).map(|tab| tab.connection_id.clone()).unwrap_or_default()
}

/// The first edit of a new key from the New Key dialog's values (`key`, `type`,
/// `name`, `value`, `expiry`): the key name, what creates it, and its expiry.
pub(super) fn new_key_edit(values: &[String]) -> Result<(String, Edit, Option<Ttl>), String> {
    let key = values[0].clone();
    if key.is_empty() {
        return Err("Enter a key name".into());
    }
    let key_type = KeyType::parse(&values[1]).unwrap_or(KeyType::String);
    let (name, value) = (values[2].clone(), values[3].clone());
    let edit = match key_type {
        KeyType::String => Edit::SetString { value },
        KeyType::Hash => {
            if name.is_empty() {
                return Err("A hash needs a field name".into());
            }
            Edit::HashSet { field: name, value }
        }
        KeyType::List => Edit::ListPush { value, left: false },
        KeyType::Set => {
            if name.is_empty() {
                return Err("A set needs a first member".into());
            }
            Edit::SetAdd { member: name }
        }
        KeyType::ZSet => {
            let score: f64 =
                value.trim().parse().map_err(|_| "A sorted set needs a numeric score".to_owned())?;
            if name.is_empty() || score.is_nan() {
                return Err("A sorted set needs a member and a numeric score".into());
            }
            Edit::ZAdd { member: name, score }
        }
        KeyType::Stream => Edit::StreamAdd { fields: parse_stream_fields(&value)? },
    };
    let ttl = match values[4].trim() {
        "" => None,
        text => Some(redis::parse_ttl(text)?),
    };
    Ok((key, edit, ttl))
}

/// `field=value` lines of the Add Entry dialog.
fn parse_stream_fields(text: &str) -> Result<Vec<(String, String)>, String> {
    let fields: Vec<(String, String)> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split_once('=')
                .map(|(field, value)| (field.trim().to_owned(), value.trim().to_owned()))
                .filter(|(field, _)| !field.is_empty())
                .ok_or_else(|| format!("`{line}` is not field=value"))
        })
        .collect::<Result<_, _>>()?;
    if fields.is_empty() {
        return Err("Enter at least one field=value line".into());
    }
    Ok(fields)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The context menu of a Redis sidebar row.
pub(super) fn build_redis_node_menu(
    menu: gpui_component::menu::PopupMenu,
    root: gpui::WeakEntity<AppRoot>,
    target: RedisTarget,
) -> gpui_component::menu::PopupMenu {
    let item = |label: &'static str, root: &gpui::WeakEntity<AppRoot>, action: MenuAction| {
        let root = root.clone();
        let action = std::rc::Rc::new(action);
        PopupMenuItem::new(label).on_click(move |_, window, app| {
            let action = action.clone();
            let _ = root.update(app, |this, cx| action(this, window, cx));
        })
    };
    match target {
        RedisTarget::Key { connection, db, key_raw, display, node } => {
            let open = {
                let node = node.clone();
                item("Open", &root, Box::new(move |this, _, cx| {
                    this.open_redis_key_node(&node, cx);
                }))
            };
            let copy = {
                let display = display.clone();
                item("Copy Key Name", &root, Box::new(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(display.clone()));
                }))
            };
            let rename = {
                let (connection, db, key_raw, display) = (connection.clone(), db.clone(), key_raw.clone(), display.clone());
                item("Rename...", &root, Box::new(move |this, window, cx| {
                    this.open_redis_rename_form(&connection, &db, &key_raw, &display, window, cx);
                }))
            };
            let ttl = {
                let (connection, db, key_raw, display) = (connection.clone(), db.clone(), key_raw.clone(), display.clone());
                item("Set Expiry...", &root, Box::new(move |this, window, cx| {
                    let tab_id = this.open_redis_key_tab(&connection, &db, &key_raw, &display, cx);
                    this.open_redis_ttl_dialog(tab_id, window, cx);
                }))
            };
            let delete = item("Delete...", &root, Box::new(move |this, window, cx| {
                this.confirm_delete_redis_key_named(&connection, &db, &key_raw, &display, window, cx);
            }));
            menu.item(open).item(copy).separator().item(rename).item(ttl).separator().item(delete)
        }
        RedisTarget::Namespace { connection, db, prefix } => {
            let copy = {
                let prefix = prefix.clone();
                item("Copy Prefix", &root, Box::new(move |_, _, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(prefix.clone()));
                }))
            };
            let only = item("Show Only This Namespace", &root, Box::new(move |this, _, cx| {
                let node = NavigatorNodeId::Provider { connection: connection.clone(), node: format!("database:{db}") };
                let mut filter = this.navigator.redis_filter(&node);
                filter.pattern = redis::subtree_pattern(&prefix);
                this.apply_redis_filter(&node, filter, cx);
            }));
            menu.item(copy).item(only)
        }
        RedisTarget::Database { connection, db, node } => {
            let new_key = {
                let (connection, db) = (connection.clone(), db.clone());
                item("New Key...", &root, Box::new(move |this, window, cx| {
                    this.open_redis_new_key_dialog(&connection, &db, window, cx);
                }))
            };
            let filter = {
                let node = node.clone();
                item("Filter Keys...", &root, Box::new(move |this, window, cx| {
                    this.open_redis_filter_dialog(node.clone(), window, cx);
                }))
            };
            let clear = {
                let node = node.clone();
                item("Clear Filter", &root, Box::new(move |this, _, cx| {
                    this.apply_redis_filter(&node, KeyFilter::default(), cx);
                }))
            };
            let console = {
                let node = node.clone();
                item("New Query", &root, Box::new(move |this, _, cx| {
                    this.open_redis_console_for_node(&node);
                    this.notify_chrome(cx);
                }))
            };
            let info = {
                let (connection, db) = (connection.clone(), db.clone());
                item("Server Info", &root, Box::new(move |this, _, cx| {
                    let tab = this.open_redis_console_tab(&connection, &db);
                    if let Some(tab_state) = this.workspace.tab_mut(tab) {
                        tab_state.sql = "INFO".into();
                    }
                    this.run_tab_query(tab, cx);
                    this.notify_chrome(cx);
                }))
            };
            let refresh = {
                let (connection, db) = (connection.clone(), db.clone());
                item("Refresh", &root, Box::new(move |this, _, cx| {
                    this.reload_redis_database(&connection, &db, cx);
                }))
            };
            menu.item(new_key).item(filter).item(clear).separator().item(console).item(info).item(refresh)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_fields_are_field_equals_value_lines() {
        assert_eq!(
            parse_stream_fields("a=1\n\n b = two words \nc=").unwrap(),
            [("a".into(), "1".into()), ("b".into(), "two words".into()), ("c".into(), "".into())]
        );
        assert!(parse_stream_fields("").is_err());
        assert!(parse_stream_fields("no equals").unwrap_err().contains("field=value"));
        assert!(parse_stream_fields("=v").is_err());
    }

    #[test]
    fn the_new_key_dialog_builds_the_first_edit_of_every_type() {
        let values = |key: &str, kind: &str, name: &str, value: &str, ttl: &str| {
            [key, kind, name, value, ttl].map(String::from).to_vec()
        };
        let edit = |kind, name, value| new_key_edit(&values("k", kind, name, value, "")).map(|(_, edit, _)| edit);
        assert_eq!(edit("string", "", "v"), Ok(Edit::SetString { value: "v".into() }));
        assert_eq!(edit("hash", "f", "v"), Ok(Edit::HashSet { field: "f".into(), value: "v".into() }));
        assert_eq!(edit("list", "", "a"), Ok(Edit::ListPush { value: "a".into(), left: false }));
        assert_eq!(edit("set", "m", ""), Ok(Edit::SetAdd { member: "m".into() }));
        assert_eq!(edit("zset", "m", " 2.5 "), Ok(Edit::ZAdd { member: "m".into(), score: 2.5 }));
        assert_eq!(
            edit("stream", "", "a=1\nb=2"),
            Ok(Edit::StreamAdd { fields: vec![("a".into(), "1".into()), ("b".into(), "2".into())] })
        );
        for refused in [edit("hash", "", "v"), edit("set", "", ""), edit("zset", "m", "x"), edit("zset", "", "1"), edit("stream", "", "")] {
            assert!(refused.is_err(), "{refused:?}");
        }
        assert!(new_key_edit(&values("", "string", "", "v", "")).unwrap_err().contains("key name"));
        let (_, _, ttl) = new_key_edit(&values("k", "string", "", "v", "10m")).unwrap();
        assert_eq!(ttl, Some(Ttl::Seconds(600)));
        assert!(new_key_edit(&values("k", "string", "", "v", "0")).is_err(), "a TTL of 0 is refused");
    }

    #[test]
    fn byte_sizes_and_default_db_read_naturally() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(default_db("redis://localhost:6379/7"), "7");
        assert_eq!(default_db("rediss://u@h"), "0");
        assert_eq!(default_db("redis://h/notanumber"), "0");
    }
}
