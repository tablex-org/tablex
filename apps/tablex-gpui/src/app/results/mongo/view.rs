use super::super::super::*;

impl AppRoot {
    fn render_mongo_json_panel(
        &self,
        tab_id: u64,
        snapshot: Option<&crate::mongo_result::MongoDocumentSnapshot>,
    ) -> AnyElement {
        let (Some(_), Some(viewer)) = (snapshot, self.view(tab_id).json_views.mongo_doc.as_ref())
        else {
            return div()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Select a document to inspect")
                .into_any_element();
        };
        div()
            .debug_selector(|| "mongo-json-editor".into())
            .size_full()
            .flex_1()
            .min_h_0()
            .font_family("Menlo")
            .text_size(px(12.0))
            .child(Editor::new(&viewer.state).readonly(true).appearance(false).bordered(false).h_full())
            .into_any_element()
    }

    /// The Document view's body while its document is being edited: the code
    /// editor on the document text, and under it why the last Save was refused.
    /// Escape cancels and Save Script's chord (Cmd/Ctrl+S) saves while the
    /// editor has focus.
    fn render_mongo_document_editor(
        &self,
        tab_id: u64,
        edit: &super::document_edit::MongoDocumentEdit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .debug_selector(|| "mongo-document-editor".into())
            .size_full()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .on_action(cx.listener(move |this, _: &SaveActiveSql, window, cx| {
                this.save_mongo_document_edit(tab_id, window, cx);
            }))
            .capture_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                if !event.is_held && event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.cancel_mongo_document_edit(tab_id, window, cx);
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .font_family("Menlo")
                    .text_size(px(12.0))
                    .child(Editor::new(&edit.editor).appearance(false).bordered(false).h_full()),
            )
            .when_some(edit.error.clone(), |this, error| {
                this.child(
                    div()
                        .debug_selector(|| "mongo-document-edit-error".into())
                        .flex_shrink_0()
                        .px(px(12.0))
                        .py(px(6.0))
                        .border_t_1()
                        .border_color(theme::border())
                        .text_size(px(12.0))
                        .text_color(theme::danger())
                        .child(error),
                )
            })
            .child(
                div()
                    .debug_selector(|| "mongo-document-edit-hint".into())
                    .flex_shrink_0()
                    .px(px(12.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(theme::border())
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child("New fields are added at the end; the order of top-level fields is not changed."),
            )
            .into_any_element()
    }

    fn render_mongo_tree_panel(
        &self,
        tab_id: u64,
        snapshot: Option<&crate::mongo_result::MongoDocumentSnapshot>,
        expanded_paths: &std::collections::HashSet<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(snapshot) = snapshot else {
            return div()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Select a document to inspect")
                .into_any_element();
        };
        let rows = crate::mongo_result::flatten_tree(&snapshot.document, expanded_paths);
        let list = uniform_list(
            "mongo-tree-lines",
            rows.len(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        let row = rows[index].clone();
                        let path = row.path.clone();
                        let expandable = row.container;
                        let is_expanded = this
                            .workspace
                            .runtime(tab_id)
                            .map(|runtime| runtime.mongo.inspector.expanded_paths.contains(&path))
                            .unwrap_or(false);
                        let mut element = div()
                            .id(("mongo-tree-row", index))
                            .h(px(24.0))
                            .debug_selector(|| format!("mongo-tree-row-{index}"))
                            .flex()
                            .items_center()
                            .pl(px((row.depth as f32) * 18.0 + 10.0))
                            .gap(px(6.0))
                            .text_size(px(12.0));
                        if expandable {
                            element = element.child(
                                div()
                                    .w(px(12.0))
                                    .text_color(theme::text_muted())
                                    .child(if is_expanded { "v" } else { ">" }),
                            );
                            element = element.on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(runtime) = this.workspace.runtime_mut(tab_id) {
                                    if !runtime.mongo.inspector.expanded_paths.remove(&path) {
                                        runtime.mongo.inspector.expanded_paths.insert(path.clone());
                                    }
                                }
                                this.notify_tab(tab_id, cx);
                            }));
                        } else {
                            element = element.child(div().w(px(12.0)));
                        }
                        element
                            .when(!row.key.is_empty(), |element| {
                                element.child(
                                    div()
                                        .text_color(theme::accent())
                                        .child(format!("{}:", row.key)),
                                )
                            })
                            .child(
                                div()
                                    .text_color(mongo_tree_value_color(&row.type_name, row.container))
                                    .child(row.value),
                            )
                            .into_any_element()
                    })
                    .collect()
            }),
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .flex_1()
        .h_full()
        .min_h_0()
        .track_scroll(&self.view(tab_id).mongo_detail_scroll);
        div()
            .size_full()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(list)
            .into_any_element()
    }

    pub(in crate::app) fn render_mongo_documents(
        &self,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(runtime) = self.workspace.runtime(tab_id) else {
            return div().into_any_element();
        };
        let selected = runtime
            .mongo
            .inspector
            .selected
            .map(|key| key.row_index as usize);
        let document_view = runtime.mongo.document_view;
        let view = self.view(tab_id);
        let document_json_mode = view.mongo.document_json_mode;
        let editability = self.mongo_document_editability(tab_id);
        let editing = view.mongo.document_edit.is_some();
        let loading = runtime.mongo.inspector.loading;
        let error = runtime.mongo.inspector.error.clone();
        let snapshot = runtime.mongo.inspector.snapshot.clone();
        let has_mutations = !runtime.mongo.mutations.is_empty();
        let mutations_for_tree = (has_mutations
            && document_view == crate::mongo_result::MongoDocumentView::Tree)
            .then(|| runtime.mongo.mutations.clone());
        let expanded_paths = runtime.mongo.inspector.expanded_paths.clone();
        // The JSON view reads its text from the cached viewer (keyed on the
        // history revision), so the snapshot only needs its document
        // recomputed for the tree; never re-pretty-print per frame.
        let snapshot = if let Some(mutations) = mutations_for_tree {
            snapshot.map(|snapshot| {
                let document = selected
                    .map(|row| {
                        crate::mongo_result::apply_staged_updates(
                            &snapshot.document,
                            row,
                            &mutations,
                        )
                    })
                    .unwrap_or_else(|| snapshot.document.clone());
                std::sync::Arc::new(crate::mongo_result::MongoDocumentSnapshot {
                    document,
                    ..(*snapshot).clone()
                })
            })
        } else {
            snapshot
        };
        let grid = &runtime.grid;
        let id_column = (0..grid.column_count()).find(|column| grid.column_name(*column) == Some("_id"));
        let document_list = uniform_list(
            "mongo-document-list",
            grid.row_count(),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                range
                    .map(|row| {
                        let label = id_column
                            .and_then(|column| {
                                this.grid(tab_id)
                                    .cell_text(crate::grid::CellPosition { row, column })
                            })
                            .unwrap_or_else(|| format!("Document {}", row + 1).into());
                        let label = mongo_document_preview(label.as_ref());
                        let is_selected = selected == Some(row);
                        let is_deleted = this
                            .workspace
                            .runtime(tab_id)
                            .map(|runtime| runtime.mongo.mutations.is_deleted(row))
                            .unwrap_or(false);
                        let mut item = div()
                            .id(("mongo-document-row", row))
                            .debug_selector(|| format!("mongo-document-row-{row}"))
                            .h(px(36.0))
                            .px(px(12.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(px(12.0))
                            .text_color(if is_deleted {
                                theme::danger()
                            } else {
                                theme::text()
                            })
                            .child(label);
                        if is_selected {
                            item = item.bg(theme::accent_surface());
                        } else if row % 2 == 1 {
                            item = item.bg(theme::surface());
                        }
                        if is_deleted {
                            item = item.child("Deleted");
                        }
                        item.on_click(cx.listener(move |this, _, _, cx| {
                            if !this.refuse_while_editing_mongo_document(tab_id, cx) {
                                this.select_mongo_document(tab_id, row, cx);
                            }
                        }))
                        .into_any_element()
                    })
                    .collect()
            }),
        )
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .flex_1()
        .h_full()
        .min_h_0()
        .track_scroll(&view.mongo_document_scroll);
        let left = div()
            .w(px(300.0))
            .min_w(px(240.0))
            .max_w(px(420.0))
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::chrome())
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .h(px(theme::HEADER_HEIGHT))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(format!("{} documents", grid.row_count())),
            )
            .child(document_list);
        let can_edit = editability == super::document_edit::DocumentEditability::Editable;
        let read_only_reason = match editability {
            super::document_edit::DocumentEditability::Editable => None,
            super::document_edit::DocumentEditability::ReadOnly(reason) => {
                Some(reason.map(gpui::SharedString::from))
            }
        };
        let identity = snapshot
            .as_ref()
            .map(|snapshot| match snapshot.identity.as_ref() {
                Some(id) => crate::mongo_result::ejson_scalar(id)
                    .map(|(display, _)| display)
                    .unwrap_or_else(|| serde_json::to_string(id).unwrap_or_default()),
                None => "no _id in this page".to_string(),
            })
            .unwrap_or_default();
        let body = if let Some(edit) = view.mongo.document_edit.as_ref() {
            self.render_mongo_document_editor(tab_id, edit, cx)
        } else if let Some(error) = error {
            div()
                .flex_1()
                .p(px(16.0))
                .text_color(theme::danger())
                .child(error)
                .into_any_element()
        } else if loading {
            div()
                .flex_1()
                .p(px(16.0))
                .text_color(theme::text_muted())
                .child("Loading document...")
                .into_any_element()
        } else if document_view == crate::mongo_result::MongoDocumentView::Json {
            self.render_mongo_json_panel(tab_id, snapshot.as_deref())
        } else {
            self.render_mongo_tree_panel(tab_id, snapshot.as_deref(), &expanded_paths, cx)
        };
        let right = div()
            .debug_selector(|| "mongo-document-panel".into())
            // A landing place for focus when the editor closes.
            .when_some(view.grid_focus.clone(), |this, focus| this.track_focus(&focus))
            .flex_1()
            .min_w(px(0.0))
            .min_h_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(
                // The identity and "Read only" give way first (they truncate
                // and clip); the view controls on the right never shrink, so a
                // narrow pane cannot push them out of sight.
                div()
                    .debug_selector(|| "mongo-document-header".into())
                    .h(px(theme::HEADER_HEIGHT + 8.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .min_w(px(0.0))
                                    .px(px(8.0))
                                    .py(px(3.0))
                                    .bg(theme::surface())
                                    .rounded(px(4.0))
                                    .text_size(px(11.0))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(identity),
                            )
                            .when_some(read_only_reason, |this, reason| {
                                this.child(
                                    div()
                                        .id(("mongo-document-read-only", tab_id))
                                        .debug_selector(|| "mongo-document-read-only".into())
                                        .flex_shrink_0()
                                        .text_size(px(10.0))
                                        .text_color(theme::text_muted())
                                        .whitespace_nowrap()
                                        .when_some(reason, |this, reason| {
                                            this.tooltip(move |window, cx| {
                                                gpui_component::tooltip::Tooltip::new(reason.clone())
                                                    .build(window, cx)
                                            })
                                        })
                                        .child("Read only"),
                                )
                            }),
                    )
                    .when(editing, |this| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .debug_selector(|| "mongo-document-edit-actions".into())
                                .flex()
                                .items_center()
                                .gap(px(6.0))
                                .child(
                                    Button::new(("mongo-document-cancel", tab_id))
                                        .label("Cancel")
                                        .compact()
                                        .ghost()
                                        .with_size(Size::XSmall)
                                        .debug_selector(|| "mongo-document-cancel".into())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.cancel_mongo_document_edit(tab_id, window, cx);
                                        })),
                                )
                                .child(
                                    Button::new(("mongo-document-save", tab_id))
                                        .label("Save")
                                        .compact()
                                        .primary()
                                        .with_size(Size::XSmall)
                                        .debug_selector(|| "mongo-document-save".into())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.save_mongo_document_edit(tab_id, window, cx);
                                        })),
                                ),
                        )
                    })
                    .when(can_edit && !editing, |this| {
                        this.child(
                            Button::new(("mongo-document-edit", tab_id))
                                .icon(Icon::empty().path("icons/pencil.svg").size(px(12.0)))
                                .label("Edit")
                                .compact()
                                .ghost()
                                .with_size(Size::XSmall)
                                .tooltip("Edit document")
                                .flex_shrink_0()
                                .debug_selector(|| "mongo-document-edit".into())
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.start_mongo_document_edit(tab_id, window, cx);
                                })),
                        )
                    })
                    .when(
                        document_view == crate::mongo_result::MongoDocumentView::Json,
                        |this| {
                            this.child(
                                div().flex_shrink_0().debug_selector(|| "mongo-json-mode".into()).child(
                                    if editing {
                                        // The text is in this mode; changing it would rewrite it.
                                        div()
                                            .text_size(px(12.0))
                                            .text_color(theme::text_muted())
                                            .child(match document_json_mode {
                                                crate::cell_value::EjsonMode::Shell => "Shell",
                                                crate::cell_value::EjsonMode::Canonical => "Canonical",
                                            })
                                            .into_any_element()
                                    } else {
                                        super::super::json_view::ejson_mode_toggle(
                                            ("mongo-json-mode-toggle", tab_id),
                                            document_json_mode,
                                            cx.entity().downgrade(),
                                            move |this, mode| this.view_mut(tab_id).mongo.document_json_mode = mode,
                                        )
                                        .into_any_element()
                                    },
                                ),
                            )
                        },
                    )
                    .child(
                        div().flex_shrink_0().debug_selector(|| "mongo-document-view".into()).child(
                        Button::new(("mongo-document-view", tab_id))
                            .icon(
                                Icon::new(match document_view {
                                    crate::mongo_result::MongoDocumentView::Json => {
                                        IconName::FolderOpen
                                    }
                                    crate::mongo_result::MongoDocumentView::Tree => IconName::File,
                                })
                                .size(px(14.0)),
                            )
                            .compact()
                            .ghost()
                            .disabled(editing)
                            .tooltip(match document_view {
                                crate::mongo_result::MongoDocumentView::Json => "Tree view",
                                crate::mongo_result::MongoDocumentView::Tree => "JSON view",
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_mongo_document_view(tab_id, cx);
                            })),
                        ),
                    ),
            )
            .child(body);
        gpui_component::resizable::h_resizable(("mongo-inspector", tab_id))
            .child(
                gpui_component::resizable::resizable_panel()
                    .size(px(300.0))
                    .size_range(px(160.0)..px(420.0))
                    .child(left),
            )
            .child(
                gpui_component::resizable::resizable_panel()
                    .size_range(px(240.0)..gpui::Pixels::MAX)
                    .child(right),
            )
            .into_any_element()
    }
}
