//! Compare DDL: two tables or views of the tree (of one connection or two) shown as a line diff in a read-only tab.
//!
//! "Compare DDL" with exactly two of them selected, or "Compare DDL With…" on one, which asks for the second through the Go to
//! Object dialog. The two definitions are fetched the way the Properties DDL and the source tabs fetch theirs (`describe_object`
//! on each connection's session, off the UI thread). `similar` makes the unified patch (the crate gpui-component's diff view
//! already builds on); the view is the component's `Diff`, read-only, with removed and added lines coloured and the changed
//! words inside a line marked. It is shown unified: a DDL line is long and the tab is narrow, and the component gives two
//! columns only half the width each.
//!
//! The tab holds what was fetched and is never persisted. Refresh fetches both again. No migration script is made.

use gpui_component::diff::{Diff, DiffFile, DiffState};

use super::*;
use engine_driver_api::sql::{SchemaObjectKind, SchemaObjectRef, SchemaObjectSource};

/// Where a compare tab stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CompareStatus {
    Loading,
    Ready,
    Failed(String),
}

/// One side of a compare.
pub(super) struct CompareSide {
    pub(super) id: NavigatorNodeId,
    /// `connection · schema.object`: what the toolbar calls it.
    pub(super) label: String,
}

pub(super) struct DdlCompareState {
    pub(super) left: CompareSide,
    pub(super) right: CompareSide,
    pub(super) status: CompareStatus,
    /// The unified patch of the two definitions (what Copy hands out); `None`: they are identical, or not read yet.
    pub(super) patch: Option<String>,
    pub(super) additions: usize,
    pub(super) deletions: usize,
    diff: Option<gpui::Entity<DiffState>>,
    /// Orders fetches: the answer of an older one is dropped.
    fetch: u64,
    task: Option<Task<()>>,
}

/// The unified patch that turns `left` into `right` (3 lines of context), `None` when they are the same text. The two sides are
/// always named `left.sql` and `right.sql` in it: the view shows its own labels.
pub(super) fn unified_patch(left: &str, right: &str) -> Option<String> {
    let (left, right) = (with_final_newline(left), with_final_newline(right));
    if left == right {
        return None;
    }
    let patch = similar::TextDiff::from_lines(&left, &right).unified_diff().context_radius(3).header("a/left.sql", "b/right.sql").to_string();
    Some(patch)
}

/// A last line without its newline would make the patch say so on a line of its own.
fn with_final_newline(text: &str) -> String {
    let mut text = text.replace("\r\n", "\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

impl AppRoot {
    /// Whether the row `id` is a table, view or materialized view (what has a DDL to compare).
    fn comparable(&self, id: &NavigatorNodeId) -> bool {
        matches!(id, NavigatorNodeId::Object { .. }) && object_actions::ObjectKind::of(self.navigator.object_kind(id)).is_some()
    }

    /// The two rows "Compare DDL" on `id` compares: `id` and the other one of exactly two selected tables or views.
    pub(super) fn compare_pair(&self, id: &NavigatorNodeId) -> Option<(NavigatorNodeId, NavigatorNodeId)> {
        let selected = self.navigator.selection_in_visible_order();
        if selected.len() != 2 || !selected.contains(id) || !selected.iter().all(|row| self.comparable(row)) {
            return None;
        }
        Some((selected[0].clone(), selected[1].clone()))
    }

    /// "Compare DDL With…" on `id`: asks for the second object with the Go to Object dialog.
    pub(super) fn compare_ddl_with(&mut self, id: &NavigatorNodeId, window: &mut Window, cx: &mut Context<Self>) {
        if self.comparable(id) {
            self.open_goto(Some(id.clone()), window, cx);
        }
    }

    /// `connection · schema.object`.
    fn compare_label(&self, id: &NavigatorNodeId) -> String {
        let NavigatorNodeId::Object { connection, database, schema, object } = id else { return String::new() };
        let place = if schema.is_empty() { database } else { schema };
        let name = self.saved_connections.iter().find(|saved| saved.matches_id(connection)).map_or(connection.as_str(), |saved| saved.name.as_str());
        format!("{name} \u{00B7} {place}.{object}")
    }

    /// Opens a tab with the diff of the DDL of `left` and `right`, and fetches both.
    pub(super) fn open_ddl_compare(&mut self, left: NavigatorNodeId, right: NavigatorNodeId, cx: &mut Context<Self>) {
        if !self.comparable(&left) || !self.comparable(&right) {
            return;
        }
        let (NavigatorNodeId::Object { connection, object: left_name, .. }, NavigatorNodeId::Object { object: right_name, .. }) = (&left, &right) else { return };
        let title = format!("Compare {left_name} \u{2194} {right_name}");
        let tab_id = self.workspace.open_compare_tab(connection, &title);
        let state = DdlCompareState {
            left: CompareSide { label: self.compare_label(&left), id: left },
            right: CompareSide { label: self.compare_label(&right), id: right },
            status: CompareStatus::Loading,
            patch: None,
            additions: 0,
            deletions: 0,
            diff: None,
            fetch: 0,
            task: None,
        };
        self.compare_tabs.insert(tab_id, state);
        self.fetch_compare(tab_id, cx);
        self.activate_tab_ui(tab_id, cx);
        self.notify_chrome(cx);
        cx.notify();
    }

    /// Fetches both definitions of the tab again (Refresh, and the first time).
    pub(super) fn fetch_compare(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some((left, right)) = self.compare_tabs.get(&tab_id).map(|state| (state.left.id.clone(), state.right.id.clone())) else { return };
        let (left_task, right_task) = (self.compare_ddl_task(&left, cx), self.compare_ddl_task(&right, cx));
        let Some(state) = self.compare_tabs.get_mut(&tab_id) else { return };
        state.status = CompareStatus::Loading;
        state.fetch += 1;
        let fetch = state.fetch;
        state.task = Some(cx.spawn(async move |this, cx| {
            let (left, right) = (left_task.await, right_task.await);
            let _ = this.update(cx, |this, cx| this.compare_fetched(tab_id, fetch, left, right, cx));
        }));
        cx.notify();
    }

    /// The definition of the table, view or materialized view `id`, off the UI thread.
    fn compare_ddl_task(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) -> Task<Result<String, String>> {
        let NavigatorNodeId::Object { connection, database, schema, object } = id else {
            return cx.spawn(async move |_, _| Err("This row has no definition.".to_owned()));
        };
        let materialized = self.navigator.object_kind(id) == Some(KindOfObject::MaterializedView);
        let missing = |what: &str| format!("The server returned no definition for {what}.");
        if let Some(service) = self.scenario_ui.service.as_ref() {
            let result = if materialized {
                let reference = SchemaObjectRef { database: Some(database.clone()), schema: Some(schema.clone()), kind: SchemaObjectKind::MaterializedView, name: object.clone(), arguments: None, table: None, identity: None };
                crate::ui_fixture::schema_object_source(service.scenario().fixture, &reference).map_err(|error| error.to_string())
            } else {
                crate::ui_fixture_properties::table_properties(service.scenario().fixture, connection, schema, object)
                    .and_then(|properties| properties.ddl.filter(|ddl| !ddl.trim().is_empty()).ok_or_else(|| missing(object)))
            };
            return cx.spawn(async move |_, _| result);
        }
        let Some(session) = self.driver_runtime.sessions.get(connection).cloned() else {
            return cx.spawn(async move |_, _| Err("The connection is not open.".to_owned()));
        };
        let (database, schema, object) = (database.clone(), schema.clone(), object.clone());
        let work = cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<String, String> {
                let text = if materialized {
                    let reference = SchemaObjectRef { database: Some(database), schema: Some(schema), kind: SchemaObjectKind::MaterializedView, name: object.clone(), arguments: None, table: None, identity: None };
                    let request = serde_json::to_value(&reference).map_err(|error| error.to_string())?;
                    let described = session.describe_object(&request).map_err(|error| error.to_string())?;
                    serde_json::from_value::<SchemaObjectSource>(described).map(|described| described.source).map_err(|error| error.to_string())?
                } else {
                    let request = serde_json::to_value(engine_driver_api::sql::SqlObjectRef {
                        database: Some(database).filter(|database| !database.is_empty()),
                        schema: Some(schema).filter(|schema| !schema.is_empty()),
                        object: object.clone(),
                    })
                    .map_err(|error| error.to_string())?;
                    let described = session.describe_object(&request).map_err(|error| error.to_string())?;
                    serde_json::from_value::<crate::catalog::TableProperties>(described).map_err(|error| error.to_string())?.ddl.unwrap_or_default()
                };
                Some(text).filter(|text| !text.trim().is_empty()).ok_or_else(|| format!("The server returned no definition for {object}."))
            })
            .await
        });
        cx.spawn(async move |_, _| work.await.map_err(|error| crate::sessions::redact_secrets(&error)))
    }

    fn compare_fetched(&mut self, tab_id: u64, fetch: u64, left: Result<String, String>, right: Result<String, String>, cx: &mut Context<Self>) {
        let Some(state) = self.compare_tabs.get_mut(&tab_id).filter(|state| state.fetch == fetch) else { return };
        state.task = None;
        let (left, right) = match (left, right) {
            (Ok(left), Ok(right)) => (left, right),
            (Err(error), _) | (_, Err(error)) => {
                state.status = CompareStatus::Failed(error);
                return cx.notify();
            }
        };
        state.patch = unified_patch(&left, &right);
        let files = state.patch.as_deref().map(DiffFile::parse).transpose();
        match files {
            Ok(files) => {
                let files: Vec<DiffFile> = files.into_iter().flatten().map(|file| file.with_language("sql")).collect();
                state.additions = files.iter().map(DiffFile::additions).sum();
                state.deletions = files.iter().map(DiffFile::deletions).sum();
                match (&state.diff, files.is_empty()) {
                    (_, true) => state.diff = None,
                    (Some(diff), false) => diff.update(cx, |diff, cx| diff.set_files(files, cx)),
                    (None, false) => state.diff = Some(cx.new(|cx| DiffState::new(files, cx))),
                }
                state.status = CompareStatus::Ready;
            }
            Err(error) => state.status = CompareStatus::Failed(error.to_string()),
        }
        cx.notify();
    }

    fn copy_compare(&self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(patch) = self.compare_tabs.get(&tab_id).and_then(|state| state.patch.clone()) {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(patch));
        }
    }

    /// The content of a compare tab: the toolbar (what is compared, Copy, Refresh) over the diff, or the state the fetch is in.
    pub(super) fn render_compare_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let Some(state) = self.compare_tabs.get(&tab_id) else {
            return div().id(("compare-tab", tab_id)).flex_1();
        };
        let (status, diff, has_patch) = (state.status.clone(), state.diff.clone(), state.patch.is_some());
        let (left, right) = (state.left.label.clone(), state.right.label.clone());
        let counts = (state.additions, state.deletions);
        let button = |id: &'static str, icon: &'static str, label: std::borrow::Cow<'static, str>, enabled: bool| {
            Button::new((id, tab_id)).icon(Icon::empty().path(icon).size(px(14.0))).label(label).with_size(Size::Small).compact().outline().disabled(!enabled)
        };
        let wrap = |id: &'static str, button: Button| div().debug_selector(move || id.into()).child(button);
        let actions = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .child(wrap(
                "compare-copy",
                button("compare-copy", "icons/copy.svg", t!("menu.copy"), has_patch).on_click(cx.listener(move |this, _, _, cx| this.copy_compare(tab_id, cx))),
            ))
            .child(wrap(
                "compare-refresh",
                button("compare-refresh", "icons/refresh-cw.svg", t!("menu.refresh"), status != CompareStatus::Loading).on_click(cx.listener(move |this, _, _, cx| this.fetch_compare(tab_id, cx))),
            ));
        let side = |sign: &'static str, label: String, color: gpui::Rgba| {
            div().flex().items_center().gap(px(4.0)).text_size(px(11.0)).text_color(color).child(sign).child(div().text_color(theme::text()).child(label))
        };
        let legend = div()
            .debug_selector(|| "compare-legend".into())
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(16.0))
            .px(px(12.0))
            .py(px(4.0))
            .border_b_1()
            .border_color(theme::border())
            .child(side("\u{2212}", left, theme::danger()))
            .child(side("+", right, theme::success()))
            .child(div().flex_1())
            .when(status == CompareStatus::Ready, |this| {
                this.child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(t!("sidebar.compare_counts", added = counts.0, removed = counts.1).into_owned()))
            });
        let note = |selector: &'static str, text: String, color: gpui::Rgba| {
            div().debug_selector(move || selector.into()).flex_1().p(px(16.0)).text_size(px(12.0)).text_color(color).child(text).into_any_element()
        };
        let body = match (&status, diff) {
            (CompareStatus::Ready, Some(diff)) => div()
                .debug_selector(|| "compare-diff".into())
                .flex_1()
                .min_h_0()
                .font_family("Menlo")
                .text_size(px(12.0))
                .child(Diff::new(&diff).header_visible(false).h_full())
                .into_any_element(),
            (CompareStatus::Ready, None) => note("compare-identical", t!("sidebar.compare_identical").into_owned(), theme::text_muted()),
            (CompareStatus::Failed(error), _) => note("compare-error", error.clone(), theme::danger()),
            _ => note("compare-loading", t!("sidebar.compare_loading").into_owned(), theme::text_muted()),
        };
        div()
            .id(("compare-tab", tab_id))
            .debug_selector(|| "compare-tab".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .child(Self::properties_toolbar(t!("sidebar.compare_title").into_owned(), div().flex_1()).child(actions))
            .child(legend)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_patch_of_two_definitions_marks_what_changed_and_nothing_when_they_are_the_same() {
        let left = "CREATE TABLE t (\n  id integer,\n  name text\n);";
        let right = "CREATE TABLE t (\n  id bigint,\n  name text,\n  born date\n);\n";
        let patch = unified_patch(left, right).expect("they differ");
        assert!(patch.starts_with("--- a/left.sql\n+++ b/right.sql\n@@"), "{patch}");
        assert!(patch.contains("\n-  id integer,\n") && patch.contains("\n+  id bigint,\n") && patch.contains("\n+  born date\n"), "{patch}");
        // A line that only differs at the end of the text is no change, nor is the line ending style.
        assert_eq!(unified_patch("a\nb", "a\r\nb\n"), None);
        assert_eq!(unified_patch(left, left), None);
        // The component reads what is made.
        let files = DiffFile::parse(&patch).expect("a patch the view can parse");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].additions(), files[0].deletions()), (3, 2));
    }
}
