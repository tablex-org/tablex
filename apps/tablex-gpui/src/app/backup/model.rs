//! What the wizard holds apart from its widgets: the options as flags, the checklists and the states of what is read off
//! the UI thread.

use std::collections::HashSet;

use gpui::SharedString;

use crate::backup::toc::ArchiveKind;
use crate::backup::tools::Found;
use crate::connections::ConnectionKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Mode {
    Backup,
    Restore,
}

impl Mode {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Backup => "Backup",
            Self::Restore => "Restore",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Source,
    Settings,
    Run,
}

/// What of the database a backup holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    Whole,
    Schemas,
    Tables,
}

impl Scope {
    /// The choices of a `kind` server, in the order of the select.
    pub(super) fn choices(kind: ConnectionKind) -> &'static [Self] {
        if kind == ConnectionKind::Postgres {
            &[Self::Whole, Self::Schemas, Self::Tables]
        } else {
            &[Self::Whole, Self::Tables]
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Whole => "The whole database",
            Self::Schemas => "Selected schemas",
            Self::Tables => "Selected tables",
        }
    }
}

/// A yes/no option. The same list serves every kind of server and both directions; `Mode` and the format decide which
/// ones are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Flag {
    NoOwner,
    NoPrivileges,
    Clean,
    Create,
    SingleTransaction,
    Routines,
    Triggers,
    Events,
    NoData,
    AddDropTable,
    ExtendedInsert,
    CreateDatabase,
    AddDropDatabase,
    RestoreClean,
    RestoreNoOwner,
    RestoreSingleTransaction,
}

impl Flag {
    pub(super) const ALL: [Self; 16] = [
        Self::NoOwner,
        Self::NoPrivileges,
        Self::Clean,
        Self::Create,
        Self::SingleTransaction,
        Self::Routines,
        Self::Triggers,
        Self::Events,
        Self::NoData,
        Self::AddDropTable,
        Self::ExtendedInsert,
        Self::CreateDatabase,
        Self::AddDropDatabase,
        Self::RestoreClean,
        Self::RestoreNoOwner,
        Self::RestoreSingleTransaction,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::NoOwner => "Do not set owners (--no-owner)",
            Self::NoPrivileges => "Do not dump privileges (--no-privileges)",
            Self::Clean => "Drop objects first (--clean --if-exists)",
            Self::Create => "Include CREATE DATABASE (--create)",
            Self::SingleTransaction => "One consistent transaction (--single-transaction)",
            Self::Routines => "Stored procedures and functions (--routines)",
            Self::Triggers => "Triggers (--triggers)",
            Self::Events => "Events (--events)",
            Self::NoData => "Structure only, no rows (--no-data)",
            Self::AddDropTable => "DROP TABLE before each CREATE (--add-drop-table)",
            Self::ExtendedInsert => "Several rows per INSERT (--extended-insert)",
            Self::CreateDatabase => "Include CREATE DATABASE and USE (--databases)",
            Self::AddDropDatabase => "DROP DATABASE first (--add-drop-database)",
            Self::RestoreClean => "Drop objects before restoring them (--clean --if-exists)",
            Self::RestoreNoOwner => "Do not set owners (--no-owner)",
            Self::RestoreSingleTransaction => "All or nothing: one transaction (--single-transaction)",
        }
    }

    pub(super) fn selector(self) -> &'static str {
        match self {
            Self::NoOwner => "backup-flag-no-owner",
            Self::NoPrivileges => "backup-flag-no-privileges",
            Self::Clean => "backup-flag-clean",
            Self::Create => "backup-flag-create",
            Self::SingleTransaction => "backup-flag-single-transaction",
            Self::Routines => "backup-flag-routines",
            Self::Triggers => "backup-flag-triggers",
            Self::Events => "backup-flag-events",
            Self::NoData => "backup-flag-no-data",
            Self::AddDropTable => "backup-flag-add-drop-table",
            Self::ExtendedInsert => "backup-flag-extended-insert",
            Self::CreateDatabase => "backup-flag-create-database",
            Self::AddDropDatabase => "backup-flag-add-drop-database",
            Self::RestoreClean => "backup-flag-restore-clean",
            Self::RestoreNoOwner => "backup-flag-restore-no-owner",
            Self::RestoreSingleTransaction => "backup-flag-restore-single-transaction",
        }
    }

    /// Whether it is on when the wizard opens.
    pub(super) fn default_on(self) -> bool {
        matches!(self, Self::SingleTransaction | Self::Routines | Self::Triggers | Self::Events | Self::AddDropTable | Self::ExtendedInsert | Self::CreateDatabase)
    }

    pub(super) fn defaults() -> HashSet<Self> {
        Self::ALL.into_iter().filter(|flag| flag.default_on()).collect()
    }
}

/// One row of a checklist.
#[derive(Debug, Clone)]
pub(super) struct CheckItem {
    pub(super) label: SharedString,
    /// The archive's number of a table-of-contents entry; the position for the others.
    pub(super) id: u32,
    pub(super) selected: bool,
}

/// A list of things to choose from, with the filter applied: only the rows shown are drawn.
#[derive(Debug, Default)]
pub(super) struct Checklist {
    pub(super) items: Vec<CheckItem>,
    /// Indices into `items` that match the filter.
    pub(super) shown: Vec<usize>,
}

impl Checklist {
    pub(super) fn replace(&mut self, items: Vec<CheckItem>, needle: &str) {
        self.items = items;
        self.refilter(needle);
    }

    pub(super) fn refilter(&mut self, needle: &str) {
        let needle = needle.trim().to_lowercase();
        self.shown = self.items.iter().enumerate().filter(|(_, item)| needle.is_empty() || item.label.to_lowercase().contains(&needle)).map(|(index, _)| index).collect();
    }

    pub(super) fn set(&mut self, index: usize, selected: bool) {
        if let Some(item) = self.items.get_mut(index) {
            item.selected = selected;
        }
    }

    /// Selects or clears the rows the filter shows.
    pub(super) fn set_shown(&mut self, selected: bool) {
        for index in &self.shown {
            self.items[*index].selected = selected;
        }
    }

    pub(super) fn selected(&self) -> usize {
        self.items.iter().filter(|item| item.selected).count()
    }

    pub(super) fn selected_ids(&self) -> HashSet<u32> {
        self.items.iter().filter(|item| item.selected).map(|item| item.id).collect()
    }

    pub(super) fn selected_labels(&self) -> Vec<&str> {
        self.items.iter().filter(|item| item.selected).map(|item| item.label.as_ref()).collect()
    }
}

/// A list read off the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Loading {
    Idle,
    Pending,
    Ready,
    Failed(String),
}

/// What is known of the file chosen to restore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ArchiveState {
    Detecting,
    Ready(ArchiveKind),
    Failed(String),
}

/// The tool a run would use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ToolState {
    Unknown,
    Checking,
    Found(Found),
    Failed(String),
}
