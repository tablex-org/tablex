//! The key browser of a Redis connection: database nodes list their keys
//! (SCAN pages, `Load More`), optionally grouped into a namespace tree by the
//! `:` delimiter.
//!
//! The tree is plain navigator state: a namespace is a provider node of kind
//! `namespace` (its children are already loaded, so expanding it never calls
//! the driver) and a key keeps the id the driver gave it. Nothing here adds a
//! per-row entity or listener: rows stay what they were, `NavigatorRow`s of the
//! virtualized sidebar list.
//!
//! A SCAN page is merged into the tree, not sorted into it again: only the
//! sibling lists a page touches are merged (`O(list + page)`), so loading the
//! 200th page of a 100k-key database costs about the same as the 2nd.

use super::{LoadedChild, NavigatorNodeId, NavigatorState, ProviderNode};
use crate::connections::ConnectionKind;
use crate::redis::{split_namespace, KeyFilter};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashSet;

/// Provider kind of a database node, key node and namespace folder.
pub const DATABASE: &str = "database";
pub const KEY: &str = "key";
pub const NAMESPACE: &str = "namespace";

impl NavigatorState {
    /// The filter of a database node (the default when none was set).
    pub fn redis_filter(&self, database: &NavigatorNodeId) -> KeyFilter {
        self.redis_filters.get(database).cloned().unwrap_or_default()
    }

    /// Sets the filter of a database node. The caller reloads the node.
    pub fn set_redis_filter(&mut self, database: &NavigatorNodeId, filter: KeyFilter) {
        if filter == KeyFilter::default() {
            self.redis_filters.remove(database);
        } else {
            self.redis_filters.insert(database.clone(), filter);
        }
    }

    /// `true` for a database node of a Redis connection.
    pub fn is_redis_database(&self, id: &NavigatorNodeId) -> bool {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return false;
        };
        self.provider_nodes
            .get(id)
            .is_some_and(|node| node.kind == DATABASE)
            && self
                .connection(connection)
                .is_some_and(|saved| saved.kind == ConnectionKind::Redis)
    }

    /// The `SELECT` index of a Redis database node.
    pub fn redis_db_index(&self, id: &NavigatorNodeId) -> Option<String> {
        let node = self.provider_nodes.get(id)?;
        node.payload
            .get("database")
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// The database node a key or namespace node lives under.
    pub fn redis_database_of(&self, id: &NavigatorNodeId) -> Option<NavigatorNodeId> {
        let NavigatorNodeId::Provider { connection, .. } = id else {
            return None;
        };
        let node = self.provider_nodes.get(id)?;
        let db = node.payload.get("database").and_then(Value::as_str)?;
        Some(NavigatorNodeId::Provider {
            connection: connection.clone(),
            node: format!("database:{db}"),
        })
    }

    /// The full name of every key loaded under `database`.
    pub fn redis_loaded_keys<'a>(
        &'a self,
        connection: &'a str,
        database: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.provider_nodes.iter().filter_map(move |(id, node)| {
            let NavigatorNodeId::Provider { connection: owner, .. } = id else {
                return None;
            };
            (owner == connection
                && node.kind == KEY
                && node.payload.get("database").and_then(Value::as_str) == Some(database))
            .then(|| node.payload.get("key_display").and_then(Value::as_str))
            .flatten()
        })
    }

    /// Whether the keys loaded under this database node form a namespace tree.
    pub(super) fn groups_redis_keys(&self, database: &NavigatorNodeId) -> bool {
        self.is_redis_database(database) && self.redis_filter(database).group
    }

    /// Splices the rows of keys just appended to the flat key list of
    /// `database` in place of its `Load More` row, instead of flattening the
    /// whole tree again (`O(all rows)` per page: 100k rows is 100k row and id
    /// clones). Returns `false` when the shape is not the plain append - the
    /// caller then rebuilds.
    pub(super) fn append_visible_rows(&mut self, database: &NavigatorNodeId, added: &[NavigatorNodeId]) -> bool {
        if self.search_active() || !self.is_redis_database(database) {
            return false;
        }
        let marker = self.load_more_id(database);
        let (Some(&at), Some(&parent_at)) = (self.visible_lookup.get(&marker), self.visible_lookup.get(database))
        else {
            return false;
        };
        let depth = self.visible[parent_at].depth + 1;
        let has_more = self.next_cursors.contains_key(database);
        if self.visible[at].depth != depth
            || (!has_more && self.selected.as_ref() == Some(&marker))
            || added.iter().any(|id| self.is_expandable(id))
        {
            return false;
        }
        let mut rows: Vec<super::NavigatorRow> = added
            .iter()
            .map(|id| super::NavigatorRow {
                id: id.clone(),
                label: self.labels.get(id).cloned().unwrap_or_else(|| super::default_label(id)),
                depth,
                expandable: false,
                expanded: false,
                load: self.load_states.get(id).cloned(),
                disabled: false,
            })
            .collect();
        if has_more {
            rows.push(super::NavigatorRow {
                id: marker.clone(),
                label: "Load More".into(),
                depth,
                expandable: false,
                expanded: false,
                load: self.load_states.get(database).cloned(),
                disabled: false,
            });
        } else {
            self.visible_lookup.remove(&marker);
            self.selected_ids.remove(&marker);
        }
        self.visible.splice(at..=at, rows);
        // The database row stops showing its load marker.
        self.visible[parent_at].load = self.load_states.get(database).cloned();
        for index in at..self.visible.len() {
            if let Some(slot) = self.visible_lookup.get_mut(&self.visible[index].id) {
                *slot = index;
            } else {
                self.visible_lookup.insert(self.visible[index].id.clone(), index);
            }
        }
        true
    }

    /// Adds one page of keys below `database` as a namespace tree.
    pub(super) fn store_grouped_keys(
        &mut self,
        database: &NavigatorNodeId,
        keys: Vec<LoadedChild>,
        append: bool,
    ) {
        let NavigatorNodeId::Provider { connection, .. } = database else {
            return;
        };
        let connection = connection.clone();
        let db_index = self.redis_db_index(database).unwrap_or_default();
        if !append {
            self.forget_children(database);
        }
        self.catalog_revision += 1;

        // New ids per sibling list, in arrival order.
        let mut additions: Vec<(NavigatorNodeId, Vec<NavigatorNodeId>, Vec<NavigatorNodeId>)> =
            Vec::new();
        let mut counted: HashSet<NavigatorNodeId> = HashSet::new();
        for key in keys {
            // SCAN may return a key twice across pages.
            if self.provider_nodes.contains_key(&key.id) {
                continue;
            }
            let Some(mut provider) = key.provider else {
                continue;
            };
            let full = key.label;
            let mut owner = database.clone();
            let mut rest = full.as_str();
            let mut consumed = 0;
            while let Some((head, tail)) = split_namespace(rest) {
                let path = &full[..consumed + head.len()];
                let folder = NavigatorNodeId::Provider {
                    connection: connection.clone(),
                    node: format!("ns:{db_index}:{path}"),
                };
                if !self.provider_nodes.contains_key(&folder) {
                    self.provider_nodes.insert(
                        folder.clone(),
                        ProviderNode {
                            kind: NAMESPACE.into(),
                            subtitle: None,
                            icon: None,
                            has_children: true,
                            payload: json!({
                                "database": db_index,
                                "prefix": path,
                                "name": head,
                                "count": 0,
                            }),
                            actions: Vec::new(),
                        },
                    );
                    self.labels.insert(folder.clone(), head.to_owned());
                    self.children.insert(folder.clone(), Some(Vec::new()));
                    self.load_states
                        .insert(folder.clone(), super::NodeLoadState::Loaded);
                    slot(&mut additions, &owner).1.push(folder.clone());
                }
                if let Some(count) = self
                    .provider_nodes
                    .get_mut(&folder)
                    .and_then(|node| node.payload.get_mut("count"))
                {
                    *count = json!(count.as_u64().unwrap_or(0) + 1);
                }
                counted.insert(folder.clone());
                consumed += head.len() + 1;
                rest = tail;
                owner = folder;
            }
            if let Some(map) = provider.payload.as_object_mut() {
                map.insert("key_display".into(), full.clone().into());
            }
            self.labels.insert(key.id.clone(), rest.to_owned());
            self.provider_nodes.insert(key.id.clone(), provider);
            slot(&mut additions, &owner).2.push(key.id);
        }

        for (owner, folders, leaves) in additions {
            let existing = self
                .children
                .get_mut(&owner)
                .and_then(Option::take)
                .unwrap_or_default();
            let merged = self.merge_siblings(existing, folders, leaves);
            self.children.insert(owner, Some(merged));
        }
        for folder in counted {
            let Some(node) = self.provider_nodes.get(&folder) else {
                continue;
            };
            let name = node.payload.get("name").and_then(Value::as_str).unwrap_or("");
            let count = node.payload.get("count").and_then(Value::as_u64).unwrap_or(0);
            self.labels.insert(folder, format!("{name} ({count})"));
        }
    }

    /// `existing` is sorted (folders, then keys); `folders` and `leaves` are the
    /// new arrivals. Folders sort before keys, each by name, case-insensitively.
    fn merge_siblings(
        &self,
        mut existing: Vec<NavigatorNodeId>,
        mut folders: Vec<NavigatorNodeId>,
        mut leaves: Vec<NavigatorNodeId>,
    ) -> Vec<NavigatorNodeId> {
        let split = existing
            .iter()
            .position(|id| !self.is_namespace(id))
            .unwrap_or(existing.len());
        let by_name = |a: &NavigatorNodeId, b: &NavigatorNodeId| {
            compare_names(self.sort_name(a), self.sort_name(b))
        };
        folders.sort_by(by_name);
        leaves.sort_by(by_name);
        // Leaves first: they sit behind the folders, so the folder segment
        // keeps its place.
        insert_sorted(&mut existing, split.., leaves, by_name);
        insert_sorted(&mut existing, ..split, folders, by_name);
        existing
    }

    fn is_namespace(&self, id: &NavigatorNodeId) -> bool {
        self.provider_nodes
            .get(id)
            .is_some_and(|node| node.kind == NAMESPACE)
    }

    fn sort_name(&self, id: &NavigatorNodeId) -> &str {
        match self.provider_nodes.get(id) {
            Some(node) if node.kind == NAMESPACE => {
                node.payload.get("name").and_then(Value::as_str).unwrap_or("")
            }
            _ => self.labels.get(id).map_or("", String::as_str),
        }
    }

    /// Drops the loaded children of `parent` (and their subtrees) without
    /// touching `parent` itself.
    fn forget_children(&mut self, parent: &NavigatorNodeId) {
        let mut pending = self.children.remove(parent).flatten().unwrap_or_default();
        while let Some(node) = pending.pop() {
            if let Some(Some(children)) = self.children.remove(&node) {
                pending.extend(children);
            }
            self.load_states.remove(&node);
            self.load_generations.remove(&node);
            self.labels.remove(&node);
            self.provider_nodes.remove(&node);
            self.expanded.remove(&node);
            self.selected_ids.remove(&node);
            if self.selected.as_ref() == Some(&node) {
                self.selected = None;
            }
        }
    }

    /// Removes a key the server deleted from the tree: its parent folders lose
    /// one count and the ones left empty disappear. No reload, and no folder
    /// collapses.
    pub fn remove_redis_key(&mut self, key: &NavigatorNodeId) -> bool {
        let Some(full) = self
            .provider_nodes
            .get(key)
            .and_then(|node| node.payload.get("key_display"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return false;
        };
        let (Some(database), NavigatorNodeId::Provider { connection, .. }) =
            (self.redis_database_of(key), key)
        else {
            return false;
        };
        let db_index = self.redis_db_index(&database).unwrap_or_default();
        let mut folders = Vec::new();
        let (mut rest, mut consumed) = (full.as_str(), 0);
        while let Some((head, tail)) = split_namespace(rest) {
            folders.push(NavigatorNodeId::Provider {
                connection: connection.clone(),
                node: format!("ns:{db_index}:{}", &full[..consumed + head.len()]),
            });
            consumed += head.len() + 1;
            rest = tail;
        }
        // The deepest empty folder (or the key itself) is what leaves its parent.
        let mut doomed = key.clone();
        for folder in folders.iter().rev() {
            let Some(node) = self.provider_nodes.get_mut(folder) else {
                continue;
            };
            let count = node.payload.get("count").and_then(Value::as_u64).unwrap_or(1);
            let left = count.saturating_sub(1);
            node.payload["count"] = json!(left);
            let name = node.payload.get("name").and_then(Value::as_str).unwrap_or("").to_owned();
            self.labels.insert(folder.clone(), format!("{name} ({left})"));
            if left == 0 {
                doomed = folder.clone();
            }
        }
        self.remove_node(&doomed)
    }
}

fn slot<'a>(
    additions: &'a mut Vec<(NavigatorNodeId, Vec<NavigatorNodeId>, Vec<NavigatorNodeId>)>,
    owner: &NavigatorNodeId,
) -> &'a mut (NavigatorNodeId, Vec<NavigatorNodeId>, Vec<NavigatorNodeId>) {
    let at = match additions.iter().position(|(id, ..)| id == owner) {
        Some(at) => at,
        None => {
            additions.push((owner.clone(), Vec::new(), Vec::new()));
            additions.len() - 1
        }
    };
    &mut additions[at]
}

/// Case-insensitive, allocation-free; ties broken by the exact text so the
/// order is total.
fn compare_names(a: &str, b: &str) -> Ordering {
    a.chars()
        .flat_map(char::to_lowercase)
        .cmp(b.chars().flat_map(char::to_lowercase))
        .then_with(|| a.cmp(b))
}

/// Inserts the sorted `new` into the sorted segment `range` of `list`. A few
/// arrivals go in place (a binary search each, no allocation); a large page is
/// merged by moving runs across. Equal names keep the old id first.
fn insert_sorted(
    list: &mut Vec<NavigatorNodeId>,
    range: impl std::ops::RangeBounds<usize>,
    new: Vec<NavigatorNodeId>,
    mut compare: impl FnMut(&NavigatorNodeId, &NavigatorNodeId) -> Ordering,
) {
    use std::ops::Bound;
    if new.is_empty() {
        return;
    }
    let start = match range.start_bound() {
        Bound::Included(&start) => start,
        _ => 0,
    };
    let end = match range.end_bound() {
        Bound::Excluded(&end) => end,
        Bound::Included(&end) => end + 1,
        Bound::Unbounded => list.len(),
    };
    const IN_PLACE: usize = 16;
    if new.len() <= IN_PLACE {
        for (inserted, item) in new.into_iter().enumerate() {
            let end = end + inserted;
            let place = start
                + list[start..end]
                    .partition_point(|existing| compare(existing, &item) != Ordering::Greater);
            list.insert(place, item);
        }
        return;
    }
    let tail = list.split_off(end);
    let old = list.split_off(start);
    let mut places = Vec::with_capacity(new.len());
    let mut from = 0;
    for item in &new {
        from += old[from..].partition_point(|existing| compare(existing, item) != Ordering::Greater);
        places.push(from);
    }
    let mut old = old.into_iter();
    let mut taken = 0;
    for (item, place) in new.into_iter().zip(places) {
        list.extend(old.by_ref().take(place - taken));
        taken = place;
        list.push(item);
    }
    list.extend(old);
    list.extend(tail);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::SavedConnection;
    use crate::navigator::{provider_child, LoadRequest};
    use engine_driver_api::{NavigatorNode, NavigatorPage};

    const CONN: &str = "22222222-2222-2222-2222-222222222222";

    fn redis_state() -> NavigatorState {
        NavigatorState::new(vec![SavedConnection {
            id: uuid::Uuid::parse_str(CONN).unwrap(),
            name: "Cache".into(),
            uri: "redis://localhost/0".into(),
            kind: ConnectionKind::Redis,
            save_password: false,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }])
    }

    fn db_node() -> NavigatorNode {
        NavigatorNode {
            id: "database:0".into(),
            kind: "database".into(),
            label: "db0 (9 keys)".into(),
            subtitle: None,
            icon: Some("database".into()),
            has_children: true,
            payload: json!({"database": "0", "db": 0, "keys": 9}),
            actions: vec!["refresh".into()],
        }
    }

    pub(super) fn key_node(name: &str, key_type: &str) -> NavigatorNode {
        let raw = crate::redis::encode_key(name.as_bytes());
        NavigatorNode {
            id: format!("key:0:{raw}"),
            kind: "key".into(),
            label: name.into(),
            subtitle: Some(key_type.into()),
            icon: Some(key_type.into()),
            has_children: false,
            payload: json!({"database": "0", "db": 0, "key_raw": raw, "key_type": key_type, "ttl": null, "size": 1}),
            actions: vec!["open_query".into()],
        }
    }

    /// Connects the Redis fixture connection and loads `db0`'s first page.
    pub(super) fn loaded(names: &[&str], next: Option<&str>) -> (NavigatorState, NavigatorNodeId) {
        let mut state = redis_state();
        state.enable_provider(CONN);
        let conn = NavigatorNodeId::Connection { connection: CONN.into() };
        let outcome = state.toggle_expansion(&conn);
        let request = outcome.request.expect("the connection lists its databases");
        let generation = state.load_generation(&request).unwrap();
        state.apply_provider_page(
            &request,
            generation,
            Ok(NavigatorPage { nodes: vec![db_node()], next_cursor: None }),
        );
        let db = NavigatorNodeId::Provider { connection: CONN.into(), node: "database:0".into() };
        let request = state.toggle_expansion(&db).request.expect("the database lists keys");
        let generation = state.load_generation(&request).unwrap();
        state.apply_provider_page(
            &request,
            generation,
            Ok(NavigatorPage {
                nodes: names.iter().map(|name| key_node(name, "string")).collect(),
                next_cursor: next.map(str::to_owned),
            }),
        );
        (state, db)
    }

    fn labels(state: &NavigatorState) -> Vec<(usize, String)> {
        state
            .visible_rows()
            .into_iter()
            .skip(2)
            .map(|row| (row.depth, row.label))
            .collect()
    }

    #[test]
    fn keys_form_a_namespace_tree_with_folders_first() {
        let (mut state, db) = loaded(&["user:2", "plain", "user:1:name", "order:9", "user:1:mail"], None);
        let expect = [
            (2, "order (1)"),
            (2, "user (3)"),
            (2, "plain"),
        ];
        assert_eq!(
            labels(&state),
            expect.iter().map(|(d, l)| (*d, l.to_string())).collect::<Vec<_>>()
        );
        // Folders open without a driver call.
        let user = NavigatorNodeId::Provider { connection: CONN.into(), node: "ns:0:user".into() };
        let outcome = state.toggle_expansion(&user);
        assert!(outcome.request.is_none());
        let user1 = NavigatorNodeId::Provider { connection: CONN.into(), node: "ns:0:user:1".into() };
        assert!(state.toggle_expansion(&user1).request.is_none());
        let rows = labels(&state);
        assert_eq!(
            rows.iter().map(|(d, l)| (*d, l.as_str())).collect::<Vec<_>>(),
            [
                (2, "order (1)"),
                (2, "user (3)"),
                (3, "1 (2)"),
                (4, "mail"),
                (4, "name"),
                (3, "2"),
                (2, "plain"),
            ]
        );
        // A key keeps its full name for the tab it opens.
        let key = state
            .provider_nodes
            .iter()
            .find(|(_, node)| node.payload["key_display"] == "user:1:mail")
            .unwrap();
        assert_eq!(key.1.kind, KEY);
        assert_eq!(state.redis_loaded_keys(CONN, "0").count(), 5);
        let _ = db;
    }

    #[test]
    fn later_pages_merge_into_the_same_folders_and_repeats_are_ignored() {
        let (mut state, db) = loaded(&["user:3", "user:1"], Some("17"));
        let more = state.load_more(&db).expect("a cursor is left");
        let LoadRequest::Provider { cursor, .. } = &more else { panic!("provider load") };
        assert_eq!(cursor.as_deref(), Some("17"));
        let generation = state.load_generation(&more).unwrap();
        state.apply_provider_page(
            &more,
            generation,
            Ok(NavigatorPage {
                nodes: vec![
                    key_node("user:2", "hash"),
                    key_node("user:1", "string"), // SCAN returned it twice
                    key_node("zz", "set"),
                    key_node("Admin:root", "string"),
                ],
                next_cursor: None,
            }),
        );
        let user = NavigatorNodeId::Provider { connection: CONN.into(), node: "ns:0:user".into() };
        state.toggle_expansion(&user);
        let rows: Vec<String> = labels(&state).into_iter().map(|(_, l)| l).collect();
        // Case-insensitive folder order; the repeat did not count twice.
        assert_eq!(rows, ["Admin (1)", "user (3)", "1", "2", "3", "zz"]);
    }

    #[test]
    fn a_flat_database_keeps_scan_order_and_a_filter_reaches_the_request() {
        let (mut state, db) = loaded(&["b", "a:1"], None);
        state.set_redis_filter(
            &db,
            KeyFilter { pattern: "a:*".into(), key_type: None, group: false },
        );
        state.reset_node(&db);
        let request = state.toggle_expansion(&db).request.unwrap();
        let LoadRequest::Provider { payload, .. } = &request else { panic!() };
        let payload = payload.as_ref().unwrap();
        assert_eq!(payload["pattern"], "a:*");
        assert_eq!(payload["database"], "0");
        let generation = state.load_generation(&request).unwrap();
        state.apply_provider_page(
            &request,
            generation,
            Ok(NavigatorPage { nodes: vec![key_node("b", "string"), key_node("a:1", "string")], next_cursor: None }),
        );
        // Ungrouped: full names, in the order the server answered.
        let rows: Vec<String> = labels(&state).into_iter().map(|(_, l)| l).collect();
        assert_eq!(rows, ["b", "a:1"]);
        state.set_redis_filter(&db, KeyFilter::default());
        assert_eq!(state.redis_filter(&db), KeyFilter::default());
    }

    #[test]
    fn a_page_appended_to_a_flat_list_splices_the_same_rows_a_full_rebuild_makes() {
        let (mut state, db) = loaded(&["k1", "k2"], Some("2"));
        state.set_redis_filter(&db, KeyFilter { group: false, ..Default::default() });
        state.reset_node(&db);
        let request = state.toggle_expansion(&db).request.unwrap();
        let generation = state.load_generation(&request).unwrap();
        let page = |names: &[&str], next: Option<&str>| NavigatorPage {
            nodes: names.iter().map(|name| key_node(name, "string")).collect(),
            next_cursor: next.map(str::to_owned),
        };
        state.apply_provider_page(&request, generation, Ok(page(&["k1", "k2"], Some("2"))));
        for (names, next) in [(&["k3", "k4", "k2"][..], Some("4")), (&["k5"][..], None)] {
            let more = state.load_more(&db).expect("a cursor");
            let generation = state.load_generation(&more).unwrap();
            state.apply_provider_page(&more, generation, Ok(page(names, next)));
            let spliced = state.visible_rows();
            let lookup = state.visible_lookup.clone();
            state.rebuild_visible();
            assert_eq!(spliced, state.visible_rows(), "same rows as a full rebuild");
            assert_eq!(lookup, state.visible_lookup, "same row index");
        }
        let labels: Vec<String> = state.visible_rows().into_iter().skip(2).map(|row| row.label).collect();
        assert_eq!(labels, ["k1", "k2", "k3", "k4", "k5"], "the repeated k2 was not listed twice, Load More is gone");
        assert_eq!(state.load_state(&db), Some(&crate::navigator::NodeLoadState::Loaded));
    }

    #[test]
    fn deleting_a_key_shrinks_its_folders_and_removes_empty_ones() {
        let (mut state, _db) = loaded(&["user:1:name", "user:2", "solo:x"], None);
        let id_of = |state: &NavigatorState, name: &str| {
            state
                .provider_nodes
                .iter()
                .find(|(_, node)| node.payload["key_display"] == name)
                .map(|(id, _)| id.clone())
                .unwrap()
        };
        let solo = id_of(&state, "solo:x");
        assert!(state.remove_redis_key(&solo));
        let rows: Vec<String> = labels(&state).into_iter().map(|(_, l)| l).collect();
        assert_eq!(rows, ["user (2)"], "the emptied folder is gone");
        let name = id_of(&state, "user:1:name");
        assert!(state.remove_redis_key(&name));
        let user = NavigatorNodeId::Provider { connection: CONN.into(), node: "ns:0:user".into() };
        state.toggle_expansion(&user);
        let rows: Vec<String> = labels(&state).into_iter().map(|(_, l)| l).collect();
        assert_eq!(rows, ["user (1)", "2"]);
        assert!(!state.remove_redis_key(&name), "already gone");
    }

    #[test]
    fn provider_child_keeps_the_driver_payload() {
        let child = provider_child(CONN, key_node("a", "list"));
        assert_eq!(child.provider.unwrap().payload["key_type"], "list");
    }
}
