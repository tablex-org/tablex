//! A pending change-set against what the database holds (no GPUI, no I/O).
//!
//! Two questions, answered from a [`LiveSchema`] (what the catalog says about
//! the tables a change-set touches):
//!
//! * [`stale`]: does every change still start from the structure it was made
//!   from? A draft is made against a catalog; when the table changed since
//!   (a column widened by someone else, an index dropped) the change is *stale*
//!   and is never applied: the user refreshes and decides, nothing is silently
//!   rebased.
//! * [`already_applied`]: which of the changes an interrupted apply was
//!   running are visible in the catalog (so they are not applied twice).

use std::collections::HashMap;

use super::{Change, ColumnSpec, NameRules, PrimaryKeyRef, TableId};

/// One table as the catalog describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LiveTable {
    pub columns: Vec<ColumnSpec>,
    pub primary_key: Option<PrimaryKeyRef>,
    pub indexes: Vec<String>,
    pub foreign_keys: Vec<String>,
}

impl LiveTable {
    fn column(&self, name: &str) -> Option<&ColumnSpec> {
        self.columns.iter().find(|column| column.name == name)
    }
}

/// What is known about one table.
#[derive(Debug, Clone, Copy)]
pub enum Lookup<'a> {
    Present(&'a LiveTable),
    /// Described and not there.
    Absent,
    /// Not described (not part of the diagram): no conclusion either way.
    Unknown,
}

#[derive(Debug, Clone, Default)]
pub struct LiveSchema {
    tables: HashMap<TableId, Option<LiveTable>>,
    /// Every table of the namespace is in `tables`: a missing one is absent.
    complete: bool,
}

impl LiveSchema {
    pub fn new(complete: bool) -> Self {
        Self { tables: HashMap::new(), complete }
    }

    pub fn present(&mut self, table: TableId, live: LiveTable) {
        self.tables.insert(table, Some(live));
    }

    pub fn absent(&mut self, table: TableId) {
        self.tables.insert(table, None);
    }

    pub fn get(&self, table: &TableId) -> Lookup<'_> {
        match self.tables.get(table) {
            Some(Some(live)) => Lookup::Present(live),
            Some(None) => Lookup::Absent,
            None if self.complete => Lookup::Absent,
            None => Lookup::Unknown,
        }
    }
}

/// The name `table` has in the database before the change-set's renames.
pub fn original_table(changes: &[Change], table: &TableId) -> TableId {
    changes
        .iter()
        .find_map(|change| match change {
            Change::RenameTable { table: original, to } if original.schema == table.schema && to == &table.name => {
                Some(original.clone())
            }
            _ => None,
        })
        .unwrap_or_else(|| table.clone())
}

/// The name column `name` of `table` has in the database before the change-set's renames.
pub fn original_column(changes: &[Change], table: &TableId, name: &str) -> String {
    changes
        .iter()
        .find_map(|change| match change {
            Change::RenameColumn { table: t, from, to } if t == table && to == name => Some(from.clone()),
            _ => None,
        })
        .unwrap_or_else(|| name.to_owned())
}

const REFRESH: &str = "schema changed since this draft was made: refresh the diagram";

/// What a replayed change found wrong with the catalog it starts from.
struct Finding {
    /// Why the change is stale.
    reason: String,
    /// The change takes a name that is already used (`stale` calls it stale, [`name_clashes`] refuses it).
    clash: Option<String>,
}

impl Finding {
    fn gone(reason: String) -> Self {
        Self { reason, clash: None }
    }

    fn taken(reason: String, message: String) -> Self {
        Self { reason, clash: Some(message) }
    }
}

/// The tables as the change-set evolves them, replayed in execution order
/// ([`super::plan`]'s phase order): a name an earlier change frees (a rename, a
/// drop) can be taken by a later one, an index or foreign key that is dropped
/// can be created again under its name, and a column the primary key makes NOT
/// NULL is NOT NULL for the changes after it.
struct Replay<'a> {
    base: &'a LiveSchema,
    /// The tables the replay has changed; every other one is as `base` has it (a table is copied
    /// from `base` when a change first touches it, so a catalog of many tables is not copied).
    tables: HashMap<TableId, Option<LiveTable>>,
}

impl<'a> Replay<'a> {
    fn new(base: &'a LiveSchema) -> Self {
        Self { base, tables: HashMap::new() }
    }

    fn get(&self, table: &TableId) -> Lookup<'_> {
        match self.tables.get(table).or_else(|| self.base.tables.get(table)) {
            Some(Some(live)) => Lookup::Present(live),
            Some(None) => Lookup::Absent,
            None if self.base.complete => Lookup::Absent,
            None => Lookup::Unknown,
        }
    }

    /// The table when the replay knows it (a table nothing is known about decides nothing).
    fn present_mut(&mut self, table: &TableId) -> Option<&mut LiveTable> {
        if !self.tables.contains_key(table) {
            let known = self.base.tables.get(table)?.clone();
            self.tables.insert(table.clone(), known);
        }
        self.tables.get_mut(table).and_then(Option::as_mut)
    }

    /// Takes the table out of the replay (it is absent from here on); what it held, when it was there.
    fn take(&mut self, table: &TableId) -> Option<LiveTable> {
        let held = match self.tables.remove(table) {
            Some(held) => held,
            None => self.base.tables.get(table).cloned().flatten(),
        };
        self.tables.insert(table.clone(), None);
        held
    }
}

/// The changes that no longer start from what the catalog says, with why
/// (`(index into changes, reason)`).
pub fn stale(changes: &[Change], live: &LiveSchema) -> Vec<(usize, String)> {
    let mut replay = Replay::new(live);
    // Columns a pending primary key makes NOT NULL (a draft made from the overlay already shows them so).
    let keyed: Vec<(&TableId, &String)> = changes
        .iter()
        .filter_map(|change| match change {
            Change::SetPrimaryKey { table, columns, .. } => Some(columns.iter().map(move |column| (table, column))),
            _ => None,
        })
        .flatten()
        .collect();
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&index| changes[index].phase());
    let mut found = Vec::new();
    for index in order {
        if let Some(finding) = step(&mut replay, &keyed, &changes[index], NameRules::EXACT) {
            found.push((index, format!("{REFRESH} ({})", finding.reason)));
        }
    }
    found.sort_by_key(|(index, _)| *index);
    found
}

/// The changes that take a column, index or foreign key name the table already uses (in the
/// catalog, or through an earlier change of the set), with the message
/// (`(index into changes, message)`). Tables `live` does not describe are checked against the
/// names the change-set itself uses, so a set made without a catalog still cannot repeat a name.
pub fn name_clashes(changes: &[Change], live: &LiveSchema, rules: NameRules) -> Vec<(usize, String)> {
    let mut replay = Replay::new(live);
    let created: Vec<&TableId> = changes.iter().filter(|c| matches!(c, Change::CreateTable { .. })).filter_map(Change::table).collect();
    for change in changes {
        let Some(table) = change.table() else { continue };
        if change.is_whole_table()
            || created.contains(&table)
            || !matches!(replay.get(table), Lookup::Unknown)
        {
            continue;
        }
        replay.tables.insert(table.clone(), Some(known_to_exist(changes, table)));
    }
    let mut order: Vec<usize> = (0..changes.len()).collect();
    order.sort_by_key(|&index| changes[index].phase());
    let mut found: Vec<(usize, String)> = order
        .into_iter()
        .filter_map(|index| step(&mut replay, &[], &changes[index], rules).and_then(|finding| finding.clash).map(|message| (index, message)))
        .collect();
    found.sort_by_key(|(index, _)| *index);
    found
}

/// What the change-set itself says `table` has: the columns, indexes and foreign keys its
/// changes start from, under the names the database has them (a rename's `from`, a drop's
/// column, an alteration's column before the renames; never a name a rename gives).
fn known_to_exist(changes: &[Change], table: &TableId) -> LiveTable {
    let mut known = LiveTable::default();
    let mut column = |name: &str| {
        if !known.columns.iter().any(|c| c.name == name) {
            known.columns.push(ColumnSpec { name: name.to_owned(), ..ColumnSpec::default() });
        }
    };
    let mut indexes = Vec::new();
    let mut foreign_keys = Vec::new();
    for change in changes.iter().filter(|change| change.table() == Some(table)) {
        match change {
            Change::RenameColumn { from, .. } => column(from),
            Change::DropColumn { column: name, .. } => column(name),
            // The spec carries the name the column has when the alteration runs (after the set's
            // renames); it exists in the database under the name before them.
            Change::AlterColumn { from, .. } => column(&original_column(changes, table, &from.name)),
            Change::DropIndex { name, .. } => indexes.push(name.clone()),
            Change::DropForeignKey { name, .. } => foreign_keys.push(name.clone()),
            _ => {}
        }
    }
    known.indexes = indexes;
    known.foreign_keys = foreign_keys;
    known
}

/// Checks one change against the replayed catalog and applies its effect to it.
fn step(replay: &mut Replay<'_>, keyed: &[(&TableId, &String)], change: &Change, rules: NameRules) -> Option<Finding> {
    match change {
        Change::CreateTable { table, columns, primary_key, .. } => {
            let reason = matches!(replay.get(table), Lookup::Present(_)).then(|| format!("a table named {table} exists now"));
            let primary_key = (!primary_key.is_empty()).then(|| PrimaryKeyRef { name: None, columns: primary_key.clone() });
            replay.tables.insert(table.clone(), Some(LiveTable { columns: columns.clone(), primary_key, ..LiveTable::default() }));
            reason.map(Finding::gone)
        }
        Change::DropTable { table, .. } | Change::DropView { table, .. } => {
            let reason = matches!(replay.get(table), Lookup::Absent).then(|| format!("table {table} no longer exists"));
            replay.tables.insert(table.clone(), None);
            reason.map(Finding::gone)
        }
        Change::RenameTable { table: from, to } | Change::RenameView { table: from, to, .. } => {
            let target = TableId { schema: from.schema.clone(), name: to.clone() };
            let reason = match (replay.get(from), replay.get(&target)) {
                (Lookup::Absent, _) => Some(format!("table {from} no longer exists")),
                (_, Lookup::Present(_)) => Some(format!("a table named {to} exists now")),
                _ => None,
            };
            if let Some(moved) = replay.take(from) {
                replay.tables.insert(target, Some(moved));
            }
            reason.map(Finding::gone)
        }
        // A namespace is not a table: nothing of the replayed catalog decides it. Nor is an account.
        Change::CreateNamespace { .. } | Change::Account(_) => None,
        other => {
            let table = other.table()?;
            match replay.get(table) {
                Lookup::Absent => return Some(Finding::gone(format!("table {table} no longer exists"))),
                Lookup::Unknown => return None,
                Lookup::Present(_) => {}
            }
            let live = replay.present_mut(table)?;
            table_step(live, keyed, other, rules)
        }
    }
}

fn table_step(live: &mut LiveTable, keyed: &[(&TableId, &String)], change: &Change, rules: NameRules) -> Option<Finding> {
    let table = change.table()?;
    match change {
        Change::AddColumn { column, .. } => {
            let finding = live.columns.iter().find(|c| rules.same(&c.name, &column.name)).map(|_| {
                Finding::taken(format!("column {} exists now", column.name), format!("A column named \"{}\" already exists in {table}.", column.name))
            });
            live.columns.push(column.clone());
            finding
        }
        Change::DropColumn { column, .. } => {
            let reason = live.column(column).is_none().then(|| format!("column {column} no longer exists"));
            live.columns.retain(|c| &c.name != column);
            reason.map(Finding::gone)
        }
        Change::RenameColumn { from, to, .. } => {
            // Another column of that name (the column itself may change its case).
            let taken = live
                .columns
                .iter()
                .any(|c| c.name != *from && rules.same(&c.name, to))
                .then(|| format!("A column named \"{to}\" already exists in {table}."));
            // A rename whose source is gone is reported as that, not as a clash.
            let finding = if live.column(from).is_none() {
                Some(Finding::gone(format!("column {from} no longer exists")))
            } else {
                taken.map(|message| Finding::taken(format!("column {to} exists now"), message))
            };
            for column in live.columns.iter_mut().filter(|c| &c.name == from) {
                column.name = to.clone();
            }
            if let Some(key) = live.primary_key.as_mut() {
                key.columns.iter_mut().filter(|c| *c == from).for_each(|c| *c = to.clone());
            }
            finding
        }
        Change::AlterColumn { table, from, to } => {
            let reason = match live.column(&from.name) {
                None => Some(format!("column {} no longer exists", from.name)),
                Some(now) => {
                    let mut now = without_name(now);
                    let in_live_key = live.primary_key.as_ref().is_some_and(|key| key.columns.contains(&from.name));
                    // The draft was made with the primary key it adds already showing the column as NOT NULL.
                    if keyed.iter().any(|(t, c)| *t == table && **c == from.name) && !in_live_key && !from.nullable {
                        now.nullable = false;
                    }
                    (now != without_name(from)).then(|| {
                        format!("column {} is {} now, the draft started from {}", from.name, describe(&now), describe(from))
                    })
                }
            };
            if let Some(column) = live.columns.iter_mut().find(|c| c.name == from.name) {
                *column = ColumnSpec { name: to.name.clone(), ..to.clone() };
            }
            reason.map(Finding::gone)
        }
        Change::SetPrimaryKey { previous, columns, .. } => {
            let live_columns: &[String] = live.primary_key.as_ref().map_or(&[], |key| &key.columns);
            let expected: &[String] = previous.as_ref().map_or(&[], |key| &key.columns);
            let reason = (live_columns != expected).then(|| "the primary key is not what the draft started from".to_owned());
            live.primary_key = (!columns.is_empty()).then(|| PrimaryKeyRef { name: previous.as_ref().and_then(|k| k.name.clone()), columns: columns.clone() });
            for column in live.columns.iter_mut().filter(|c| columns.contains(&c.name)) {
                column.nullable = false;
            }
            reason.map(Finding::gone)
        }
        Change::CreateIndex { index, .. } => {
            let finding = live.indexes.iter().any(|name| rules.same(name, &index.name)).then(|| {
                Finding::taken(format!("index {} exists now", index.name), format!("An index named \"{}\" already exists on {table}.", index.name))
            });
            live.indexes.push(index.name.clone());
            finding
        }
        Change::DropIndex { name, .. } => {
            let reason = (!live.indexes.contains(name)).then(|| format!("index {name} no longer exists"));
            live.indexes.retain(|index| index != name);
            reason.map(Finding::gone)
        }
        Change::AddForeignKey { foreign_key, .. } => {
            let finding = live.foreign_keys.iter().any(|name| rules.same(name, &foreign_key.name)).then(|| {
                Finding::taken(
                    format!("foreign key {} exists now", foreign_key.name),
                    format!("A foreign key named \"{}\" already exists on {table}.", foreign_key.name),
                )
            });
            live.foreign_keys.push(foreign_key.name.clone());
            finding
        }
        Change::DropForeignKey { name, .. } => {
            let reason = (!live.foreign_keys.contains(name)).then(|| format!("foreign key {name} no longer exists"));
            live.foreign_keys.retain(|key| key != name);
            reason.map(Finding::gone)
        }
        // Truncating changes no structure: the table has to be there, which `step` checked.
        Change::TruncateTable { .. } => None,
        Change::CreateTable { .. }
        | Change::DropTable { .. }
        | Change::RenameTable { .. }
        | Change::DropView { .. }
        | Change::RenameView { .. }
        | Change::CreateNamespace { .. }
        | Change::Account(_) => None,
    }
}

fn without_name(spec: &ColumnSpec) -> ColumnSpec {
    ColumnSpec { name: String::new(), ..spec.clone() }
}

fn describe(spec: &ColumnSpec) -> String {
    format!(
        "{} {}{}",
        spec.data_type,
        if spec.nullable { "NULL" } else { "NOT NULL" },
        spec.default.as_deref().map_or(String::new(), |default| format!(" DEFAULT {default}"))
    )
}

/// The changes whose result is already in the catalog (an apply that was
/// interrupted ran them). Only positive evidence counts; a table the catalog
/// knows nothing about decides nothing.
pub fn already_applied(changes: &[Change], live: &LiveSchema) -> Vec<usize> {
    changes
        .iter()
        .enumerate()
        .filter(|(_, change)| applied(changes, change, live))
        .map(|(index, _)| index)
        .collect()
}

fn applied(changes: &[Change], change: &Change, live: &LiveSchema) -> bool {
    // Nothing in a catalog of tables says a namespace exists.
    let Some(table) = change.table() else { return false };
    // After a table rename the table has its new name; before it, its old one.
    let present = |name: &TableId| match live.get(name) {
        Lookup::Present(table) => Some(table),
        _ => None,
    };
    let current = || present(table).or_else(|| present(&original_table(changes, table)));
    match change {
        Change::CreateTable { table, .. } => present(table).is_some(),
        Change::DropTable { table, .. } | Change::DropView { table, .. } => matches!(live.get(table), Lookup::Absent),
        // Rows that are gone leave no trace in a catalog of structures.
        Change::TruncateTable { .. } | Change::CreateNamespace { .. } => false,
        // A catalog of tables says nothing of the server's accounts.
        Change::Account(_) => false,
        Change::RenameTable { table: from, to } | Change::RenameView { table: from, to, .. } => {
            let target = TableId { schema: from.schema.clone(), name: to.clone() };
            matches!(live.get(from), Lookup::Absent) && present(&target).is_some()
        }
        Change::AddColumn { column, .. } => current().is_some_and(|live| live.column(&column.name).is_some()),
        Change::DropColumn { column, .. } => current().is_some_and(|live| live.column(column).is_none()),
        Change::RenameColumn { from, to, .. } => {
            current().is_some_and(|live| live.column(to).is_some() && live.column(from).is_none())
        }
        Change::AlterColumn { to, .. } => {
            current().and_then(|live| live.column(&to.name)).is_some_and(|now| without_name(now) == without_name(to))
        }
        Change::SetPrimaryKey { columns, .. } => current().is_some_and(|live| {
            live.primary_key.as_ref().map_or(columns.is_empty(), |key| &key.columns == columns)
        }),
        Change::CreateIndex { index, .. } => current().is_some_and(|live| live.indexes.contains(&index.name)),
        Change::DropIndex { name, .. } => current().is_some_and(|live| !live.indexes.contains(name)),
        Change::AddForeignKey { foreign_key, .. } => current().is_some_and(|live| live.foreign_keys.contains(&foreign_key.name)),
        Change::DropForeignKey { name, .. } => current().is_some_and(|live| !live.foreign_keys.contains(name)),
    }
}
