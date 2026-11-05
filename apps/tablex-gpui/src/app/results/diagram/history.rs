//! Undo / redo of diagram edits (no GPUI). An edit is either a set of entity
//! moves or a change of the [`Annotations`]; both are stored as before / after
//! values, so undoing is applying the other side and nothing needs a command
//! object per kind of edit. The history itself is `structure_edit::History`, which the diagram
//! shares with every other structure editor: the draft's steps are in it too.

use std::sync::Arc;

use super::annotations::{Annotations, TableKey};
use super::layout::DiagramPoint;
use crate::app::structure_edit::{SessionEdit, Snapshot};

#[cfg(test)]
pub(in crate::app) use crate::app::structure_edit::HISTORY_LIMIT;

/// The diagram's undo history.
pub(in crate::app) type History = crate::app::structure_edit::History<Edit>;

/// One entity that moved (a drag, a group move, Re-layout).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct MoveStep {
    pub(in crate::app) table: TableKey,
    pub(in crate::app) from: DiagramPoint,
    pub(in crate::app) to: DiagramPoint,
}

/// One undoable step.
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) enum Edit {
    Move(Vec<MoveStep>),
    Annotations { before: Arc<Annotations>, after: Arc<Annotations> },
    /// D3: the pending structure changes (not the DDL that was applied).
    Draft { before: Snapshot, after: Snapshot },
}

impl SessionEdit for Edit {
    fn of_draft(before: Snapshot, after: Snapshot) -> Self {
        Self::Draft { before, after }
    }

    fn is_draft(&self) -> bool {
        matches!(self, Self::Draft { .. })
    }
}
