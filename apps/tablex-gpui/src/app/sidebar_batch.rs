use super::*;
use crate::credentials::{CredentialStore, SystemStore};

impl AppRoot {
    pub(super) fn toggle_sidebar_group_connections(
        &mut self,
        group: &str,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let prefs = self.navigator.preferences();
        if !prefs.groups.iter().any(|item| item.id == group) {
            return Err("Connection group not found".into());
        }
        let members: Vec<_> = self
            .saved_connections
            .iter()
            .filter_map(|saved| {
                let id = saved.id.to_string();
                let mut parent = prefs
                    .connections
                    .iter()
                    .find(|item| item.id == id)
                    .and_then(|item| item.group.as_deref());
                while let Some(current) = parent {
                    if current == group {
                        return Some(id);
                    }
                    parent = prefs
                        .groups
                        .iter()
                        .find(|item| item.id == current)
                        .and_then(|item| item.parent.as_deref());
                }
                None
            })
            .collect();
        for id in &members {
            self.navigator.reveal_connection_group(id);
        }
        let members: HashSet<_> = members.into_iter().collect();
        let projected: Vec<_> = self
            .navigator
            .visible_rows()
            .into_iter()
            .filter_map(|row| match row.id {
                NavigatorNodeId::Connection { connection } if members.contains(&connection) => {
                    Some(connection)
                }
                _ => None,
            })
            .collect();
        if projected.is_empty() {
            return Ok(0);
        }
        let mut selected: HashSet<String> = self
            .navigator
            .selected_connection_ids()
            .unwrap_or_default()
            .into_iter()
            .collect();
        if projected.iter().all(|id| selected.contains(id)) {
            for id in &projected {
                selected.remove(id);
            }
        } else {
            selected.extend(projected.iter().cloned());
        }
        let ordered: Vec<_> = self
            .navigator
            .visible_rows()
            .into_iter()
            .filter_map(|row| match row.id {
                NavigatorNodeId::Connection { connection } if selected.contains(&connection) => {
                    Some(connection)
                }
                _ => None,
            })
            .collect();
        self.navigator.clear_multi_selection();
        for id in &ordered {
            self.navigator.select_visible(
                &NavigatorNodeId::Connection {
                    connection: id.clone(),
                },
                true,
                false,
            );
        }
        cx.notify();
        Ok(ordered.len())
    }

    pub(super) fn reconcile_sidebar_placements(&mut self, cx: &mut Context<Self>) {
        let live: HashSet<_> = self
            .saved_connections
            .iter()
            .map(|saved| saved.id.to_string())
            .collect();
        if !self
            .navigator
            .preferences()
            .connections
            .iter()
            .any(|item| !live.contains(&item.id))
        {
            return;
        }
        let mut candidate = self.navigator.preferences().clone();
        candidate.connections.retain(|item| live.contains(&item.id));
        if let Err(error) = self.change_sidebar_preferences(
            |prefs| {
                prefs.connections.retain(|item| live.contains(&item.id));
                Ok(())
            },
            cx,
        ) {
            if self.navigator.set_preferences(candidate).is_ok() {
                self.sidebar.refresh_overrides(self.navigator.preferences());
            }
            self.report_warning(Area::Connect, None, format!("Stale sidebar placements were removed in memory but not saved: {error}"));
            self.sidebar.notice = Some(format!(
                "Stale sidebar placements were removed in memory but not saved: {error}"
            ));
            cx.notify();
        }
    }

    pub(super) fn selected_sidebar_snapshot_matches(&self, ids: &[String]) -> bool {
        self.selected_sidebar_connection_ids()
            .is_ok_and(|current| current == ids)
    }
    pub(super) fn move_sidebar_drag_roots(
        &mut self,
        anchor: &NavigatorNodeId,
        snapshot: Option<&[NavigatorNodeId]>,
        target: &NavigatorNodeId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.navigator.action_target(anchor).is_none()
            || self.navigator.action_target(target).is_none()
        {
            return Err("Dragged sidebar node is no longer visible".into());
        }
        let selected = if let Some(snapshot) = snapshot {
            if !snapshot.contains(anchor) || self.navigator.selection_in_visible_order() != snapshot
            {
                return Err("Sidebar selection changed during drag".into());
            }
            snapshot.to_vec()
        } else {
            vec![anchor.clone()]
        };
        let prefs = self.navigator.preferences();
        let selected_groups: HashSet<_> = selected
            .iter()
            .filter_map(|id| match id {
                NavigatorNodeId::Group { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let group_has_selected_ancestor = |group_id: &str| {
            let mut parent = Some(group_id);
            while let Some(id) = parent {
                if selected_groups.contains(id) {
                    return true;
                }
                parent = prefs
                    .groups
                    .iter()
                    .find(|group| group.id == id)
                    .and_then(|group| group.parent.as_deref());
            }
            false
        };
        let roots: Vec<_> = selected
            .into_iter()
            .filter(|node| match node {
                NavigatorNodeId::Group { id } => prefs
                    .groups
                    .iter()
                    .find(|group| group.id == *id)
                    .and_then(|group| group.parent.as_deref())
                    .is_none_or(|parent| !group_has_selected_ancestor(parent)),
                NavigatorNodeId::Connection { connection } => prefs
                    .connections
                    .iter()
                    .find(|item| item.id == *connection)
                    .and_then(|item| item.group.as_deref())
                    .is_none_or(|group| !group_has_selected_ancestor(group)),
                _ => false,
            })
            .collect();
        if roots.is_empty()
            || roots
                .iter()
                .any(|node| self.navigator.action_target(node).is_none())
        {
            return Err("Drag contains no visible connection or group roots".into());
        }
        let group_roots: Vec<_> = roots
            .iter()
            .filter_map(|id| match id {
                NavigatorNodeId::Group { id } => Some(id.clone()),
                _ => None,
            })
            .collect();
        let connection_roots: Vec<_> = roots
            .iter()
            .filter_map(|id| match id {
                NavigatorNodeId::Connection { connection } => Some(connection.clone()),
                _ => None,
            })
            .collect();
        if !group_roots.is_empty() && !connection_roots.is_empty() {
            return Err("Select only groups or only connections to drag".into());
        }
        let root_set: HashSet<_> = roots.iter().collect();
        if root_set.contains(target) {
            return Ok(());
        }
        let all = self.saved_connection_ids();
        self.change_sidebar_preferences(
            |prefs| {
                prefs.normalize_unplaced_connections(&all);
                let (group, position) = match target {
                    NavigatorNodeId::Group { id } => (Some(id.clone()), usize::MAX),
                    NavigatorNodeId::Connection { connection } => {
                        let parent = prefs
                            .connections
                            .iter()
                            .find(|item| item.id == *connection)
                            .ok_or("Drop target connection missing")?
                            .group
                            .clone();
                        let mut siblings: Vec<_> = prefs
                            .groups
                            .iter()
                            .filter(|item| item.parent == parent)
                            .map(|item| {
                                (
                                    item.order,
                                    NavigatorNodeId::Group {
                                        id: item.id.clone(),
                                    },
                                )
                            })
                            .chain(
                                prefs
                                    .connections
                                    .iter()
                                    .filter(|item| item.group == parent)
                                    .map(|item| {
                                        (
                                            item.order,
                                            NavigatorNodeId::Connection {
                                                connection: item.id.clone(),
                                            },
                                        )
                                    }),
                            )
                            .collect();
                        siblings.sort_by_key(|(order, _)| *order);
                        siblings.retain(|(_, node)| !root_set.contains(node));
                        (
                            parent,
                            siblings
                                .iter()
                                .position(|(_, node)| node == target)
                                .ok_or("Drop target was removed")?,
                        )
                    }
                    _ => return Err("Invalid sidebar drop target".into()),
                };
                if !group_roots.is_empty() {
                    prefs.move_groups(&group_roots, group.as_deref(), position)
                } else {
                    prefs.move_connections(&all, &connection_roots, group.as_deref(), position)
                }
            },
            cx,
        )?;
        self.navigator.clear_multi_selection();
        cx.notify();
        Ok(())
    }

    pub(super) fn selected_sidebar_connection_ids(&self) -> Result<Vec<String>, String> {
        if !self.navigator.connection_multi_mode() {
            return Err("Select connections to use a batch action".into());
        }
        let ids = self
            .navigator
            .selected_connection_ids()
            .ok_or("Mixed sidebar selection cannot use connection batch actions")?;
        if ids.iter().any(|id| {
            self.navigator
                .action_target(&NavigatorNodeId::Connection {
                    connection: id.clone(),
                })
                .is_none()
                || self.navigator.connection(id).is_none()
        }) {
            return Err("Selected connection is no longer available".into());
        }
        Ok(ids)
    }

    fn saved_connection_ids(&self) -> Vec<String> {
        self.saved_connections
            .iter()
            .map(|saved| saved.id.to_string())
            .collect()
    }

    pub(super) fn move_selected_sidebar_connections(
        &mut self,
        group: Option<&str>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let ids = self.selected_sidebar_connection_ids()?;
        let all = self.saved_connection_ids();
        self.change_sidebar_preferences(
            |prefs| prefs.move_connections(&all, &ids, group, usize::MAX),
            cx,
        )?;
        self.navigator.clear_multi_selection();
        cx.notify();
        Ok(ids.len())
    }

    pub(super) fn create_group_from_selected_connections(
        &mut self,
        name: &str,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let ids = self.selected_sidebar_connection_ids()?;
        let all = self.saved_connection_ids();
        let group = uuid::Uuid::new_v4().to_string();
        self.change_sidebar_preferences(
            |prefs| {
                prefs.add_group(&group, name, None)?;
                prefs.move_connections(&all, &ids, Some(&group), usize::MAX)
            },
            cx,
        )?;
        self.navigator.clear_multi_selection();
        cx.notify();
        Ok(group)
    }

    pub(super) fn hide_selected_sidebar_connections(
        &mut self,
        hidden: bool,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let ids = self.selected_sidebar_connection_ids()?;
        self.change_sidebar_preferences(
            |prefs| {
                for id in &ids {
                    prefs.set_hidden(id, hidden)?;
                }
                Ok(())
            },
            cx,
        )?;
        self.navigator.clear_multi_selection();
        cx.notify();
        Ok(ids.len())
    }

    pub(super) fn confirm_delete_selected_connections(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(ids) = self.selected_sidebar_connection_ids() else {
            return;
        };
        // An open transaction of a tab on one of them is asked about first; the delete is asked for
        // again once they ended.
        let tabs = self.tabs_of_connections(&ids);
        if self.guard_transactions(tabs, "Deleting the connections ends their sessions.", window, cx, |this, window, cx| {
            this.confirm_delete_selected_connections(window, cx)
        }) {
            return;
        }
        let names = ids
            .iter()
            .filter_map(|id| {
                self.saved_connections
                    .iter()
                    .find(|saved| saved.matches_id(id))
                    .map(|saved| saved.name.as_str())
            })
            .take(6)
            .collect::<Vec<_>>()
            .join(", ");
        let count = ids.len();
        let summary = if count > 6 {
            format!("{names}, and {} more", count - 6)
        } else {
            names
        };
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let ids = ids.clone();
            let summary = summary.clone();
            dialog.title(format!("Delete {count} Saved Connections"))
                .child(format!("Delete saved connection configurations: {summary}? Databases and their data will NOT be deleted."))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Delete"))
                .confirm()
                .on_ok(move |_, _, app| {
                    root.update(app, |this, cx| {
                        if this.scenario_ui.service.is_some() { return false; }
                        if !this.selected_sidebar_snapshot_matches(&ids) { return false; }
                        // A transaction opened since the question was asked is not lost without one.
                        if this.open_transactions_on(&ids) { return false; }
                        this.start_selected_connections_delete(ids.clone(), cx);
                        true
                    }).unwrap_or(false)
                })
        });
        cx.notify();
    }

    fn start_selected_connections_delete(&mut self, snapshot: Vec<String>, cx: &mut Context<Self>) {
        let storage_ids: Vec<_> = snapshot
            .iter()
            .map(|id| crate::connections::storage_connection_id(id))
            .collect();
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || -> Result<Vec<String>, String> {
                engine_storage::connection::delete_connections_batch(&storage_ids)
                    .map_err(|error| error.to_string())?;
                let mut warnings = Vec::new();
                for id in &storage_ids {
                    if let Err(error) = SystemStore::new().delete_all(id) {
                        warnings.push(format!(
                            "Could not clear {} secrets for {id}: {error}",
                            crate::platform::CREDENTIAL_STORE_LABEL
                        ));
                    }
                }
                Ok(warnings)
            })
            .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(warnings) => {
                    this.apply_deleted_connections_after_commit(&snapshot, warnings, cx);
                }
                Err(error) => {
                    this.report_error(Area::Connect, None, format!("Could not delete saved connections; none were changed: {error}"));
                    this.sidebar.notice = Some(format!(
                        "Could not delete saved connections; none were changed: {error}"
                    ));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    #[cfg(test)]
    pub(super) fn delete_selected_connections_with(
        &mut self,
        snapshot: &[String],
        storage_delete: impl FnOnce(&[String]) -> Result<(), String>,
        mut cleanup: impl FnMut(&str) -> Result<(), String>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let current = self.selected_sidebar_connection_ids()?;
        if current != snapshot || snapshot.is_empty() {
            return Err("Selection changed; reopen the confirmation".into());
        }
        let storage_ids: Vec<_> = snapshot
            .iter()
            .map(|id| crate::connections::storage_connection_id(id))
            .collect();
        if let Err(error) = storage_delete(&storage_ids) {
            let message = format!("Could not delete saved connections; none were changed: {error}");
            self.sidebar.notice = Some(message.clone());
            cx.notify();
            return Err(message);
        }
        let mut warnings = Vec::new();
        for id in &storage_ids {
            if let Err(error) = cleanup(id) {
                warnings.push(format!(
                    "Could not clear {} secrets for {id}: {error}",
                    crate::platform::CREDENTIAL_STORE_LABEL
                ));
            }
        }
        self.apply_deleted_connections_after_commit(snapshot, warnings, cx);
        Ok(snapshot.len())
    }

    pub(super) fn apply_deleted_connections_after_commit(
        &mut self,
        snapshot: &[String],
        mut warnings: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let clear_selection = self.navigator.connection_multi_mode()
            && self.navigator.selected_connection_ids().as_deref() == Some(snapshot);
        for id in snapshot {
            self.disconnect_connection(id, cx);
        }
        let deleted: HashSet<_> = snapshot.iter().map(String::as_str).collect();
        self.saved_connections
            .retain(|saved| !deleted.contains(saved.id.to_string().as_str()));
        self.navigator
            .set_connections(self.saved_connections.clone());
        if self.scenario_ui.service.is_none() {
            self.refresh_saved_connections(cx);
        }
        let mut candidate = self.navigator.preferences().clone();
        for id in snapshot {
            candidate.remove_connection(id);
        }
        if candidate != *self.navigator.preferences() {
            if let Err(error) = self.change_sidebar_preferences(
                |prefs| {
                    for id in snapshot {
                        prefs.remove_connection(id);
                    }
                    Ok(())
                },
                cx,
            ) {
                // The DB commit cannot be rolled back here. Keep the live layout reconciled and report the unsaved file.
                self.navigator
                    .set_preferences(candidate)
                    .expect("removing placements preserves validation");
                self.sidebar.refresh_overrides(self.navigator.preferences());
                warnings.push(format!("Sidebar placement was not saved: {error}"));
            }
        }
        if clear_selection {
            self.navigator.clear_multi_selection();
        }
        if !warnings.is_empty() {
            if let Some(existing) = self.sidebar.notice.take() {
                warnings.push(existing);
            }
            self.report_warning(Area::Connect, None, warnings.join("; "));
            self.sidebar.notice = Some(warnings.join("; "));
        }
        cx.notify();
    }
}
