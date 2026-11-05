//! The diagram's side of a structure apply: AppRoot hands the review dialog's Apply / Cancel to
//! the shared round trip (`structure_edit::apply`), tells the view how it ended and reloads it.

use gpui::Context;

use super::view::{DiagramEvent, DiagramKind};
use crate::app::structure_edit::ApplyTarget;
use crate::app::AppRoot;

impl AppRoot {
    /// The `DiagramEvent`s that need AppRoot (the subscription in `render_diagram_kind`).
    pub(in crate::app) fn handle_diagram_apply(&mut self, tab_id: u64, kind: DiagramKind, event: &DiagramEvent, cx: &mut Context<Self>) {
        let target = ApplyTarget::Diagram { tab_id, kind };
        match event {
            DiagramEvent::Apply(request) => self.apply_structure_changes(target, (**request).clone(), cx),
            DiagramEvent::CancelApply => self.cancel_structure_apply(target, cx),
            _ => {}
        }
    }

    /// How an apply asked from the diagram on `tab_id` ended.
    pub(in crate::app) fn diagram_apply_answer(
        &mut self,
        tab_id: u64,
        done: &[usize],
        partial: &[usize],
        report: String,
        failed: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(view) = self.diagram_views.get(&tab_id).cloned() {
            view.update(cx, |view, cx| view.apply_finished(done, partial, report, failed, cx));
        }
    }

    /// The catalog changed: the diagram on `tab_id` reloads.
    pub(in crate::app) fn refresh_diagram_after_apply(&mut self, tab_id: u64, kind: DiagramKind, cx: &mut Context<Self>) {
        match kind {
            DiagramKind::Table => self.refresh_diagram(tab_id, cx),
            DiagramKind::Schema => self.refresh_schema_diagram(tab_id, cx),
        }
    }
}
