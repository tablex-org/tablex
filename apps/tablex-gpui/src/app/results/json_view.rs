//! Read-only JSON viewers (rendered with `Editor::readonly(true)`) built on gpui-component's code editor, which owns
//! the tree-sitter JSON highlighting, virtualized line layout, horizontal
//! scrolling and selection/copy. States are created lazily and refilled only
//! when the displayed text changes (identified by a cheap key), never per frame.

use super::super::*;
use std::hash::{Hash, Hasher};

pub(in crate::app) struct JsonViewer {
    pub(in crate::app) state: gpui::Entity<EditorState>,
    key: u64,
}

#[derive(Default)]
pub(in crate::app) struct JsonViews {
    /// Details tab value box for JSON-like cell values.
    pub(in crate::app) cell_value: Option<JsonViewer>,
    /// Row tab JSON view (whole row).
    pub(in crate::app) cell_json: Option<JsonViewer>,
    /// Mongo result JSON document view.
    pub(in crate::app) mongo_doc: Option<JsonViewer>,
    /// How many times any viewer's text was (re)built; the tests use it to
    /// prove that unchanged state never rebuilds text.
    pub(in crate::app) text_builds: u64,
}

/// Layout knobs that differ per viewer.
#[derive(Clone, Copy)]
pub(in crate::app) struct JsonViewerStyle {
    pub soft_wrap: bool,
    pub line_numbers: bool,
    /// Code folding gutter; useful for whole-document views, noise in the
    /// small value box.
    pub folding: bool,
}

/// Small Shell / Canonical segmented control shared by the Mongo JSON views.
pub(in crate::app) fn ejson_mode_toggle(
    id: impl Into<gpui::ElementId>,
    mode: crate::cell_value::EjsonMode,
    root: gpui::WeakEntity<AppRoot>,
    set: impl Fn(&mut AppRoot, crate::cell_value::EjsonMode) + 'static,
) -> impl IntoElement {
    use crate::cell_value::EjsonMode;
    use gpui_component::button::ButtonGroup;
    const OPTIONS: [(EjsonMode, &str); 2] =
        [(EjsonMode::Shell, "Shell"), (EjsonMode::Canonical, "Canonical")];
    ButtonGroup::new(id)
        .outline()
        .compact()
        .with_size(Size::XSmall)
        .children(OPTIONS.iter().enumerate().map(|(index, (option, label))| {
            Button::new(("ejson-mode-option", index))
                .label(*label)
                .selected(mode == *option)
        }))
        .on_click(move |selected, _, app| {
            if let Some((option, _)) = selected.first().and_then(|index| OPTIONS.get(*index)) {
                let option = *option;
                let _ = root.update(app, |this, cx| {
                    set(this, option);
                    cx.notify();
                });
            }
        })
}

/// Text of a whole-document JSON view in `mode`.
pub(in crate::app) fn document_json_text(
    document: &serde_json::Value,
    canonical: &str,
    mode: crate::cell_value::EjsonMode,
) -> String {
    match mode {
        crate::cell_value::EjsonMode::Canonical => canonical.to_string(),
        crate::cell_value::EjsonMode::Shell => crate::cell_value::shell_text(document, true)
            .unwrap_or_else(|| canonical.to_string()),
    }
}

pub(in crate::app) fn hash_key(parts: impl Hash) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut hasher);
    hasher.finish()
}

/// Identity of an `Arc<str>` allocation: stable across clones, so it is a
/// free change detector for text that is replaced rather than mutated.
pub(in crate::app) fn arc_key(text: &std::sync::Arc<str>) -> (usize, usize) {
    (text.as_ptr() as usize, text.len())
}

impl JsonViewer {
    /// Ensures `slot` shows the text identified by `key`. `text` runs only
    /// when the key changed; returns whether it did.
    pub(in crate::app) fn sync(
        slot: &mut Option<JsonViewer>,
        key: u64,
        style: JsonViewerStyle,
        text: impl FnOnce() -> String,
        window: &mut Window,
        cx: &mut Context<AppRoot>,
    ) -> bool {
        if let Some(viewer) = slot.as_mut() {
            if viewer.key == key {
                return false;
            }
            let text = text();
            viewer.key = key;
            viewer.state.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.set_soft_wrap(style.soft_wrap, window, cx);
            });
            return true;
        }
        let text = text();
        let state = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .line_number(style.line_numbers)
                .folding(style.folding)
                .soft_wrap(style.soft_wrap)
                .default_value(text)
        });
        *slot = Some(JsonViewer { state, key });
        true
    }
}

impl AppRoot {
    /// Runs before a shown tab's results render: makes sure the JSON viewers
    /// that the tab's view will display exist and hold the right text. Every key
    /// is an O(1) identity (ids, `Arc` pointers, revision counters); text is only
    /// built inside the lazy closures, when a key changed.
    pub(in crate::app) fn sync_json_viewers(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return;
        };
        let is_mongo = tab.query_mode.is_mongo_find();
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return;
        };
        // Direct field access keeps the borrow disjoint from `self.workspace`.
        let view = self.tab_views.entry(tab_id).or_default();
        // A document editor belongs to the document it was opened on; a new
        // page or selection (a re-run, a commit's refresh) leaves it behind.
        if view
            .mongo
            .document_edit
            .as_ref()
            .is_some_and(|edit| runtime.mongo.inspector.selected != Some(edit.key))
        {
            view.mongo.document_edit = None;
        }
        let views = &mut view.json_views;
        let mut rebuilt = 0u64;

        // Row tab JSON view (whole row).
        let detail = &runtime.cell_detail;
        if detail.open
            && detail.tab == CellDetailTab::Row
            && detail.row_view == crate::cell_detail::RowView::Json
        {
            if let Some(row) = detail.row.as_ref() {
                let mode = if is_mongo {
                    detail.row_json_mode
                } else {
                    crate::cell_value::EjsonMode::Canonical
                };
                let key = hash_key((tab_id, row.row_number, arc_key(&row.json), mode as u8));
                rebuilt += u64::from(JsonViewer::sync(
                    &mut views.cell_json,
                    key,
                    JsonViewerStyle { soft_wrap: false, line_numbers: true, folding: true },
                    || document_json_text(&row.document, &row.json, mode),
                    window,
                    cx,
                ));
            }
        }

        // Details tab value box (JSON-like values only, while not editing).
        let editing = view.detail_ui.edit.is_some();
        let mode = view.detail_ui.value_mode;
        if detail.open && detail.tab == CellDetailTab::Details && !editing {
            if let Some((cell, canonical)) = detail
                .detail
                .as_ref()
                .and_then(|cell| Some((cell, cell.formatted_json.as_ref()?)))
            {
                let key = hash_key((
                    tab_id,
                    cell.position.row,
                    cell.position.column,
                    mode as u8,
                    arc_key(&cell.value),
                    is_mongo,
                ));
                rebuilt += u64::from(JsonViewer::sync(
                    &mut views.cell_value,
                    key,
                    JsonViewerStyle {
                        soft_wrap: mode == crate::cell_value::ValueMode::Raw,
                        line_numbers: false,
                        folding: false,
                    },
                    || crate::cell_value::value_text(&cell.value, canonical, mode, is_mongo),
                    window,
                    cx,
                ));
            }
        }

        // Mongo document view (JSON mode), staged updates applied.
        if is_mongo
            && runtime.mongo.view == crate::mongo_result::MongoResultView::Document
            && runtime.mongo.document_view == crate::mongo_result::MongoDocumentView::Json
        {
            if let Some(snapshot) = runtime.mongo.inspector.snapshot.as_ref() {
                let selected = runtime.mongo.inspector.selected.map(|key| key.row_index as usize);
                // Identity of the inputs: the snapshot allocation and its
                // document key, the selected row and the mutation history
                // revision (bumped by every change to the active set).
                let key = hash_key((
                    tab_id,
                    std::sync::Arc::as_ptr(snapshot) as usize,
                    snapshot.key.page_id,
                    snapshot.key.row_index,
                    selected,
                    runtime.mongo.mutations.revision(),
                    view.mongo.document_json_mode as u8,
                ));
                let mode = view.mongo.document_json_mode;
                rebuilt += u64::from(JsonViewer::sync(
                    &mut views.mongo_doc,
                    key,
                    JsonViewerStyle { soft_wrap: false, line_numbers: true, folding: true },
                    || match selected {
                        Some(row) if !runtime.mongo.mutations.is_empty() => {
                            let document = crate::mongo_result::apply_staged_updates(
                                &snapshot.document,
                                row,
                                &runtime.mongo.mutations,
                            );
                            let canonical = crate::mongo_result::pretty_json(&document);
                            document_json_text(&document, &canonical, mode)
                        }
                        _ => document_json_text(&snapshot.document, &snapshot.json, mode),
                    },
                    window,
                    cx,
                ));
            }
        }
        views.text_builds += rebuilt;
    }
}
