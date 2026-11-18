//! SQL statements of a Mongo script tab painted as SQL. The editor stays "javascript"; the
//! statements `mongo_sql` rewrites are covered by one text-decoration collection per tab
//! (`mongo_sql_highlight` computes the runs). Recomputed 150 ms after the last edit and when the
//! theme changes; a script with no SQL statement costs one scan for the word `select`. The scan, the
//! masked copy and the SQL parse run on the background executor on a clone of the editor's rope
//! (O(1)); the result is applied only if no edit or refresh came after it started.
use super::*;
use gpui_component::input::{TextDecoration, TextDecorationCollection};

const DEBOUNCE: Duration = Duration::from_millis(150);

pub(super) struct SqlHighlight {
    pub(super) collection: TextDecorationCollection,
    /// The recompute waiting for the debounce; replacing it cancels the previous one.
    pending: Option<gpui::Task<()>>,
    /// Bumped by every edit and every refresh: a result is applied only while it is the latest.
    pub(super) generation: u64,
}

impl AppRoot {
    /// Starts the highlighting of a Mongo script tab's editor (called once, on creation).
    pub(super) fn start_sql_highlight(
        &mut self,
        tab_id: u64,
        editor: &gpui::Entity<EditorState>,
        cx: &mut Context<Self>,
    ) {
        let collection = editor.update(cx, |state, cx| state.create_decorations_collection(Vec::new(), cx));
        self.query_editors.sql_highlight.insert(tab_id, SqlHighlight { collection, pending: None, generation: 0 });
        self.refresh_sql_highlight(tab_id, cx);
    }

    /// Recomputes the highlighting once the typing pauses.
    pub(super) fn schedule_sql_highlight(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(highlight) = self.query_editors.sql_highlight.get_mut(&tab_id) else {
            return;
        };
        highlight.generation += 1;
        highlight.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| this.refresh_sql_highlight(tab_id, cx));
        }));
    }

    pub(super) fn refresh_all_sql_highlights(&mut self, cx: &mut Context<Self>) {
        let tabs: Vec<u64> = self.query_editors.sql_highlight.keys().copied().collect();
        for tab_id in tabs {
            self.refresh_sql_highlight(tab_id, cx);
        }
    }

    fn refresh_sql_highlight(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let (Some(editor), Some(highlight)) =
            (self.query_editors.by_tab.get(&tab_id), self.query_editors.sql_highlight.get_mut(&tab_id))
        else {
            return;
        };
        highlight.generation += 1;
        let generation = highlight.generation;
        // O(1): the rope is shared, not copied; the size check needs no text.
        let rope = editor.read(cx).text().clone();
        if rope.len() > crate::mongo_sql_highlight::MAX_SCRIPT_BYTES {
            highlight.pending = None;
            highlight.collection.set(Vec::new(), cx);
            return;
        }
        let theme = gpui_component::theme::Theme::global(cx);
        let (highlight_theme, foreground) = (theme.highlight_theme.clone(), theme.foreground);
        let runs = cx.background_executor().spawn(async move {
            crate::mongo_sql_highlight::sql_runs(&rope.to_string(), highlight_theme.as_ref(), foreground)
        });
        highlight.pending = Some(cx.spawn(async move |this, cx| {
            let runs = runs.await;
            let _ = this.update(cx, |this, cx| this.apply_sql_highlight(tab_id, generation, runs, cx));
        }));
    }

    /// Paints `runs` unless the tab is gone or an edit / newer refresh came since they were asked for.
    pub(super) fn apply_sql_highlight(
        &mut self,
        tab_id: u64,
        generation: u64,
        runs: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)>,
        cx: &mut Context<Self>,
    ) {
        let Some(highlight) = self.query_editors.sql_highlight.get_mut(&tab_id) else {
            return;
        };
        if highlight.generation != generation || !self.query_editors.by_tab.contains_key(&tab_id) {
            return;
        }
        highlight.pending = None;
        highlight.collection.set(
            runs.into_iter().map(|(range, style)| TextDecoration::new(range, style)).collect(),
            cx,
        );
    }
}
