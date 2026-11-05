//! Undo / redo of edits (no GPUI). An edit is stored as before / after values, so undoing is
//! applying the other side and nothing needs a command object per kind of edit. The history is
//! bounded. It is generic over the edit type: the diagram records its moves and annotation edits
//! next to the draft's, so one undo order covers all of them.

use std::collections::VecDeque;

/// Most steps kept; the oldest is dropped first.
pub(in crate::app) const HISTORY_LIMIT: usize = 100;

#[derive(Debug)]
pub(in crate::app) struct History<E> {
    undo: VecDeque<E>,
    redo: Vec<E>,
}

impl<E> Default for History<E> {
    fn default() -> Self {
        Self { undo: VecDeque::new(), redo: Vec::new() }
    }
}

impl<E> History<E> {
    /// Records a new edit; whatever could be redone is gone.
    pub(in crate::app) fn push(&mut self, edit: E) {
        self.redo.clear();
        if self.undo.len() == HISTORY_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(edit);
    }

    pub(in crate::app) fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub(in crate::app) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub(in crate::app) fn undo_len(&self) -> usize {
        self.undo.len()
    }

    /// The edit to undo, if any; hand it to [`Self::keep_undone`] once undone.
    pub(in crate::app) fn take_undo(&mut self) -> Option<E> {
        self.undo.pop_back()
    }

    /// Remembers an edit that was just undone so it can be redone.
    pub(in crate::app) fn keep_undone(&mut self, edit: E) {
        self.redo.push(edit);
    }

    /// The edit to redo, if any; hand it to [`Self::keep_done`] once redone.
    pub(in crate::app) fn take_redo(&mut self) -> Option<E> {
        self.redo.pop()
    }

    /// Remembers an edit that was just redone so it can be undone again.
    pub(in crate::app) fn keep_done(&mut self, edit: E) {
        self.undo.push_back(edit);
    }

    /// Keeps the undo and redo steps `keep` accepts.
    pub(in crate::app) fn retain(&mut self, keep: impl Fn(&E) -> bool) {
        self.undo.retain(|edit| keep(edit));
        self.redo.retain(|edit| keep(edit));
    }

    pub(in crate::app) fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}
