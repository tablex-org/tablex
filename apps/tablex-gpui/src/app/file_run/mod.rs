//! Run a large SQL file: the navigator's and the editor's "Run SQL File…" open a tab that streams a `.sql` file
//! through the statement splitter (never loaded into the editor or any text widget) and runs its statements one by
//! one on a session of its own, with the transaction mode and the error policy the user chose, progress, cancel, and a
//! summary with the errors and the statement to start again at.
//!
//! * `run`: the run itself (statements, transactions, errors, resume), generic over the executor;
//! * `executor`: the driver session the statements go through;
//! * `view`, `panels`: the wizard of the tab;
//! * `file_drop`: `.sql` files dragged in from the OS open in the editor (the wizard when too big);
//! * `tab`: opening the tab, the gates (read-only, Production), the worker thread and what follows a run.
//!
//! The file is cut into statements by `crate::sql_script`, which knows nothing of the app.

mod executor;
pub(in crate::app) mod file_drop;
pub(in crate::app) mod head;
mod panels;
pub(in crate::app) mod run;
mod tab;
pub(in crate::app) mod view;

#[cfg(test)]
pub(in crate::app) use executor::search_path_sql;
pub(in crate::app) use tab::{FileRunInit, FileRunViews};
