//! Source tabs: the read-only source of one schema object (a function, a
//! procedure, a trigger, a sequence, a type, an event or a materialized view).
//!
//! The tab persists its object only (`Tab::source`); the text is fetched with the
//! driver's `describe_object` each time the tab is first shown, so a restored tab
//! shows what the server has now. The text is shown in the same read-only code
//! editor as the Properties DDL (SQL highlighting, line numbers, selection and
//! copy), with Copy, Open in SQL Editor and Refresh in the header.

use super::*;
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef, SchemaObjectSource};

/// Where a source tab's text stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SourceStatus {
    /// The connection is opening; the fetch starts once it did (`epoch` is the
    /// connect attempt waited for).
    Connecting { epoch: u64 },
    Loading,
    Ready,
    Failed(String),
}

/// One source tab's text, editor and in-flight fetch.
pub(super) struct SourceTabState {
    pub(super) status: SourceStatus,
    /// The source as fetched (what Copy and Open in SQL Editor hand out).
    pub(super) text: String,
    /// Read-only editor over `text`, built on the first render after the fetch.
    pub(super) editor: Option<gpui::Entity<EditorState>>,
    /// The editor shows `text` as of this revision.
    editor_revision: u64,
    revision: u64,
    /// Orders fetches: an answer of an older one is dropped.
    fetch: u64,
    task: Option<Task<()>>,
}

impl SourceTabState {
    fn new(status: SourceStatus) -> Self {
        Self {
            status,
            text: String::new(),
            editor: None,
            editor_revision: 0,
            revision: 0,
            fetch: 0,
            task: None,
        }
    }
}

/// Why a source could not be fetched.
#[derive(Debug)]
enum SourceError {
    NotConnected,
    Request(serde_json::Error),
    Driver(engine_driver_host::LoaderError),
    Unreadable(serde_json::Error),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConnected => f.write_str("The connection is not open"),
            Self::Request(error) => std::fmt::Display::fmt(error, f),
            Self::Driver(error) => std::fmt::Display::fmt(error, f),
            Self::Unreadable(error) => write!(f, "Unreadable source: {error}"),
        }
    }
}

impl std::error::Error for SourceError {}

impl SourceError {
    /// The driver said the session is gone (`<area>.not_connected`).
    fn is_connection_lost(&self) -> bool {
        matches!(
            self,
            Self::Driver(engine_driver_host::LoaderError::DriverRejected { code, .. })
                if crate::sessions::is_connection_lost_code(code)
        )
    }
}

impl From<engine_driver_host::LoaderError> for SourceError {
    fn from(error: engine_driver_host::LoaderError) -> Self {
        Self::Driver(error)
    }
}

/// "Function", "Materialized view", ...: the header's name for a kind.
pub(super) fn kind_title(kind: SchemaObjectKind) -> &'static str {
    match kind {
        SchemaObjectKind::MaterializedView => "Materialized view",
        SchemaObjectKind::Function => "Function",
        SchemaObjectKind::Procedure => "Procedure",
        SchemaObjectKind::Sequence => "Sequence",
        SchemaObjectKind::Trigger => "Trigger",
        SchemaObjectKind::Type => "Type",
        SchemaObjectKind::Event => "Event",
    }
}

/// `schema.name(args)` / `database.name`: where the object lives and what it is called.
fn qualified_name(source: &SchemaObjectRef) -> String {
    let place = source
        .schema
        .as_deref()
        .or(source.database.as_deref())
        .filter(|place| !place.is_empty());
    let name = match (&source.arguments, &source.table, source.kind) {
        (Some(arguments), _, _) => format!("{}({arguments})", source.name),
        (None, Some(table), SchemaObjectKind::Trigger) => format!("{} on {table}", source.name),
        _ => source.name.clone(),
    };
    match place {
        Some(place) => format!("{place}.{name}"),
        None => name,
    }
}

impl AppRoot {
    /// Opens (or shows) the source tab of the function, trigger, ... a sidebar row
    /// stands for. A row the navigator holds no reference for opens nothing.
    pub(super) fn open_schema_object_source(&mut self, id: &NavigatorNodeId, label: &str, cx: &mut Context<Self>) {
        let (connection, reference) = match id {
            NavigatorNodeId::SchemaObject { connection, .. } => {
                let Some(reference) = self.navigator.schema_object(id) else {
                    return;
                };
                (connection, reference.clone())
            }
            // A materialized view is a table-like row that also has a definition.
            NavigatorNodeId::Object { connection, database, schema, object } => {
                let kind = match self.navigator.object_kind(id) {
                    Some(KindOfObject::MaterializedView) => SchemaObjectKind::MaterializedView,
                    Some(KindOfObject::Sequence) => SchemaObjectKind::Sequence,
                    _ => return,
                };
                let reference = SchemaObjectRef {
                    database: Some(database.clone()),
                    schema: Some(schema.clone()),
                    kind,
                    name: object.clone(),
                    arguments: None,
                    table: None,
                    identity: None,
                };
                (connection, reference)
            }
            _ => return,
        };
        let tab_id = self
            .workspace
            .open_or_activate_source_tab(connection, reference, label);
        self.ensure_source_loaded(tab_id, cx);
        self.notify_chrome(cx);
        cx.notify();
    }

    /// Starts (or waits for the connection of) the fetch of tab `tab_id`'s source,
    /// once per tab. Called when the tab is opened and when a restored session makes it the
    /// active tab; the first render of a tab that was restored in the background calls it
    /// too (a map lookup once the tab has its state).
    pub(super) fn ensure_source_loaded(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.source_tabs.contains_key(&tab_id) {
            return;
        }
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let (connection, Some(_)) = (tab.connection_id.clone(), tab.source.as_ref()) else {
            return;
        };
        if self.scenario_ui.service.is_some() || self.driver_runtime.sessions.contains_key(&connection) {
            return self.fetch_source(tab_id, cx);
        }
        // Not connected (a restored tab): open the connection, fetch when it is up.
        self.ensure_connected(connection.clone(), None, cx);
        let epoch = matches!(self.sessions.status(&connection), ConnectionStatus::Connecting)
            .then(|| self.sessions.generation(&connection))
            .flatten();
        let status = match epoch {
            Some(epoch) => SourceStatus::Connecting { epoch },
            None => SourceStatus::Failed("Could not connect; check the connection status".into()),
        };
        self.source_tabs.insert(tab_id, SourceTabState::new(status));
    }

    /// Fetches the source of tab `tab_id` again (Refresh, Retry). A tab whose connection is not
    /// open (its connect failed, or it was lost) starts over: it connects first, like a restored tab.
    pub(super) fn refresh_source(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let connected = self.scenario_ui.service.is_some()
            || self
                .workspace
                .tab(tab_id)
                .is_some_and(|tab| self.driver_runtime.sessions.contains_key(&tab.connection_id));
        if !connected {
            self.source_tabs.remove(&tab_id);
            self.ensure_source_loaded(tab_id, cx);
            cx.notify();
            return;
        }
        if let Some(state) = self.source_tabs.get_mut(&tab_id) {
            // The text on screen stays until the new one lands.
            if matches!(state.status, SourceStatus::Failed(_)) {
                state.text.clear();
            }
        }
        self.fetch_source(tab_id, cx);
    }

    fn fetch_source(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let (connection, Some(reference)) = (tab.connection_id.clone(), tab.source.clone()) else {
            return;
        };
        let state = self
            .source_tabs
            .entry(tab_id)
            .or_insert_with(|| SourceTabState::new(SourceStatus::Loading));
        state.status = SourceStatus::Loading;
        state.fetch += 1;
        let fetch = state.fetch;
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = crate::ui_fixture::schema_object_source(service.scenario().fixture, &reference)
                .map_err(|error| error.to_string());
            return self.apply_source(tab_id, fetch, result, cx);
        }
        let session = match self.driver_runtime.sessions.get(&connection).cloned() {
            Some(session) => session,
            None => {
                let error = SourceError::NotConnected.to_string();
                return self.apply_source(tab_id, fetch, Err(error), cx);
            }
        };
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<String, SourceError> {
                let request = serde_json::to_value(&reference).map_err(SourceError::Request)?;
                let described = session.describe_object(&request)?;
                serde_json::from_value::<SchemaObjectSource>(described)
                    .map(|described| described.source)
                    .map_err(SourceError::Unreadable)
            })
            .await
        });
        let task = cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                if result.as_ref().is_err_and(SourceError::is_connection_lost) {
                    this.mark_tab_connection_lost(tab_id);
                }
                let result = result.map_err(|error| crate::sessions::redact_secrets(&error.to_string()));
                this.apply_source(tab_id, fetch, result, cx);
            });
        });
        if let Some(state) = self.source_tabs.get_mut(&tab_id) {
            state.task = Some(task);
        }
    }

    fn apply_source(&mut self, tab_id: u64, fetch: u64, result: Result<String, String>, cx: &mut Context<Self>) {
        let connection = self.workspace.tab(tab_id).map(|tab| tab.connection_id.clone());
        let Some(state) = self.source_tabs.get_mut(&tab_id).filter(|state| state.fetch == fetch) else {
            return;
        };
        state.task = None;
        match result {
            Ok(text) => {
                if state.text != text || state.editor.is_none() {
                    state.text = text;
                    state.revision += 1;
                }
                state.status = SourceStatus::Ready;
            }
            Err(error) => {
                if let Some(connection) = connection.as_deref() {
                    self.report_error(Area::Navigator, Some(connection), &error);
                }
                if let Some(state) = self.source_tabs.get_mut(&tab_id) {
                    state.status = SourceStatus::Failed(error);
                }
            }
        }
        cx.notify();
    }

    /// `connection` opened: the source tabs that waited for it fetch now.
    pub(super) fn resume_waiting_sources(&mut self, connection: &str, cx: &mut Context<Self>) {
        let waiting: Vec<(u64, u64)> = self
            .source_tabs
            .iter()
            .filter_map(|(tab_id, state)| match state.status {
                SourceStatus::Connecting { epoch } => Some((*tab_id, epoch)),
                _ => None,
            })
            .filter(|(tab_id, _)| {
                self.workspace.tab(*tab_id).is_some_and(|tab| tab.connection_id == connection)
            })
            .collect();
        for (tab_id, epoch) in waiting {
            if self.sessions.generation(connection) == Some(epoch) {
                self.fetch_source(tab_id, cx);
            } else if let Some(state) = self.source_tabs.get_mut(&tab_id) {
                state.status = SourceStatus::Failed("Connection changed while connecting; try again".into());
            }
        }
        cx.notify();
    }

    /// The connect attempt `epoch` of `connection` failed: the source tabs waiting
    /// on it show why.
    pub(super) fn fail_waiting_sources(&mut self, connection: &str, epoch: u64, reason: &str, cx: &mut Context<Self>) {
        let waiting: Vec<u64> = self
            .source_tabs
            .iter()
            .filter(|(tab_id, state)| {
                state.status == SourceStatus::Connecting { epoch }
                    && self.workspace.tab(**tab_id).is_some_and(|tab| tab.connection_id == connection)
            })
            .map(|(tab_id, _)| *tab_id)
            .collect();
        for tab_id in waiting {
            if let Some(state) = self.source_tabs.get_mut(&tab_id) {
                state.status = SourceStatus::Failed(reason.to_string());
            }
        }
        cx.notify();
    }

    pub(super) fn copy_source(&self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(state) = self.source_tabs.get(&tab_id).filter(|state| !state.text.is_empty()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(state.text.clone()));
        }
    }

    /// Copies the source into a new query tab on the same connection, database and
    /// schema; the tab is the user's to edit and run.
    pub(super) fn open_source_in_sql_editor(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let Some(text) = self
            .source_tabs
            .get(&tab_id)
            .map(|state| state.text.clone())
            .filter(|text| !text.is_empty())
        else {
            return;
        };
        let metadata = TabMetadata {
            connection_id: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: tab.schema.clone(),
            ..Default::default()
        };
        let id = self.workspace.open_query_tab_with_metadata("Query", metadata);
        if let Some(query) = self.workspace.tab_mut(id) {
            query.sql = text;
        }
        self.notify_chrome(cx);
        cx.notify();
    }

    /// The content of a source tab: header (what, where, actions) over the
    /// read-only code editor, or the state the fetch is in.
    pub(super) fn render_source_tab(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        self.ensure_source_loaded(tab_id, cx);
        let source = self.workspace.tab(tab_id).and_then(|tab| tab.source.clone());
        // The editor follows the fetched text: built once, updated when the text changed.
        if let Some(state) = self.source_tabs.get_mut(&tab_id) {
            if state.status == SourceStatus::Ready && state.editor_revision != state.revision {
                state.editor_revision = state.revision;
                let text = state.text.clone();
                match &state.editor {
                    Some(editor) => editor.update(cx, |editor, cx| editor.set_value(text, window, cx)),
                    None => {
                        // The official code editor: tree-sitter SQL highlighting, line
                        // numbers and horizontal scrolling; read-only where it is drawn.
                        state.editor = Some(cx.new(|cx| {
                            EditorState::new(window, cx)
                                .language("sql")
                                .line_number(true)
                                .soft_wrap(false)
                                .default_value(text)
                        }));
                    }
                }
            }
        }
        let (status, editor, has_text) = match self.source_tabs.get(&tab_id) {
            Some(state) => (state.status.clone(), state.editor.clone(), !state.text.is_empty()),
            None => (SourceStatus::Loading, None, false),
        };
        // Each button sits in a div that carries its selector (as the DDL's Copy does).
        let button = |id: &'static str, icon: &'static str, label: &'static str, enabled: bool| {
            Button::new((id, tab_id))
                .icon(Icon::empty().path(icon).size(px(14.0)))
                .label(label)
                .with_size(Size::Small)
                .compact()
                .outline()
                .disabled(!enabled)
        };
        let wrap = |id: &'static str, button: Button| div().debug_selector(move || id.into()).child(button);
        let actions = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(wrap(
                "source-copy",
                button("source-copy", "icons/copy.svg", "Copy", has_text)
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_source(tab_id, cx))),
            ))
            .child(wrap(
                "source-open-in-editor",
                button("source-open-in-editor", "icons/square-terminal.svg", "Open in SQL Editor", has_text)
                    .on_click(cx.listener(move |this, _, _, cx| this.open_source_in_sql_editor(tab_id, cx))),
            ))
            .child(wrap(
                "source-refresh",
                button(
                    "source-refresh",
                    "icons/refresh-cw.svg",
                    "Refresh",
                    !matches!(status, SourceStatus::Loading | SourceStatus::Connecting { .. }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.refresh_source(tab_id, cx))),
            ));
        let title = source
            .as_ref()
            .map(|source| format!("{} \u{00B7} {}", kind_title(source.kind), qualified_name(source)))
            .unwrap_or_default();
        let note = |text: String, color: gpui::Rgba| {
            div()
                .debug_selector(|| "source-note".into())
                .flex_1()
                .p(px(16.0))
                .text_size(px(12.0))
                .text_color(color)
                .child(text)
                .into_any_element()
        };
        let body = match (&status, editor) {
            (SourceStatus::Ready, Some(state)) => div()
                .debug_selector(|| "source-editor".into())
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_size(px(12.0))
                .child(Editor::new(&state).readonly(true).appearance(false).bordered(false).h_full())
                .into_any_element(),
            (SourceStatus::Failed(error), _) => note(error.clone(), theme::danger()),
            _ => note("Loading source\u{2026}".into(), theme::text_muted()),
        };
        div()
            .id(("source-tab", tab_id))
            .debug_selector(|| "source-tab".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(Self::properties_toolbar(title, div().flex_1()).child(actions))
            .child(body)
    }
}
