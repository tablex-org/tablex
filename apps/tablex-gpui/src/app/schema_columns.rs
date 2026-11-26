//! The columns of a schema's tables, asked of the driver once per schema (`metadata.schema_columns`)
//! instead of once per table, and shared by what needs a table's columns: the navigator's Columns
//! folders and SQL completion (their per-table answer is the same names and types).
//!
//! * A Columns folder whose schema is stored opens from it, with no driver call. One that is not
//!   asks for the table alone (the answer should not wait for a schema's worth of columns) and, for a
//!   schema of at most [`PREFETCH_MAX_TABLES`] tables, starts the schema request in the background
//!   so the next folders are instant. A load that completion started waits for the schema request
//!   instead (it needs several tables, and one request answers them all).
//! * A driver without the capability, a schema over the cap (`navigator::schema_columns::MAX_COLUMNS`),
//!   a failed request and a table the request did not list are all served per table, as before.
//! * Forgotten where the table properties are ([`AppRoot::invalidate_table_properties`]): a refresh
//!   of the schema, table or connection, the DDL a script ran, a structure change; a table on its own
//!   when only it is refreshed. The key holds the engine session, so a reconnect never reads the
//!   old session's columns, and a disconnect drops them.

use super::*;
use crate::navigator::schema_columns::{self, SchemaColumns, StoredColumn};
use crate::navigator::LoadedChild;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// A schema of at most this many tables is read whole in the background by the first Columns folder
/// that opens in it. Above it, nothing is read unless completion asks.
const PREFETCH_MAX_TABLES: usize = 1_000;

/// Schemas kept at a time (the least recently used one goes first): a long session over many
/// schemas does not hold all their columns.
const MAX_SCHEMAS: usize = 8;

/// A request that failed (a blip, not a verdict on the schema) is asked again after this long; until
/// then the tables are asked for one by one.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SchemaKey {
    pub(super) connection: String,
    pub(super) database: String,
    pub(super) schema: String,
    /// Engine connection id (0 = not connected): a reconnect reads again.
    pub(super) session: u64,
}

enum Slot {
    /// In flight: the loads waiting for it. `stale`: invalidated meanwhile, so the answer is not kept.
    Loading { stale: bool, waiters: Vec<(LoadRequest, u64)> },
    Ready(SchemaColumns),
    /// Not stored (over the cap): its tables are asked for one by one.
    Unavailable,
    /// The request failed at this time: like `Unavailable` until [`RETRY_AFTER`] has passed.
    Failed(std::time::Instant),
}

/// How a schema request ended.
pub(super) enum Outcome {
    Ready(SchemaColumns),
    /// Over the cap, or a driver that cannot answer it: the schema is not asked again.
    Unavailable,
    /// The request itself failed (no session, an error): asked again later.
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SchemaState {
    Absent,
    Loading,
    Ready,
    Unavailable,
}

/// A handful of schemas at a time, so a list is the whole index.
#[derive(Default)]
pub(super) struct SchemaColumnsCache {
    /// Each with the tick of its last use.
    slots: Vec<(SchemaKey, Slot, AtomicU64)>,
    clock: AtomicU64,
    /// Schema requests started (tests count them).
    #[cfg(test)]
    pub(super) fetches: usize,
    /// Columns loads that went to the driver for one table (tests count them).
    #[cfg(test)]
    pub(super) per_table_calls: usize,
}

impl SchemaColumnsCache {
    fn slot(&self, connection: &str, database: &str, schema: &str, session: u64) -> Option<&Slot> {
        self.slots
            .iter()
            .find(|(key, ..)| key.session == session && key.connection == connection && key.database == database && key.schema == schema)
            .map(|(_, slot, used)| {
                used.store(self.clock.fetch_add(1, Relaxed) + 1, Relaxed);
                slot
            })
    }

    pub(super) fn state(&self, key: &SchemaKey) -> SchemaState {
        match self.slot(&key.connection, &key.database, &key.schema, key.session) {
            None => SchemaState::Absent,
            Some(Slot::Loading { .. }) => SchemaState::Loading,
            Some(Slot::Ready(_)) => SchemaState::Ready,
            Some(Slot::Unavailable) => SchemaState::Unavailable,
            Some(Slot::Failed(at)) if at.elapsed() < RETRY_AFTER => SchemaState::Unavailable,
            Some(Slot::Failed(_)) => SchemaState::Absent,
        }
    }

    /// The stored columns of `table` (its exact name) of the schema, without allocating.
    pub(super) fn table(&self, connection: &str, database: &str, schema: &str, session: u64, table: &str) -> Option<&[StoredColumn]> {
        match self.slot(connection, database, schema, session)? {
            Slot::Ready(columns) => columns.table(table),
            _ => None,
        }
    }

    /// Notes that the schema is wanted. `true`: the caller starts the request; `false`: one is in
    /// flight (the waiter joins it) or the schema is already settled. Readings of the connection's
    /// older sessions are dropped.
    pub(super) fn begin(&mut self, key: &SchemaKey, waiter: Option<(LoadRequest, u64)>) -> bool {
        let tick = self.clock.fetch_add(1, Relaxed) + 1;
        let waiters = if let Some((_, slot, used)) = self.slots.iter_mut().find(|(other, ..)| other == key) {
            used.store(tick, Relaxed);
            match slot {
                Slot::Loading { waiters, .. } => {
                    waiters.extend(waiter);
                    return false;
                }
                // A failure that is old enough is asked again.
                Slot::Failed(at) if at.elapsed() >= RETRY_AFTER => {
                    *slot = Slot::Loading { stale: false, waiters: waiter.into_iter().collect() };
                    #[cfg(test)]
                    {
                        self.fetches += 1;
                    }
                    return true;
                }
                _ => return false,
            }
        } else {
            waiter.into_iter().collect()
        };
        self.slots.retain(|(other, slot, _)| {
            other.connection != key.connection || other.session == key.session || matches!(slot, Slot::Loading { .. })
        });
        self.slots.push((key.clone(), Slot::Loading { stale: false, waiters }, AtomicU64::new(tick)));
        while self.slots.len() > MAX_SCHEMAS {
            // The least recently used one that is not in flight (its waiters need the answer).
            let oldest = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, (_, slot, _))| !matches!(slot, Slot::Loading { .. }))
                .min_by_key(|(_, (_, _, used))| used.load(Relaxed))
                .map(|(at, _)| at);
            match oldest {
                Some(at) => self.slots.remove(at),
                None => break,
            };
        }
        #[cfg(test)]
        {
            self.fetches += 1;
        }
        true
    }

    /// The request ended: keeps its answer (unless the schema was invalidated meanwhile) and returns
    /// the loads that waited for it.
    pub(super) fn finish(&mut self, key: &SchemaKey, outcome: Outcome) -> Vec<(LoadRequest, u64)> {
        let Some(at) = self.slots.iter().position(|(other, ..)| other == key) else { return Vec::new() };
        let Slot::Loading { stale, waiters } = std::mem::replace(&mut self.slots[at].1, Slot::Unavailable) else {
            return Vec::new();
        };
        if stale {
            self.slots.remove(at);
        } else {
            self.slots[at].1 = match outcome {
                Outcome::Ready(columns) => Slot::Ready(columns),
                Outcome::Unavailable => Slot::Unavailable,
                Outcome::Failed => Slot::Failed(std::time::Instant::now()),
            };
        }
        waiters
    }

    /// Makes every failed request old enough to be asked again (tests do not wait for it).
    #[cfg(test)]
    pub(super) fn expire_failures(&mut self) {
        for (_, slot, _) in &mut self.slots {
            if let Slot::Failed(at) = slot {
                *at = std::time::Instant::now() - RETRY_AFTER * 2;
            }
        }
    }

    #[cfg(test)]
    pub(super) fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Forgets what `covered` selects: all of a schema's columns, or only `table`'s when given. A
    /// request in flight still answers its waiters but is not kept.
    pub(super) fn forget(&mut self, covered: impl Fn(&SchemaKey) -> bool, table: Option<&str>) {
        self.slots.retain_mut(|(key, slot, _)| {
            if !covered(key) {
                return true;
            }
            match (slot, table) {
                (Slot::Loading { stale, .. }, _) => {
                    *stale = true;
                    true
                }
                (Slot::Ready(columns), Some(table)) => {
                    columns.remove_table(table);
                    true
                }
                (Slot::Unavailable | Slot::Failed(_), Some(_)) => true,
                (_, None) => false,
            }
        });
    }

    /// A connection that is closed leaves nothing behind. Returns the loads that waited for a request
    /// still in flight (it answers nothing now): the caller settles them.
    pub(super) fn forget_connection(&mut self, connection: &str) -> Vec<(LoadRequest, u64)> {
        let mut orphaned = Vec::new();
        self.slots.retain_mut(|(key, slot, _)| {
            if key.connection != connection {
                return true;
            }
            if let Slot::Loading { waiters, .. } = slot {
                orphaned.append(waiters);
            }
            false
        });
        orphaned
    }
}

/// The tree rows of a table's Columns folder from its stored columns.
fn column_children(connection: &str, database: &str, schema: &str, object: &str, columns: &[StoredColumn]) -> Vec<LoadedChild> {
    columns.iter().map(|column| navigator::column_child(connection, database, schema, object, &column.name)).collect()
}

impl AppRoot {
    /// Whether the driver behind `connection` answers the schema-wide column request.
    pub(super) fn driver_lists_schema_columns(&self, connection: &str) -> bool {
        let driver_id = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
            .map(|saved| saved.kind.driver_id())
            .unwrap_or_default();
        self.driver_capabilities(connection, driver_id)
            .is_some_and(|capabilities| capabilities.has(engine_driver_api::Capability::MetadataSchemaColumns))
    }

    pub(super) fn schema_key(&self, connection: &str, database: &str, schema: &str) -> SchemaKey {
        SchemaKey {
            connection: connection.to_owned(),
            database: database.to_owned(),
            schema: schema.to_owned(),
            session: self.driver_runtime.connection_ids.get(connection).copied().unwrap_or(0),
        }
    }

    /// Answers the Columns load `request` from the schema's stored columns when it can, or defers it to
    /// the schema request (`true`: nothing more to do). `false`: the table is asked for on its own.
    pub(super) fn serve_columns_from_schema(&mut self, request: &LoadRequest, generation: u64, cx: &mut Context<Self>) -> bool {
        let LoadRequest::Columns { connection, database, schema, object } = request else { return false };
        if !self.driver_lists_schema_columns(connection) {
            return false;
        }
        let key = self.schema_key(connection, database, schema);
        let state = self.schema_columns.state(&key);
        match state {
            SchemaState::Ready => {
                let Some(columns) = self.schema_columns.table(connection, database, schema, key.session, object) else { return false };
                let children = column_children(connection, database, schema, object, columns);
                self.apply_navigator_load(request, generation, Ok(children), cx);
                cx.notify();
                true
            }
            SchemaState::Unavailable => false,
            // A load that completion started wants several tables: it waits for the one request.
            // One in flight is joined; one that starts here is only for a schema small enough to read
            // whole (a huge one is asked for per table, as when no completion is involved).
            SchemaState::Loading | SchemaState::Absent
                if self.completion_loads.started.contains(&request.parent_id())
                    && (state == SchemaState::Loading || self.schema_is_small(connection, database, schema)) =>
            {
                if self.schema_columns.begin(&key, Some((request.clone(), generation))) {
                    self.load_schema_columns(key, cx);
                }
                true
            }
            SchemaState::Loading => false,
            SchemaState::Absent => {
                if self.schema_is_small(connection, database, schema) && self.schema_columns.begin(&key, None) {
                    self.load_schema_columns(key, cx);
                }
                false
            }
        }
    }

    /// Whether the schema's table list is loaded and short enough to read its columns whole.
    fn schema_is_small(&self, connection: &str, database: &str, schema: &str) -> bool {
        let tables = navigator::object_group_id(connection, database, schema, ObjectGroup::Tables);
        self.navigator.loaded_children(&tables).is_some_and(|tables| tables.len() <= PREFETCH_MAX_TABLES)
    }

    /// Hands back the loads that waited for a schema request that ended or was dropped: only one still
    /// waited for by the tree (a node reset meanwhile has its own) is run again.
    pub(super) fn redispatch_schema_waiters(&mut self, waiters: Vec<(LoadRequest, u64)>, cx: &mut Context<Self>) {
        for (waiting, generation) in waiters {
            if self.navigator.load_generation(&waiting) == Some(generation) {
                self.dispatch_provider_load(waiting, cx);
            }
        }
    }

    /// Asks the driver for the columns of the schema (every page, off the UI thread) and settles the
    /// loads that wait for it.
    fn load_schema_columns(&mut self, key: SchemaKey, cx: &mut Context<Self>) {
        let session = self.driver_runtime.sessions.get(&key.connection).cloned();
        let request = schema_columns::request(&key.database, &key.schema, self.sql_supports_schemas(&key.connection));
        cx.spawn(async move |this, cx| {
            let result = match session {
                None => Outcome::Failed,
                Some(session) => cx
                    .background_executor()
                    .spawn(async move {
                        smol::unblock(move || {
                            schema_columns::collect(request, |request| super::navigator_ui::list_driver_nav_nodes(&session, request))
                        })
                        .await
                    })
                    .await
                    .map_or_else(
                        |error| match error {
                            schema_columns::SchemaColumnsError::Fetch(_) => Outcome::Failed,
                            _ => Outcome::Unavailable,
                        },
                        Outcome::Ready,
                    ),
            };
            let _ = this.update(cx, |this, cx| {
                let waiters = this.schema_columns.finish(&key, result);
                this.redispatch_schema_waiters(waiters, cx);
            });
        })
        .detach();
    }
}
