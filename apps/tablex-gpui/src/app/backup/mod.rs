//! Backup and restore with the server's native tools (R6): "Backup…" and "Restore…" in the navigator's menus, the File menu and
//! the command palette open a tab that runs `pg_dump` / `pg_restore` (PostgreSQL) or `mysqldump` / `mariadb-dump` (MySQL, MariaDB)
//! as a separate process, with progress, a capped log, cancel and a summary.
//!
//! * `view`, `panels`: the wizard of the tab (Source, Settings, Run);
//! * `model`: its options and lists;
//! * `tab`: opening the tab, the gates (read-only, Production), the worker thread of the run and what follows it.
//!
//! The tools, their commands, the private files for the password and the process itself are in `crate::backup`, which knows
//! nothing of windows.

mod model;
mod panels;
mod tab;
pub(in crate::app) mod view;

pub(in crate::app) use model::Mode;
pub(in crate::app) use tab::BackupViews;
#[cfg(test)]
pub(in crate::app) use view::BackupInit;
