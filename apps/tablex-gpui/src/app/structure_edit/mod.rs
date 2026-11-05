//! The structure editor's core, shared by every place that edits a table's structure (the
//! diagram and the table Properties tab).
//!
//! - [`EditSession`]: the pending change-set (a [`Draft`](crate::sql_ddl::draft::Draft)), its
//!   undo history, staleness and the review dialog's progress. Plain state, owned by the host.
//! - [`EditContext`]: what AppRoot allows (scope, confirmations, read-only reason, `sql_mode`).
//! - `dialog`: the review dialog (SQL, warnings, acknowledgements, Apply / Cancel), built for
//!   any [`ReviewHost`].
//! - `apply`: the round trip on a dedicated driver session, keyed by [`ApplyTarget`].

pub(in crate::app) mod apply;
#[cfg(test)]
pub(in crate::app) mod bench;
mod context;
mod dialog;
mod history;
pub(in crate::app) mod live;
mod session;
pub(in crate::app) mod specs;

pub(in crate::app) use apply::ApplyTarget;
pub(in crate::app) use context::{ApplyRequest, EditContext, EditScope};
pub(in crate::app) use dialog::{close_dialog_later, open_review, ReviewHost, ReviewOption};
#[cfg(test)]
pub(in crate::app) use dialog::{MIN_CARD_HEIGHT, VIRTUAL_FROM};
pub(in crate::app) use history::History;
#[cfg(test)]
pub(in crate::app) use history::HISTORY_LIMIT;
pub(in crate::app) use session::{DraftEdit, EditSession, NotSent, SessionEdit, Snapshot, Wording};
