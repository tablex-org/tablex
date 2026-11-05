//! Aggregate tab (`query_mode = "mongoAggregate"`): a Compass-style pipeline
//! builder with a Text editor and a Stages editor over one
//! [`AggregateDraft`](crate::aggregate::AggregateDraft) kept on the tab.
//!
//! Layout of the code:
//! * `mod.rs`: UI state, edit handlers and the debounced preview pipeline;
//! * `run.rs`: Run (full result in the normal Mongo grid), Explain, cancel, the
//!   `$out` / `$merge` confirmation, Export Code / Data;
//! * `saved.rs`: saved pipelines (storage, dialogs, sidebar group);
//! * `view.rs`: rendering.
//!
//! Preview rules (performance): edits only bump a generation counter and
//! re-arm one 400 ms timer; the previous in-flight requests are cancelled
//! (driver `operation.cancel`) and anything that still arrives for an old
//! generation is dropped. At most [`MAX_PARALLEL_PREVIEWS`] requests run at
//! once, only for the visible output (Text: one; Stages: expanded stages).
//! A preview page is decoded once into `serde_json` and released at once, so
//! previews never hold driver page-cache slots, and the tree rows are
//! flattened when the data or the expansion changes, never per frame.
use super::*;
use crate::aggregate::{AggregateDraft, EditorMode, PreviewDocs, PREVIEW_DEBOUNCE_MS};
use crate::mongo_aggregate as engine;
use crate::mongo_result::{flatten_tree, MongoTreeRow};
use gpui::Subscription;
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};
use std::collections::VecDeque;

pub(super) mod run;
pub(super) mod saved;
pub(super) mod view;

/// Concurrent preview requests per tab.
const MAX_PARALLEL_PREVIEWS: usize = 2;
/// Label of the Select entry that lets a stage use any operator name.
pub(super) const CUSTOM_OPERATOR: &str = "Custom…";

#[derive(Clone, Debug, PartialEq)]
pub(super) enum PreviewStatus {
    Idle,
    Loading,
    Ready,
    /// The pipeline (or an option) does not parse; nothing was requested.
    Invalid,
    Failed(String),
}

/// Output of one preview request plus its tree state.
pub(super) struct PreviewSlot {
    pub(super) status: PreviewStatus,
    pub(super) docs: Arc<PreviewDocs>,
    pub(super) rows: Arc<Vec<MongoTreeRow>>,
    pub(super) expanded: HashSet<String>,
    /// Bumped whenever `docs` changes (cache key of the JSON viewer).
    pub(super) revision: u64,
    pub(super) elapsed_ms: Option<u64>,
}

impl PreviewSlot {
    fn new() -> Self {
        Self {
            status: PreviewStatus::Idle,
            docs: Arc::new(PreviewDocs { docs: Vec::new() }),
            rows: Arc::new(Vec::new()),
            expanded: HashSet::new(),
            revision: 0,
            elapsed_ms: None,
        }
    }

    fn set_docs(&mut self, docs: PreviewDocs, elapsed_ms: Option<u64>) {
        self.docs = Arc::new(docs);
        self.status = PreviewStatus::Ready;
        self.revision += 1;
        self.elapsed_ms = elapsed_ms;
        // The first document opens so the preview shows content at once.
        self.expanded = HashSet::from(["/0".to_string()]);
        self.rebuild_rows();
    }

    fn rebuild_rows(&mut self) {
        self.rows = Arc::new(flatten_tree(&self.docs.as_array(), &self.expanded));
    }

    pub(super) fn toggle(&mut self, path: &str) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_string());
        }
        self.rebuild_rows();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum SlotKey {
    Main,
    Stage(u64),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) enum PreviewView {
    #[default]
    Tree,
    Json,
}

struct PreviewJob {
    key: SlotKey,
    payload: serde_json::Value,
}

struct RunningJob {
    key: SlotKey,
    generation: u64,
    started: Instant,
    operation: Option<DriverQueryOperation>,
    /// Cancelled by a newer generation but not finished yet: it keeps its
    /// slot (the server is still winding it down) until its answer arrives.
    cancelled: bool,
}

/// Editors of one stage card. Created lazily, dropped with the stage.
pub(super) struct StageEditors {
    pub(super) body: gpui::Entity<EditorState>,
    pub(super) operator: gpui::Entity<SelectState<Vec<String>>>,
    pub(super) custom: gpui::Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

/// Inputs of the Options popover.
pub(super) struct OptionInputs {
    pub(super) sample: gpui::Entity<InputState>,
    pub(super) max_time: gpui::Entity<InputState>,
    pub(super) collation: gpui::Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Default)]
pub(super) struct TabUi {
    pub(super) text_editor: Option<gpui::Entity<EditorState>>,
    text_subscription: Option<Subscription>,
    pub(super) stages: HashMap<u64, StageEditors>,
    pub(super) main: Option<PreviewSlot>,
    pub(super) stage_previews: HashMap<u64, PreviewSlot>,
    /// Stages whose preview is collapsed (the default is expanded).
    pub(super) collapsed: HashSet<u64>,
    pub(super) json_viewer: Option<results::json_view::JsonViewer>,
    pub(super) preview_view: PreviewView,
    pub(super) generation: u64,
    debounce: Option<Task<()>>,
    queue: VecDeque<PreviewJob>,
    running: HashMap<u64, RunningJob>,
    next_job: u64,
    pub(super) options_open: bool,
    pub(super) option_inputs: Option<OptionInputs>,
    /// Validation message of the Options popover.
    pub(super) options_error: Option<String>,
    /// One-line message in the header (copied, saved, ...).
    pub(super) notice: Option<String>,
    /// The first preview of a tab is scheduled when it first renders.
    started: bool,
    /// Last text pushed into / read from the text editor, so an echoed
    /// `Change` event after a programmatic `set_value` is ignored.
    pub(super) explain: Option<run::ExplainUi>,
    pub(super) next_explain: u64,
    /// Elapsed time of the last full Run.
    pub(super) run_ms: Option<u64>,
    /// Cached analysis of the draft (check, edit-safety, final write) for the
    /// revision it was computed at; refreshed by [`AppRoot::ensure_aggregate_derived`].
    pub(super) derived: Option<crate::aggregate::Derived>,
    /// An Export Data request is running (and the operation to cancel).
    pub(super) exporting: bool,
    pub(super) export_operation: Option<DriverQueryOperation>,
}

impl TabUi {
    pub(super) fn slot(&self, key: SlotKey) -> Option<&PreviewSlot> {
        match key {
            SlotKey::Main => self.main.as_ref(),
            SlotKey::Stage(id) => self.stage_previews.get(&id),
        }
    }

    fn slot_mut(&mut self, key: SlotKey) -> &mut PreviewSlot {
        match key {
            SlotKey::Main => self.main.get_or_insert_with(PreviewSlot::new),
            SlotKey::Stage(id) => self
                .stage_previews
                .entry(id)
                .or_insert_with(PreviewSlot::new),
        }
    }
}

#[derive(Default)]
pub(super) struct AggregateUi {
    tabs: HashMap<u64, TabUi>,
}

impl AggregateUi {
    pub(super) fn tab(&self, tab_id: u64) -> Option<&TabUi> {
        self.tabs.get(&tab_id)
    }

    pub(super) fn tab_mut(&mut self, tab_id: u64) -> &mut TabUi {
        self.tabs.entry(tab_id).or_default()
    }

    #[cfg(test)]
    pub(super) fn explain_state(&self, tab_id: u64) -> Option<(u64, bool)> {
        self.tabs
            .get(&tab_id)
            .and_then(|ui| ui.explain.as_ref())
            .map(|explain| (explain.id(), explain.in_flight()))
    }

    #[cfg(test)]
    pub(super) fn running_count(&self, tab_id: u64) -> usize {
        self.tabs.get(&tab_id).map_or(0, |ui| ui.running.len())
    }
}

/// `(body editor, operator Select, custom operator input)` of a stage card.
pub(super) type StageEditorHandles = (
    gpui::Entity<EditorState>,
    gpui::Entity<SelectState<Vec<String>>>,
    gpui::Entity<InputState>,
);

/// Items of the operator Select: the common stages plus "Custom…".
fn operator_items() -> Vec<String> {
    crate::aggregate::COMMON_STAGES
        .iter()
        .map(|(name, _)| name.to_string())
        .chain(std::iter::once(CUSTOM_OPERATOR.to_string()))
        .collect()
}

/// Select value that stands for `operator`.
pub(super) fn operator_choice(operator: &str) -> String {
    if crate::aggregate::COMMON_STAGES
        .iter()
        .any(|(name, _)| *name == operator)
    {
        operator.to_string()
    } else {
        CUSTOM_OPERATOR.to_string()
    }
}

impl AppRoot {
    pub(super) fn aggregate_draft(&self, tab_id: u64) -> Option<&AggregateDraft> {
        self.workspace.tab(tab_id)?.aggregate.as_ref()
    }

    /// Recomputes the cached analysis when the draft changed (one parse
    /// pass); a no-op comparison otherwise. Call before rendering and after
    /// edits, never inside a loop.
    pub(super) fn ensure_aggregate_derived(&mut self, tab_id: u64) {
        let Some(draft) = self.workspace.tab(tab_id).and_then(|tab| tab.aggregate.as_ref()) else {
            return;
        };
        let ui = self.aggregate_ui.tabs.entry(tab_id).or_default();
        if ui.derived.as_ref().is_some_and(|derived| derived.revision == draft.revision) {
            return;
        }
        ui.derived = Some(draft.derive());
    }

    /// The cached analysis (see [`Self::ensure_aggregate_derived`]).
    pub(super) fn aggregate_derived(&self, tab_id: u64) -> Option<&crate::aggregate::Derived> {
        self.aggregate_ui.tab(tab_id)?.derived.as_ref()
    }

    pub(super) fn aggregate_draft_mut(&mut self, tab_id: u64) -> Option<&mut AggregateDraft> {
        self.workspace.tab_mut(tab_id)?.aggregate.as_mut()
    }

    /// Removes everything the Aggregate UI holds for a closed tab and
    /// cancels its in-flight requests.
    pub(super) fn aggregate_tab_removed(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(mut ui) = self.aggregate_ui.tabs.remove(&tab_id) {
            ui.debounce = None;
            for (_, job) in ui.running.drain() {
                if let Some(operation) = job.operation {
                    drivers::cancel_driver_operation(operation, cx);
                }
            }
            if let Some(explain) = ui.explain.take() {
                run::cancel_explain(explain, cx);
            }
            if let Some(operation) = ui.export_operation.take() {
                drivers::cancel_driver_operation(operation, cx);
            }
        }
    }

    // ---- opening ------------------------------------------------------------

    /// Opens a new Aggregate tab (sidebar menu "New Aggregation", Create New).
    pub(super) fn open_aggregate_tab(
        &mut self,
        connection: &str,
        database: &str,
        collection: &str,
        draft: AggregateDraft,
        cx: &mut Context<Self>,
    ) -> u64 {
        let tab_id = self
            .workspace
            .open_aggregate_tab(connection, database, collection, draft);
        cx.notify();
        tab_id
    }

    // ---- editors ------------------------------------------------------------

    /// Text editor of a tab, created on first use.
    pub(super) fn aggregate_text_editor(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<EditorState> {
        if let Some(editor) = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.text_editor.clone())
        {
            return editor;
        }
        let text = self
            .aggregate_draft(tab_id)
            .map(|draft| draft.text.clone())
            .unwrap_or_default();
        // Pipelines are JavaScript-flavoured (shell constructors, unquoted
        // keys), which the JSON grammar would flag as errors.
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Pipeline);
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("javascript")
                .line_number(true)
                .soft_wrap(false)
                .default_value(text)
                .completion(completion)
        });
        let subscription = cx.subscribe_in(
            &editor,
            window,
            move |this: &mut Self,
                  editor: &gpui::Entity<EditorState>,
                  event: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let text = editor.read(cx).value().to_string();
                    this.on_aggregate_text_edit(tab_id, text, window, cx);
                }
            },
        );
        let ui = self.aggregate_ui.tab_mut(tab_id);
        ui.text_editor = Some(editor.clone());
        ui.text_subscription = Some(subscription);
        editor
    }

    /// Editors of a stage card, created on first use.
    pub(super) fn aggregate_stage_editors(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<StageEditorHandles> {
        if let Some(editors) = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.stages.get(&stage_id))
        {
            return Some((
                editors.body.clone(),
                editors.operator.clone(),
                editors.custom.clone(),
            ));
        }
        let stage = self
            .aggregate_draft(tab_id)?
            .stages
            .iter()
            .find(|stage| stage.id == stage_id)?
            .clone();
        let completion = TabCompletion::for_tab(cx, tab_id, CompletionRole::Stage(stage_id));
        let body = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("javascript")
                .line_number(false)
                .soft_wrap(true)
                .default_value(stage.body.clone())
                .completion(completion)
        });
        let items = operator_items();
        let choice = operator_choice(&stage.operator);
        let selected = items.iter().position(|item| *item == choice);
        let operator = cx.new(|cx| {
            SelectState::new(
                items,
                selected.map(|row| gpui_component::IndexPath::default().row(row)),
                window,
                cx,
            )
            .searchable(true)
        });
        let custom = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("$operator")
                .default_value(stage.operator.clone())
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &body,
            window,
            move |this: &mut Self,
                  editor: &gpui::Entity<EditorState>,
                  event: &InputEvent,
                  _: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let text = editor.read(cx).value().to_string();
                    this.on_aggregate_stage_body_edit(tab_id, stage_id, text, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &operator,
            window,
            move |this: &mut Self,
                  _,
                  event: &SelectEvent<Vec<String>>,
                  window: &mut Window,
                  cx: &mut Context<Self>| {
                if let SelectEvent::Confirm(Some(choice)) = event {
                    this.on_aggregate_operator_chosen(tab_id, stage_id, choice, window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &custom,
            window,
            move |this: &mut Self,
                  input: &gpui::Entity<InputState>,
                  event: &InputEvent,
                  _: &mut Window,
                  cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let name = input.read(cx).value().trim().to_string();
                    this.on_aggregate_custom_operator(tab_id, stage_id, name, cx);
                }
            },
        ));
        let editors = StageEditors {
            body: body.clone(),
            operator: operator.clone(),
            custom: custom.clone(),
            _subscriptions: subscriptions,
        };
        self.aggregate_ui
            .tab_mut(tab_id)
            .stages
            .insert(stage_id, editors);
        Some((body, operator, custom))
    }

    /// Pushes the draft into the existing editors after a change that did not
    /// come from them (mode switch, stage operations, format, load).
    pub(super) fn push_aggregate_draft_to_editors(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) else {
            return;
        };
        if let Some(editor) = ui.text_editor.clone() {
            // Compare before writing: `set_value` resets the cursor.
            if editor.read(cx).value().as_ref() != draft.text {
                editor.update(cx, |state, cx| state.set_value(draft.text.clone(), window, cx));
            }
        }
        let live: HashSet<u64> = draft.stages.iter().map(|stage| stage.id).collect();
        ui.stages.retain(|id, _| live.contains(id));
        ui.stage_previews.retain(|id, _| live.contains(id));
        ui.collapsed.retain(|id| live.contains(id));
        for stage in &draft.stages {
            let Some(editors) = ui.stages.get(&stage.id) else {
                continue;
            };
            if editors.body.read(cx).value().as_ref() != stage.body {
                editors
                    .body
                    .update(cx, |state, cx| state.set_value(stage.body.clone(), window, cx));
            }
            let choice = operator_choice(&stage.operator);
            if editors.operator.read(cx).selected_value() != Some(&choice) {
                editors
                    .operator
                    .update(cx, |state, cx| state.set_selected_value(&choice, window, cx));
            }
            if editors.custom.read(cx).value().as_ref() != stage.operator {
                editors
                    .custom
                    .update(cx, |state, cx| state.set_value(stage.operator.clone(), window, cx));
            }
        }
    }

    // ---- edit handlers ------------------------------------------------------

    fn on_aggregate_text_edit(
        &mut self,
        tab_id: u64,
        text: String,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        if draft.text == text {
            return; // echo of a programmatic set_value
        }
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        let _ = draft.apply_text(&text);
        self.aggregate_edited(tab_id, cx);
    }

    fn on_aggregate_stage_body_edit(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        if draft.set_stage_body(stage_id, &text) {
            self.aggregate_edited(tab_id, cx);
        }
    }

    fn on_aggregate_operator_chosen(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        choice: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        let changed = if choice == CUSTOM_OPERATOR {
            // Keep a custom name that is already there; otherwise start one.
            let current = draft
                .stages
                .iter()
                .find(|stage| stage.id == stage_id)
                .map(|stage| stage.operator.clone());
            match current {
                Some(operator) if operator_choice(&operator) == CUSTOM_OPERATOR => false,
                _ => draft.set_stage_operator(stage_id, "$custom", false),
            }
        } else {
            draft.set_stage_operator(stage_id, choice, true)
        };
        if changed {
            self.push_aggregate_draft_to_editors(tab_id, window, cx);
            self.aggregate_edited(tab_id, cx);
        }
    }

    fn on_aggregate_custom_operator(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        if !name.is_empty() && draft.set_stage_operator(stage_id, &name, false) {
            self.aggregate_edited(tab_id, cx);
        }
    }

    /// Common tail of every edit: refreshes diagnostics, retitles, clears the
    /// notice and re-arms the preview timer.
    pub(super) fn aggregate_edited(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.ensure_aggregate_derived(tab_id);
        self.apply_aggregate_diagnostics(tab_id, cx);
        self.aggregate_ui.tab_mut(tab_id).notice = None;
        self.schedule_aggregate_preview(tab_id, cx);
        cx.notify();
    }

    /// Marks parse errors in the text editor (line / column of the shared
    /// parser) so they show as inline diagnostics.
    fn apply_aggregate_diagnostics(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
        use gpui_component::input::Position;
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        if draft.mode != EditorMode::Text {
            return;
        }
        self.ensure_aggregate_derived(tab_id);
        let error = self
            .aggregate_derived(tab_id)
            .and_then(|derived| derived.check.text_error.clone());
        let Some(editor) = self
            .aggregate_ui
            .tab(tab_id)
            .and_then(|ui| ui.text_editor.clone())
        else {
            return;
        };
        editor.update(cx, |state, cx| {
            let text = state.text().clone();
            if let Some(set) = state.diagnostics_mut() {
                set.reset(&text);
                if let Some(error) = &error {
                    if error.line > 0 {
                        let line = (error.line - 1) as u32;
                        let col = (error.col.saturating_sub(1)) as u32;
                        set.push(
                            Diagnostic::new(
                                Position::new(line, col)..Position::new(line, col + 1),
                                error.message.clone(),
                            )
                            .with_severity(DiagnosticSeverity::Error),
                        );
                    }
                }
            }
            cx.notify();
        });
    }

    // ---- stage operations (buttons) -----------------------------------------

    pub(super) fn aggregate_add_stage(
        &mut self,
        tab_id: u64,
        after: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        draft.add_stage_after(after, "$match");
        self.push_aggregate_draft_to_editors(tab_id, window, cx);
        self.aggregate_edited(tab_id, cx);
    }

    pub(super) fn aggregate_delete_stage(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        if draft.delete_stage(stage_id) {
            self.push_aggregate_draft_to_editors(tab_id, window, cx);
            self.aggregate_edited(tab_id, cx);
        }
    }

    pub(super) fn aggregate_toggle_stage(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        let enabled = draft
            .stages
            .iter()
            .find(|stage| stage.id == stage_id)
            .is_some_and(|stage| stage.enabled);
        if draft.set_stage_enabled(stage_id, !enabled) {
            self.push_aggregate_draft_to_editors(tab_id, window, cx);
            self.aggregate_edited(tab_id, cx);
        }
    }

    pub(super) fn aggregate_move_stage(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        target: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        if draft.move_stage(stage_id, target) {
            self.push_aggregate_draft_to_editors(tab_id, window, cx);
            self.aggregate_edited(tab_id, cx);
        }
    }

    /// Collapses / expands the preview under a stage card.
    pub(super) fn aggregate_toggle_stage_preview(
        &mut self,
        tab_id: u64,
        stage_id: u64,
        cx: &mut Context<Self>,
    ) {
        let ui = self.aggregate_ui.tab_mut(tab_id);
        if ui.collapsed.remove(&stage_id) {
            // Expanding needs data.
            self.schedule_aggregate_preview(tab_id, cx);
        } else {
            // Collapsing frees the documents.
            ui.collapsed.insert(stage_id);
            ui.stage_previews.remove(&stage_id);
        }
        cx.notify();
    }

    pub(super) fn set_aggregate_mode(
        &mut self,
        tab_id: u64,
        mode: EditorMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        match draft.set_mode(mode) {
            Ok(()) => {
                self.aggregate_ui.tab_mut(tab_id).notice = None;
                self.push_aggregate_draft_to_editors(tab_id, window, cx);
                self.apply_aggregate_diagnostics(tab_id, cx);
                self.schedule_aggregate_preview(tab_id, cx);
            }
            Err(message) => self.aggregate_ui.tab_mut(tab_id).notice = Some(message),
        }
        cx.notify();
    }

    pub(super) fn set_aggregate_preview_enabled(
        &mut self,
        tab_id: u64,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft_mut(tab_id) else {
            return;
        };
        draft.preview = enabled;
        if enabled {
            self.schedule_aggregate_preview(tab_id, cx);
        } else {
            // Turning previews off stops what is running and frees results.
            self.cancel_aggregate_preview_jobs(tab_id, cx);
            let ui = self.aggregate_ui.tab_mut(tab_id);
            ui.debounce = None;
            ui.generation += 1;
            ui.main = None;
            ui.stage_previews.clear();
        }
        cx.notify();
    }

    /// Replaces the text with its formatted form (key order preserved).
    pub(super) fn format_aggregate_text(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.aggregate_draft(tab_id) else {
            return;
        };
        match draft.formatted_text() {
            Ok(text) => {
                if let Some(draft) = self.aggregate_draft_mut(tab_id) {
                    let _ = draft.apply_text(&text);
                }
                self.push_aggregate_draft_to_editors(tab_id, window, cx);
                self.aggregate_edited(tab_id, cx);
            }
            Err(error) => {
                self.aggregate_ui.tab_mut(tab_id).notice = Some(format!("Cannot format: {error}"));
                cx.notify();
            }
        }
    }

    // ---- previews -------------------------------------------------------------

    /// Called when the tab first renders: starts the first preview.
    pub(super) fn start_aggregate_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let ui = self.aggregate_ui.tab_mut(tab_id);
        if ui.started {
            return;
        }
        ui.started = true;
        self.schedule_aggregate_preview(tab_id, cx);
    }

    /// Re-arms the debounce timer and cancels what is in flight.
    pub(super) fn schedule_aggregate_preview(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.cancel_aggregate_preview_jobs(tab_id, cx);
        let ui = self.aggregate_ui.tab_mut(tab_id);
        ui.generation += 1;
        ui.queue.clear();
        let generation = ui.generation;
        ui.debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(PREVIEW_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.fire_aggregate_previews(tab_id, generation, cx)
            });
        }));
    }

    fn cancel_aggregate_preview_jobs(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) else {
            return;
        };
        // Cancelled jobs stay in `running` until they actually finish, so the
        // concurrency cap counts requests the server may still be running.
        let mut operations = Vec::new();
        let mut fixture_cancels = 0u32;
        for job in ui.running.values_mut().filter(|job| !job.cancelled) {
            job.cancelled = true;
            match job.operation.take() {
                Some(operation) => operations.push(operation),
                None => fixture_cancels += 1,
            }
        }
        for operation in operations {
            drivers::cancel_driver_operation(operation, cx);
        }
        if fixture_cancels > 0 {
            if let Some(service) = self.scenario_ui.service.as_mut() {
                for _ in 0..fixture_cancels {
                    service.fixture_mut().note_aggregate_cancel();
                }
            }
        }
    }

    fn fire_aggregate_previews(&mut self, tab_id: u64, generation: u64, cx: &mut Context<Self>) {
        self.ensure_aggregate_derived(tab_id);
        let Some(draft) = self.aggregate_draft(tab_id).cloned() else {
            return;
        };
        let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) else {
            return;
        };
        if ui.generation != generation {
            return;
        }
        ui.debounce = None;
        if !draft.preview {
            return;
        }
        let valid = ui.derived.as_ref().is_some_and(|derived| {
            derived.revision == draft.revision && derived.check.is_valid()
        }) && draft.options.validate().is_ok();
        let mut jobs = Vec::new();
        match draft.mode {
            EditorMode::Text => {
                ui.stage_previews.clear();
                if valid {
                    jobs.push(PreviewJob {
                        key: SlotKey::Main,
                        payload: draft.preview_payload(None),
                    });
                }
            }
            EditorMode::Stages => {
                ui.main = None;
                if valid {
                    for stage in draft.enabled_stages() {
                        if ui.collapsed.contains(&stage.id) {
                            continue;
                        }
                        let Some(index) = draft.enabled_index(stage.id) else {
                            continue;
                        };
                        jobs.push(PreviewJob {
                            key: SlotKey::Stage(stage.id),
                            payload: draft.preview_payload(Some(index)),
                        });
                    }
                }
            }
        }
        if !valid {
            // Nothing runs; the editor shows the error. Old documents stay.
            match draft.mode {
                EditorMode::Text => ui.slot_mut(SlotKey::Main).status = PreviewStatus::Invalid,
                EditorMode::Stages => {
                    for stage in draft.enabled_stages() {
                        ui.slot_mut(SlotKey::Stage(stage.id)).status = PreviewStatus::Invalid;
                    }
                }
            }
            cx.notify();
            return;
        }
        for job in &jobs {
            ui.slot_mut(job.key).status = PreviewStatus::Loading;
        }
        ui.queue = jobs.into();
        self.pump_aggregate_previews(tab_id, cx);
        cx.notify();
    }

    fn pump_aggregate_previews(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        loop {
            let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) else {
                return;
            };
            if ui.running.len() >= MAX_PARALLEL_PREVIEWS {
                return;
            }
            let Some(job) = ui.queue.pop_front() else {
                return;
            };
            let job_id = ui.next_job;
            ui.next_job += 1;
            let generation = ui.generation;
            ui.running.insert(
                job_id,
                RunningJob {
                    key: job.key,
                    generation,
                    started: Instant::now(),
                    operation: None,
                    cancelled: false,
                },
            );
            self.start_aggregate_job(tab_id, job_id, job.payload, cx);
        }
    }

    /// Sends one preview request (driver, or the scenario fixture).
    fn start_aggregate_job(
        &mut self,
        tab_id: u64,
        job_id: u64,
        payload: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let Some((connection, database, collection)) =
            self.workspace.tab(tab_id).map(|tab| {
                (
                    tab.connection_id.clone(),
                    tab.database.clone(),
                    tab.collection.clone(),
                )
            })
        else {
            return;
        };
        let call = AggregateCall {
            connection,
            database,
            collection,
            explain: false,
            payload,
        };
        match self.dispatch_aggregate_call(call, cx) {
            DispatchedAggregate::Done(result) => {
                self.finish_aggregate_job(tab_id, job_id, result, cx);
            }
            DispatchedAggregate::Pending { operation, task } => {
                if let Some(job) = self
                    .aggregate_ui
                    .tabs
                    .get_mut(&tab_id)
                    .and_then(|ui| ui.running.get_mut(&job_id))
                {
                    job.operation = operation;
                }
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.finish_aggregate_job(tab_id, job_id, result, cx);
                    });
                })
                .detach();
            }
            DispatchedAggregate::Failed(message) => {
                self.finish_aggregate_job(tab_id, job_id, Err(message), cx);
            }
        }
    }

    fn finish_aggregate_job(
        &mut self,
        tab_id: u64,
        job_id: u64,
        result: Result<AggregateDocs, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(ui) = self.aggregate_ui.tabs.get_mut(&tab_id) else {
            return;
        };
        let Some(job) = ui.running.remove(&job_id) else {
            return;
        };
        let mut failure = None;
        // A cancelled or superseded job's answer is dropped; it only frees
        // its slot.
        if !job.cancelled && job.generation == ui.generation {
            let elapsed = u64::try_from(job.started.elapsed().as_millis()).ok();
            let slot = ui.slot_mut(job.key);
            match result {
                Ok(output) => slot.set_docs(PreviewDocs::from_documents(&output.documents), elapsed),
                Err(message) if message.contains("operation cancelled") => {
                    slot.status = PreviewStatus::Idle;
                }
                Err(message) => {
                    failure = Some(message.clone());
                    slot.status = PreviewStatus::Failed(message);
                }
            }
        }
        if let Some(message) = failure {
            self.report_query_failure(tab_id, &message);
        }
        self.pump_aggregate_previews(tab_id, cx);
        cx.notify();
    }
}

/// One aggregate request to the driver (or the fixture).
pub(super) struct AggregateCall {
    pub(super) connection: String,
    pub(super) database: String,
    pub(super) collection: String,
    pub(super) explain: bool,
    pub(super) payload: serde_json::Value,
}

/// Decoded documents of a preview / explain page.
pub(super) struct AggregateDocs {
    pub(super) documents: Vec<bson::Document>,
}

pub(super) enum DispatchedAggregate {
    /// Answered synchronously (scenario fixture).
    Done(Result<AggregateDocs, String>),
    /// In flight. `operation` is the driver operation to cancel (`None` for
    /// the scenario fixture, which counts cancellations instead).
    Pending {
        operation: Option<DriverQueryOperation>,
        task: Task<Result<AggregateDocs, String>>,
    },
    Failed(String),
}

/// Reads every document of a page lease.
fn decode_page_documents(
    lease: &engine_driver_host::PageLease,
    row_count: u64,
) -> Result<Vec<bson::Document>, String> {
    let mut documents = Vec::with_capacity(row_count as usize);
    for row in 0..row_count {
        let slice = lease
            .indexed_slice(row)
            .map_err(|error| error.to_string())?;
        let raw = bson::raw::RawDocument::from_bytes(slice.as_bytes())
            .map_err(|error| error.to_string())?;
        documents.push(bson::Document::try_from(raw).map_err(|error| error.to_string())?);
    }
    Ok(documents)
}

impl AppRoot {
    /// Runs one `mongo_aggregate` / `mongo_aggregate_explain` request whose
    /// output is read once and released (previews, Explain).
    pub(super) fn dispatch_aggregate_call(
        &mut self,
        call: AggregateCall,
        cx: &mut Context<Self>,
    ) -> DispatchedAggregate {
        if let Some(service) = self.scenario_ui.service.as_mut() {
            let latency = service.fixture().aggregate_latency();
            let result = service
                .query_mongo_aggregate(
                    &call.connection,
                    &call.database,
                    &call.collection,
                    call.explain,
                    &call.payload,
                    0,
                    50,
                )
                .and_then(|page| {
                    decode_page_documents(&page.lease, page.page.row_count)
                        .map(|documents| AggregateDocs { documents })
                });
            if latency.is_zero() {
                return DispatchedAggregate::Done(result);
            }
            // A slow server: the answer arrives later (tests drive the clock).
            let task = cx.background_executor().spawn({
                let executor = cx.background_executor().clone();
                async move {
                    executor.timer(latency).await;
                    result
                }
            });
            return DispatchedAggregate::Pending {
                operation: None,
                task,
            };
        }
        let prepared = match self.begin_driver_query(&call.connection) {
            Ok(prepared) => prepared,
            Err(error) => return DispatchedAggregate::Failed(error.to_string()),
        };
        let drivers::PreparedDriverQuery {
            session,
            runner,
            operation,
        } = prepared;
        let request = QueryRequest {
            kind: if call.explain {
                "mongo_aggregate_explain".into()
            } else {
                "mongo_aggregate".into()
            },
            operation_id: operation.operation_id,
            database: Some(call.database),
            object: Some(call.collection),
            offset: 0,
            limit: 200,
            payload: call.payload,
        };
        let request_json = serde_json::to_string(&request).expect("aggregate request serializes");
        let task = cx.background_executor().spawn(async move {
            // No session lock while the server works (see run_active_mongo_request).
            let page = runner
                .open_query(&request_json)
                .map_err(|error| error.to_string())?;
            // Every path after `open_query` frees the page: a poisoned
            // session lock must not leak it in the driver's page cache.
            let mut session = session.lock().map_err(|_| {
                runner.release_page(page.page_id);
                "Driver session lock poisoned".to_string()
            })?;
            session
                .adopt_page(&runner, page.page_id)
                .map_err(|error| error.to_string())?;
            let decoded = session
                .acquire_page(page.page_id, 0, page.row_count)
                .map_err(|error| error.to_string())
                .and_then(|lease| decode_page_documents(&lease, page.row_count));
            // Previews never keep a driver page: they are decoded and freed.
            let _ = session.release_page(page.page_id);
            decoded.map(|documents| AggregateDocs { documents })
        });
        DispatchedAggregate::Pending {
            operation: Some(operation),
            task,
        }
    }
}
