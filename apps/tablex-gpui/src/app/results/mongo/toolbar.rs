use super::super::super::*;

impl AppRoot {
    pub(in crate::app) fn render_mongo_result_toolbar(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let collection = self
            .workspace
            .tab(tab_id)
            .map(|tab| tab.collection.clone())
            .unwrap_or_default();
        let (
            view,
            pending,
            commit_status,
            filter,
            sort,
            known_columns,
            visible_columns,
            projection_draft_columns,
            column_open,
            column_tab,
            column_search,
            find_expanded,
            sort_expanded,
            query_preview_open,
            project,
            filter_sort_ratio,
            filter_open,
            filter_count,
            filter_error,
            filter_rules,
        ) = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| {
                (
                    runtime.mongo.view,
                    runtime.mongo.mutations.len(),
                    runtime.mongo.commit_status.clone(),
                    runtime.mongo.filter.clone(),
                    runtime.mongo.sort.clone(),
                    runtime.mongo.known_columns.clone(),
                    runtime.mongo.visible_columns.clone(),
                    runtime.mongo.projection_draft_columns.clone(),
                    runtime.mongo.column_open,
                    runtime.mongo.column_tab,
                    runtime.mongo.column_search.clone(),
                    runtime.mongo.find_expanded,
                    runtime.mongo.sort_expanded,
                    runtime.mongo.query_preview_open,
                    runtime.mongo.project.clone(),
                    runtime.mongo.filter_sort_ratio_clamped(),
                    runtime.mongo.filter_open,
                    if runtime.mongo.applied_filter.is_some() {
                        runtime
                            .mongo
                            .filter_rules
                            .iter()
                            .filter(|rule| {
                                !rule.field.trim().is_empty()
                                    && (!rule.mode.needs_value() || !rule.value.trim().is_empty())
                            })
                            .count()
                    } else {
                        0
                    },
                    runtime.mongo.filter_error.clone(),
                    runtime.mongo.filter_rules.clone(),
                )
            })
            .unwrap_or_else(|| {
                (
                    crate::mongo_result::MongoResultView::Table,
                    0,
                    crate::mongo_result::MongoCommitStatus::Idle,
                    "{}".into(),
                    r#"{"_id":1}"#.into(),
                    Vec::new(),
                    HashSet::new(),
                    HashSet::new(),
                    false,
                    crate::mongo_result::MongoColumnTab::Visible,
                    String::new(),
                    false,
                    false,
                    false,
                    String::new(),
                    0.5,
                    false,
                    0,
                    None,
                    Vec::new(),
                )
            });
        let commit_running = matches!(
            &commit_status,
            crate::mongo_result::MongoCommitStatus::Running
        );
        let filter_input = self.mongo_filter_editor(tab_id, window, cx);
        let sort_input = self.mongo_sort_editor(tab_id, window, cx);
        let expanded_find_editor =
            find_expanded.then(|| self.mongo_expanded_find_editor(tab_id, window, cx));
        let can_run = pending == 0
            && !commit_running
            && mongo_json_object_is_valid(&filter)
            && mongo_json_object_is_valid(&sort)
            && filter_error.is_none()
            && self
                .workspace
                .runtime(tab_id)
                .map(|runtime| runtime.query.can_run())
                .unwrap_or(false);
        let filter_label_button = Button::new(("mongo-filter-options", tab_id))
            .compact()
            .text()
            .selected(filter_open || filter_count > 0)
            .child(
                div()
                    .debug_selector(|| "mongo-filter-toolbar-text".into())
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .text_color(theme::accent())
                    .child("FILTER"),
            )
            .tooltip("Build filter");
        let grid = self.grid(tab_id);
        let result_columns: Vec<String> = (0..grid.column_count())
            .filter_map(|column| grid.column_name(column).map(str::to_string))
            .collect();
        let filter_popover = self.render_mongo_filter_popover(
            tab_id,
            result_columns.clone(),
            filter_rules,
            filter_error,
            filter_open,
            filter_label_button,
            window,
            cx,
        );
        let filter_trigger = div()
            .debug_selector(|| "mongo-filter-label".into())
            .relative()
            .flex_shrink_0()
            .h(px(24.0))
            .flex()
            .items_center()
            .child(filter_popover)
            .when(filter_count > 0, |this| {
                this.child(
                    div()
                        .absolute()
                        .right(px(-4.0))
                        .top(px(-2.0))
                        .min_w(px(14.0))
                        .h(px(14.0))
                        .px(px(3.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(7.0))
                        .bg(theme::accent())
                        .text_size(px(9.0))
                        .text_color(theme::background())
                        .child(filter_count.to_string()),
                )
            });
        let query_preview_button = Button::new(("mongo-query-preview-button", tab_id))
            .compact()
            .text()
            .selected(query_preview_open)
            .child(
                div()
                    .debug_selector(|| "mongo-query-toolbar-text".into())
                    .text_size(px(11.0))
                    .line_height(px(14.0))
                    .child("Query"),
            )
            .tooltip("Show generated Mongo query");
        let query_preview_popover = self.render_mongo_query_preview_popover(
            tab_id,
            collection,
            filter.clone(),
            sort.clone(),
            project.clone(),
            query_preview_open,
            query_preview_button,
            cx,
        );
        let query_preview_control = div()
            .debug_selector(|| "mongo-query-preview".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(query_preview_popover);
        let column_search_editor = self.mongo_column_search_editor(tab_id, window, cx);
        let column_button = Button::new(("mongo-columns", tab_id))
            .debug_selector(|| "mongo-columns-button".into())
            .icon(Icon::empty().path("icons/list-checks.svg").size(px(13.0)))
            .label(mongo_columns_button_label(&visible_columns, &known_columns))
            .xsmall()
            .compact()
            .selected(column_open)
            .disabled(known_columns.is_empty())
            .tooltip("Columns");
        let column_popover = self.render_mongo_column_popover(
            tab_id,
            known_columns,
            visible_columns,
            projection_draft_columns,
            column_tab,
            column_search,
            pending,
            column_open,
            column_button,
            column_search_editor,
            cx,
        );
        let column_control = div()
            .debug_selector(|| "mongo-columns".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(column_popover);
        let find_expand_button = Button::new(("mongo-find-expand", tab_id))
            .icon(Icon::new(IconName::Maximize).size(px(12.0)))
            .compact()
            .ghost()
            .selected(find_expanded)
            .tooltip("Expand Find Query");
        let find_popover = self.render_mongo_find_popover(
            tab_id,
            find_expanded,
            can_run,
            expanded_find_editor,
            find_expand_button,
            cx,
        );
        let can_clear_find =
            filter_count > 0 || (!filter.trim().is_empty() && filter.trim() != "{}");
        let clear_find = Button::new(("mongo-find-clear", tab_id))
            .icon(Icon::new(IconName::Close).size(px(11.0)))
            .compact()
            .ghost()
            .tooltip("Clear Find Query")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.clear_mongo_filters(tab_id, window, cx);
            }));
        let find_control = div()
            .debug_selector(|| "mongo-find-expand".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .when(can_clear_find, |this| this.child(clear_find))
            .child(find_popover);
        let expanded_sort_editor =
            sort_expanded.then(|| self.mongo_expanded_sort_editor(tab_id, window, cx));
        let sort_expand_button = Button::new(("mongo-sort-expand-button", tab_id))
            .icon(Icon::new(IconName::Maximize).size(px(12.0)))
            .compact()
            .ghost()
            .selected(sort_expanded)
            .tooltip("Expand Sort Query");
        let sort_popover = self.render_mongo_sort_popover(
            tab_id,
            sort_expanded,
            can_run,
            expanded_sort_editor,
            sort_expand_button,
            cx,
        );
        let can_clear_sort = !sort.trim().is_empty();
        let clear_sort = Button::new(("mongo-sort-clear", tab_id))
            .debug_selector(|| "mongo-sort-clear".into())
            .icon(Icon::new(IconName::Close).size(px(11.0)))
            .compact()
            .ghost()
            .tooltip("Clear Sort Query")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.clear_mongo_sort(tab_id, window, cx);
            }));
        let sort_control = div()
            .debug_selector(|| "mongo-sort-expand".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .when(can_clear_sort, |this| this.child(clear_sort))
            .child(sort_popover);
        // View switch: a bordered, rounded segmented group of two
        // 20x20 buttons with 12px icons, Document (Braces) first, Table
        // (Table2) second; the active one has the accent background.
        let view_button = |id: &'static str,
                           icon: &'static str,
                           selected: bool,
                           view: crate::mongo_result::MongoResultView| {
            let (bg, fg, hover) = if selected {
                (theme::accent_surface(), theme::text(), theme::accent_surface())
            } else {
                (gpui::rgba(0x00000000), theme::text_muted(), theme::hover())
            };
            Button::new((id, tab_id))
                .debug_selector(move || id.into())
                .icon(Icon::empty().path(icon).size(px(12.0)))
                .xsmall()
                .compact()
                .custom(
                    gpui_component::button::ButtonCustomVariant::new(cx)
                        .color(bg.into())
                        .foreground(fg.into())
                        .hover(hover.into())
                        .active(theme::accent_surface().into()),
                )
                .rounded(px(0.0))
                .w(px(20.0))
                .h(px(20.0))
                .tooltip(match view {
                    crate::mongo_result::MongoResultView::Table => "Table view",
                    crate::mongo_result::MongoResultView::Document => "Document view",
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_mongo_view(tab_id, view, cx);
                }))
        };
        let view_switch = div()
            .debug_selector(|| "mongo-view-switch".into())
            .flex()
            .flex_shrink_0()
            .items_center()
            .h(px(22.0))
            .rounded(px(6.0))
            .overflow_hidden()
            .border_1()
            .border_color(theme::border())
            .child(view_button(
                "mongo-view-document",
                "icons/braces.svg",
                view == crate::mongo_result::MongoResultView::Document,
                crate::mongo_result::MongoResultView::Document,
            ))
            .child(div().w(px(1.0)).h_full().flex_shrink_0().bg(theme::border()))
            .child(view_button(
                "mongo-view-table",
                "icons/table-2.svg",
                view == crate::mongo_result::MongoResultView::Table,
                crate::mongo_result::MongoResultView::Table,
            ));
        let filter_ratio = filter_sort_ratio as f32;
        let filter_field = div()
            .debug_selector(|| "mongo-filter-field".into())
            .flex()
            .flex_grow(1.0)
            .flex_basis(gpui::relative(filter_ratio))
            .min_w(px(super::super::sql_table::FIELD_MIN_WIDTH))
            .h(px(24.0))
            .items_center()
            .gap(px(4.0))
            .child(filter_trigger)
            .child(
                div().flex_1().min_w(px(0.0)).child(
                    Input::new(&filter_input)
                        .appearance(false)
                        .disabled(find_expanded),
                ),
            )
            .child(find_control);
        let sort_field = div()
            .debug_selector(|| "mongo-sort-field".into())
            .flex()
            .flex_grow(1.0)
            .flex_basis(gpui::relative(1.0 - filter_ratio))
            .min_w(px(super::super::sql_table::FIELD_MIN_WIDTH))
            .h(px(24.0))
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.0))
                    .text_color(theme::warning())
                    .child("SORT"),
            )
            .child(
                div().flex_1().min_w(px(0.0)).child(
                    Input::new(&sort_input)
                        .appearance(false)
                        .disabled(sort_expanded),
                ),
            )
            .child(sort_control);
        let filter_sort_handle = div()
            .id(("mongo-filter-sort-handle-drag", tab_id))
            .debug_selector(|| "mongo-filter-sort-handle".into())
            .w(px(12.0))
            .h(px(24.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .child(div().w(px(1.0)).h(px(22.0)).bg(theme::border()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, _| {
                    this.begin_filter_sort_resize(tab_id, event.position.x, window);
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| this.end_filter_sort_resize(tab_id)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(move |this, _, _, _| this.end_filter_sort_resize(tab_id)),
            )
            .on_drag(tab_id, |_, _, _, cx| cx.new(|_| ColumnResizeGhost))
            .on_drag_move(
                cx.listener(move |this, event: &gpui::DragMoveEvent<u64>, _, cx| {
                    if *event.drag(cx) == tab_id && this.view(tab_id).mongo.filter_sort_drag.is_some()
                    {
                        this.update_filter_sort_resize(tab_id, event.event.position.x);
                        this.notify_tab(tab_id, cx);
                    }
                }),
            );
        let query_divider = || div().w(px(1.0)).h(px(22.0)).bg(theme::border());
        let run_button = Button::new(("mongo-run", tab_id))
            .icon(Icon::empty().path("icons/play.svg").size(px(11.0)))
            .small()
            .compact()
            .tooltip("Execute query")
            .disabled(!can_run)
            .on_click(cx.listener(move |this, _, _, cx| this.run_tab_query_explicit(tab_id, cx)));
        let run_control = div()
            .debug_selector(|| "mongo-run".into())
            .flex()
            .flex_shrink_0()
            .h(px(24.0))
            .items_center()
            .child(run_button);
        let query_capsule = div()
            .debug_selector(|| "mongo-query-capsule".into())
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .h(px(30.0))
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .bg(theme::background())
            .border_1()
            .border_color(theme::border())
            .child(query_preview_control)
            // Filter and sort shrink and clip inside their own lane, so a
            // narrow pane never pushes Run out of the capsule.
            .child(
                div()
                    .debug_selector(|| "mongo-query-fields".into())
                    .flex()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .items_center()
                    .gap(px(8.0))
                    .child(filter_field)
                    .child(filter_sort_handle)
                    .child(sort_field),
            )
            .child(query_divider().flex_shrink_0())
            .child(run_control);
        let toolbar = div()
            .debug_selector(|| "mongo-result-toolbar".into())
            .h(px(theme::RESULT_TOOLBAR_HEIGHT))
            .relative()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .flex_shrink_0()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(view_switch)
            .child(query_capsule)
            .child(div().flex_shrink_0().child(column_control));
        toolbar.into_any_element()
    }
}
