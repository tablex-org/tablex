//! Import data into a table (R4a): the navigator's "Import Data…" opens a tab that reads a CSV / TSV / JSON file
//! (streamed, never held whole), shows its first rows in the app's result grid, maps its columns onto the
//! columns of an existing table or onto a new one, and loads the rows in batches through the driver's own
//! `batch` insert on a session of its own.
//!
//! * `convert`: a cell of the file as the value of its target column (the cell editor's own number and
//!   boolean rules, plus the file's spelling hints);
//! * `mapping`: auto-mapping by name, the notes the preview finds, the proposed new table;
//! * `run`: the load itself (batches, error policy, transaction mode, progress, cancel), generic over a writer;
//! * `writer`: the driver session the batches go through;
//! * `view`, `panels`: the wizard of the tab;
//! * `tab`: opening the tab, the gates (read-only, Production), the worker thread and what follows a load.
//!
//! The file is read by `crate::import`, which knows nothing of the app.

pub(in crate::app) mod convert;
pub(in crate::app) mod mapping;
mod panels;
pub(in crate::app) mod run;
mod tab;
pub(in crate::app) mod view;
pub(in crate::app) mod writer;

pub(in crate::app) use tab::{Gate, ImportCreate, ImportViews};
pub(in crate::app) use writer::{first_cell, message_of, DriverWriter, TargetTable};
