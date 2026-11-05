use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSort {
    #[default]
    Manual,
    Ascending,
    Descending,
}

/// How the workspace tab bar clusters unpinned tabs (Chrome-style groups).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabGroupBy {
    #[default]
    None,
    Connection,
    Database,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionGroup {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    pub order: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionDisplay {
    pub id: String,
    pub group: Option<String>,
    pub order: usize,
    pub color: Option<String>,
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VisibleObjectFilter {
    /// None is all; Some(empty) is none. Patterns add matches to explicit names.
    pub databases: Option<Vec<String>>,
    pub patterns: Vec<String>,
    /// Schema selections keyed by database, with the same None/all semantics.
    pub schemas: HashMap<String, Option<Vec<String>>>,
    pub show_system: bool,
}

impl Default for VisibleObjectFilter {
    fn default() -> Self {
        Self {
            databases: None,
            patterns: Vec::new(),
            schemas: HashMap::new(),
            show_system: true,
        }
    }
}

impl VisibleObjectFilter {
    fn validate(&self) -> Result<(), String> {
        if self.patterns.len() > 32
            || self.patterns.iter().any(|pattern| pattern.len() > 256)
            || self
                .databases
                .as_ref()
                .is_some_and(|names| names.len() > 100_000)
            || self
                .schemas
                .values()
                .any(|names| names.as_ref().is_some_and(|names| names.len() > 100_000))
        {
            return Err("Visible filter exceeds the supported limit".into());
        }
        for pattern in &self.patterns {
            regex::RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_err(|_| "Invalid visible database pattern".to_string())?;
        }
        if self
            .databases
            .as_ref()
            .is_some_and(|names| names.iter().any(String::is_empty))
            || self.schemas.iter().any(|(database, names)| {
                database.is_empty()
                    || names
                        .as_ref()
                        .is_some_and(|names| names.iter().any(String::is_empty))
            })
        {
            return Err("Invalid visible object name".into());
        }
        Ok(())
    }
}

fn validate_default_database(name: &str) -> Result<(), String> {
    if name.is_empty() || name.trim() != name || name.contains(['\0', '/']) {
        return Err("Invalid default database name".into());
    }
    Ok(())
}

pub fn is_system_database(name: &str) -> bool {
    matches!(
        name,
        "admin"
            | "config"
            | "local"
            | "postgres"
            | "template0"
            | "template1"
            | "information_schema"
            | "mysql"
            | "performance_schema"
            | "sys"
    )
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarPreferences {
    pub groups: Vec<ConnectionGroup>,
    pub connections: Vec<ConnectionDisplay>,
    pub sort: ConnectionSort,
    pub sidebar_width: Option<f32>,
    /// The assistant dock on the right of the main area is open (closed when missing).
    pub assistant_open: bool,
    /// Width of the assistant dock; see [`Self::resolve_assistant_width`].
    pub assistant_width: Option<f32>,
    pub visible_filters: HashMap<String, VisibleObjectFilter>,
    /// Sidebar "Local search" switch, inverted so the serde/derive default
    /// (`false`) is the default of local-only search.
    pub global_search: bool,
    /// Workspace tab-bar grouping ("Group tabs by"); missing in older files.
    pub tab_group_by: TabGroupBy,
    /// "Shorten Tab Titles": tab labels drop the `@database` suffix.
    pub compact_tab_titles: bool,
    /// "Show Tabs in a Second Row": group chips on top, the active group's
    /// tabs below. Needs `tab_group_by` != none; off keeps the one-row strip.
    pub tab_rows: bool,
    /// Per-connection default database chosen from the sidebar ("Set as
    /// Default Database"). Overrides the URI path database for new queries.
    pub default_databases: HashMap<String, String>,
}

/// Sidebar geometry: default 260px, drag clamped to 180–800px, persisted on
/// drag end only.
pub const SIDEBAR_WIDTH_DEFAULT: f32 = 260.0;
pub const SIDEBAR_WIDTH_MIN: f32 = 180.0;
pub const SIDEBAR_WIDTH_MAX: f32 = 800.0;
pub const ASSISTANT_WIDTH_DEFAULT: f32 = 380.0;
pub const ASSISTANT_WIDTH_MIN: f32 = 280.0;
pub const ASSISTANT_WIDTH_MAX: f32 = 720.0;

/// Every write of the preferences file takes this turn.
static SAVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, PartialEq, Eq, Hash)]
enum Sibling {
    Group(String),
    Connection(String),
}

impl SidebarPreferences {
    pub fn default_path() -> io::Result<PathBuf> {
        crate::platform::app_data_dir()
            .map(|dir| dir.join("sidebar-preferences.json"))
            .ok_or_else(|| io::Error::other("data directory is unavailable"))
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let json = match std::fs::read(path) {
            Ok(json) => json,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error),
        };
        let preferences: Self = serde_json::from_slice(&json).map_err(io::Error::other)?;
        preferences.validate().map_err(io::Error::other)?;
        Ok(preferences)
    }

    /// Writes the file. All writes of this file in the process take turns (see
    /// [`Self::update_assistant_layout`]).
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let _turn = SAVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.save_locked(path)
    }

    /// The assistant dock's open state and width, written into whatever the file holds *now*
    /// (read, change two fields, write, all in one turn): safe to run on a background thread
    /// next to the UI thread's own saves, which never lose it and which it never undoes. A file
    /// that cannot be read is left alone.
    pub fn update_assistant_layout(path: &Path, open: bool, width: f32) -> io::Result<()> {
        let _turn = SAVE_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut preferences = Self::load(path)?;
        preferences.set_assistant_open(open);
        preferences.set_assistant_width(width);
        preferences.save_locked(path)
    }

    fn save_locked(&self, path: &Path) -> io::Result<()> {
        self.validate().map_err(io::Error::other)?;
        let data = serde_json::to_vec(self).map_err(io::Error::other)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("missing preferences directory"))?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".sidebar-preferences-{}.tmp", uuid::Uuid::new_v4()));
        let write = (|| -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&temporary, path)?;
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        write
    }

    pub fn validate(&self) -> Result<(), String> {
        for (connection, database) in &self.default_databases {
            if connection.is_empty() {
                return Err("Invalid default database connection".into());
            }
            validate_default_database(database)?;
        }
        for (connection, filter) in &self.visible_filters {
            if connection.is_empty() {
                return Err("Invalid visible filter connection".into());
            }
            filter.validate()?;
        }
        let mut groups = HashSet::new();
        for group in &self.groups {
            if group.id.is_empty()
                || group.name.trim().is_empty()
                || !groups.insert(group.id.as_str())
            {
                return Err("Invalid or duplicate connection group".into());
            }
        }
        for group in &self.groups {
            let mut path = HashSet::new();
            let mut parent = group.parent.as_deref();
            while let Some(id) = parent {
                if !path.insert(id) {
                    return Err("Connection group cycle".into());
                }
                parent = Some(
                    self.groups
                        .iter()
                        .find(|candidate| candidate.id == id)
                        .ok_or("Missing connection group parent")?,
                )
                .and_then(|candidate| candidate.parent.as_deref());
            }
        }
        let mut connections = HashSet::new();
        for display in &self.connections {
            if display.id.is_empty() || !connections.insert(display.id.as_str()) {
                return Err("Invalid or duplicate connection placement".into());
            }
            if display
                .group
                .as_ref()
                .is_some_and(|group| !groups.contains(group.as_str()))
            {
                return Err("Missing connection group".into());
            }
            if display
                .color
                .as_ref()
                .is_some_and(|color| !valid_color(color))
            {
                return Err("Invalid connection color".into());
            }
        }
        Ok(())
    }

    pub fn set_visible_filter(
        &mut self,
        connection: &str,
        filter: VisibleObjectFilter,
    ) -> Result<(), String> {
        if connection.is_empty() {
            return Err("Invalid visible filter connection".into());
        }
        filter.validate()?;
        self.visible_filters.insert(connection.into(), filter);
        Ok(())
    }

    pub fn default_database(&self, connection: &str) -> Option<&str> {
        self.default_databases.get(connection).map(String::as_str)
    }

    /// `None` clears the default.
    pub fn set_default_database(
        &mut self,
        connection: &str,
        database: Option<&str>,
    ) -> Result<(), String> {
        if connection.is_empty() {
            return Err("Invalid default database connection".into());
        }
        match database {
            None => {
                self.default_databases.remove(connection);
            }
            Some(name) => {
                validate_default_database(name)?;
                self.default_databases
                    .insert(connection.into(), name.into());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn show_all_objects(&mut self, connection: &str) {
        self.visible_filters.remove(connection);
    }

    pub fn add_group(&mut self, id: &str, name: &str, parent: Option<&str>) -> Result<(), String> {
        if id.is_empty() || name.trim().is_empty() || self.groups.iter().any(|group| group.id == id)
        {
            return Err("Invalid or duplicate connection group".into());
        }
        self.check_parent(id, parent)?;
        self.groups.push(ConnectionGroup {
            id: id.into(),
            name: name.trim().into(),
            parent: parent.map(str::to_owned),
            order: usize::MAX,
        });
        self.reorder_siblings(parent, Some(Sibling::Group(id.into())), usize::MAX);
        Ok(())
    }

    pub fn rename_group(&mut self, id: &str, name: &str) -> Result<(), String> {
        if name.trim().is_empty() {
            return Err("Connection group name is empty".into());
        }
        let group = self
            .groups
            .iter_mut()
            .find(|group| group.id == id)
            .ok_or("Missing connection group")?;
        group.name = name.trim().into();
        Ok(())
    }

    fn check_parent(&self, id: &str, parent: Option<&str>) -> Result<(), String> {
        let mut current = parent;
        let mut visited = HashSet::new();
        while let Some(value) = current {
            if value == id || !visited.insert(value) {
                return Err("Connection group cycle".into());
            }
            current = self
                .groups
                .iter()
                .find(|group| group.id == value)
                .ok_or("Missing connection group parent")?
                .parent
                .as_deref();
        }
        Ok(())
    }

    pub fn move_group(
        &mut self,
        id: &str,
        parent: Option<&str>,
        position: usize,
    ) -> Result<(), String> {
        self.check_parent(id, parent)?;
        let index = self
            .groups
            .iter()
            .position(|group| group.id == id)
            .ok_or("Missing connection group")?;
        let old_parent = self.groups[index].parent.clone();
        self.groups[index].parent = parent.map(str::to_owned);
        self.reorder_siblings(old_parent.as_deref(), None, usize::MAX);
        self.reorder_siblings(parent, Some(Sibling::Group(id.into())), position);
        Ok(())
    }

    fn reorder_siblings(&mut self, parent: Option<&str>, moved: Option<Sibling>, position: usize) {
        self.reorder_siblings_many(parent, &moved.into_iter().collect::<Vec<_>>(), position);
    }

    fn reorder_siblings_many(&mut self, parent: Option<&str>, moved: &[Sibling], position: usize) {
        let moved_set: HashSet<_> = moved.iter().collect();
        let mut siblings: Vec<_> = self
            .groups
            .iter()
            .filter(|group| group.parent.as_deref() == parent)
            .map(|group| (group.order, Sibling::Group(group.id.clone())))
            .chain(
                self.connections
                    .iter()
                    .filter(|item| item.group.as_deref() == parent)
                    .map(|item| (item.order, Sibling::Connection(item.id.clone()))),
            )
            .filter(|(_, sibling)| !moved_set.contains(sibling))
            .collect();
        siblings.sort_by_key(|(order, _)| *order);
        let mut siblings: Vec<_> = siblings.into_iter().map(|(_, sibling)| sibling).collect();
        siblings.splice(
            position.min(siblings.len())..position.min(siblings.len()),
            moved.iter().cloned(),
        );
        let group_indices: HashMap<_, _> = self
            .groups
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();
        let connection_indices: HashMap<_, _> = self
            .connections
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();
        for (order, sibling) in siblings.into_iter().enumerate() {
            match sibling {
                Sibling::Group(id) => {
                    if let Some(index) = group_indices.get(&id) {
                        self.groups[*index].order = order;
                    }
                }
                Sibling::Connection(id) => {
                    if let Some(index) = connection_indices.get(&id) {
                        self.connections[*index].order = order;
                    }
                }
            }
        }
    }

    pub fn move_connections(
        &mut self,
        all_saved: &[String],
        ids: &[String],
        group: Option<&str>,
        position: usize,
    ) -> Result<(), String> {
        if ids.is_empty() {
            return Err("No connections selected".into());
        }
        if group.is_some_and(|id| !self.groups.iter().any(|candidate| candidate.id == id)) {
            return Err("Missing connection group".into());
        }
        let saved: HashSet<_> = all_saved.iter().map(String::as_str).collect();
        let mut selected = HashSet::new();
        if ids
            .iter()
            .any(|id| !saved.contains(id.as_str()) || !selected.insert(id.as_str()))
        {
            return Err("Invalid or duplicate selected connection".into());
        }
        self.normalize_unplaced_connections(all_saved);
        let mut old_groups = HashSet::new();
        for id in ids {
            let item = self
                .connections
                .iter_mut()
                .find(|item| item.id == *id)
                .expect("normalized above");
            old_groups.insert(item.group.clone());
            item.group = group.map(str::to_owned);
        }
        for old in old_groups {
            if old.as_deref() != group {
                self.reorder_siblings(old.as_deref(), None, usize::MAX);
            }
        }
        let moved: Vec<_> = ids.iter().cloned().map(Sibling::Connection).collect();
        self.reorder_siblings_many(group, &moved, position);
        Ok(())
    }

    pub fn move_groups(
        &mut self,
        ids: &[String],
        parent: Option<&str>,
        position: usize,
    ) -> Result<(), String> {
        if ids.is_empty() {
            return Err("No groups selected".into());
        }
        let mut selected = HashSet::new();
        for id in ids {
            if !selected.insert(id.as_str()) || !self.groups.iter().any(|group| group.id == *id) {
                return Err("Invalid or duplicate selected group".into());
            }
            self.check_parent(id, parent)?;
        }
        let mut old_parents = HashSet::new();
        for id in ids {
            let group = self
                .groups
                .iter_mut()
                .find(|group| group.id == *id)
                .expect("validated above");
            old_parents.insert(group.parent.clone());
            group.parent = parent.map(str::to_owned);
        }
        for old in old_parents {
            if old.as_deref() != parent {
                self.reorder_siblings(old.as_deref(), None, usize::MAX);
            }
        }
        let moved: Vec<_> = ids.iter().cloned().map(Sibling::Group).collect();
        self.reorder_siblings_many(parent, &moved, position);
        Ok(())
    }

    pub(crate) fn normalize_unplaced_connections(&mut self, all_saved: &[String]) {
        let present: HashSet<_> = self
            .connections
            .iter()
            .map(|item| item.id.as_str())
            .collect();
        let missing: Vec<_> = all_saved
            .iter()
            .filter(|id| !present.contains(id.as_str()))
            .cloned()
            .collect();
        self.connections
            .extend(missing.into_iter().map(|id| ConnectionDisplay {
                id,
                group: None,
                order: usize::MAX,
                color: None,
                hidden: false,
            }));
        let indices: HashMap<_, _> = self
            .connections
            .iter()
            .enumerate()
            .map(|(index, item)| (item.id.clone(), index))
            .collect();
        let base = self
            .groups
            .iter()
            .filter(|group| group.parent.is_none())
            .map(|group| group.order)
            .chain(
                self.connections
                    .iter()
                    .filter(|item| item.group.is_none())
                    .map(|item| item.order),
            )
            .filter(|order| *order != usize::MAX)
            .max()
            .map_or(0, |order| order + 1);
        for (order, id) in all_saved.iter().enumerate() {
            if let Some(index) = indices.get(id.as_str()) {
                let item = &mut self.connections[*index];
                if item.group.is_none() && item.order == usize::MAX {
                    item.order = base.saturating_add(order);
                }
            }
        }
        self.reorder_siblings(None, None, usize::MAX);
    }

    pub fn move_connection(
        &mut self,
        id: &str,
        group: Option<&str>,
        position: usize,
    ) -> Result<(), String> {
        if id.is_empty() {
            return Err("Missing connection id".into());
        }
        if group.is_some_and(|id| !self.groups.iter().any(|candidate| candidate.id == id)) {
            return Err("Missing connection group".into());
        }
        let old_group = self
            .connections
            .iter()
            .find(|item| item.id == id)
            .and_then(|item| item.group.clone());
        if let Some(item) = self.connections.iter_mut().find(|item| item.id == id) {
            item.group = group.map(str::to_owned);
        } else {
            self.connections.push(ConnectionDisplay {
                id: id.into(),
                group: group.map(str::to_owned),
                order: usize::MAX,
                color: None,
                hidden: false,
            });
        }
        self.reorder_siblings(old_group.as_deref(), None, usize::MAX);
        self.reorder_siblings(group, Some(Sibling::Connection(id.into())), position);
        Ok(())
    }

    #[cfg(test)]
    pub fn set_color(&mut self, id: &str, color: Option<&str>) -> Result<(), String> {
        if color.is_some_and(|color| !valid_color(color)) {
            return Err("Invalid connection color".into());
        }
        self.display_mut(id)?.color = color.map(str::to_owned);
        Ok(())
    }

    pub fn set_hidden(&mut self, id: &str, hidden: bool) -> Result<(), String> {
        self.display_mut(id)?.hidden = hidden;
        Ok(())
    }

    /// `true` (default): search only loaded sidebar nodes; `false`: also
    /// crawl unloaded catalogs of connected connections.
    pub fn local_search(&self) -> bool {
        !self.global_search
    }

    pub fn set_local_search(&mut self, local: bool) {
        self.global_search = !local;
    }

    pub fn set_tab_group_by(&mut self, group_by: TabGroupBy) {
        self.tab_group_by = group_by;
    }

    pub fn set_compact_tab_titles(&mut self, compact: bool) {
        self.compact_tab_titles = compact;
    }

    pub fn set_tab_rows(&mut self, rows: bool) {
        self.tab_rows = rows;
    }

    /// Stores a validated sidebar width (180–800px range). Non-finite
    /// values are rejected; out-of-range values clamp on read.
    pub fn set_sidebar_width(&mut self, width: f32) {
        if width.is_finite() {
            self.sidebar_width = Some(width);
        }
    }

    /// Sidebar width in px: saved value when finite and positive, clamped to
    /// the 180–800px range, defaulting to 260px. Never NaN/infinite, so
    /// layout math downstream stays safe.
    pub fn resolve_sidebar_width(&self) -> f32 {
        match self.sidebar_width {
            Some(width) if width.is_finite() && width > 0.0 => {
                width.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX)
            }
            _ => SIDEBAR_WIDTH_DEFAULT,
        }
    }

    pub fn set_assistant_open(&mut self, open: bool) {
        self.assistant_open = open;
    }

    /// Stores a validated assistant dock width; non-finite values are rejected.
    pub fn set_assistant_width(&mut self, width: f32) {
        if width.is_finite() {
            self.assistant_width = Some(width);
        }
    }

    /// Assistant dock width in px: the saved value when finite and positive, clamped to
    /// 280-720 px, else 380 px.
    pub fn resolve_assistant_width(&self) -> f32 {
        match self.assistant_width {
            Some(width) if width.is_finite() && width > 0.0 => {
                width.clamp(ASSISTANT_WIDTH_MIN, ASSISTANT_WIDTH_MAX)
            }
            _ => ASSISTANT_WIDTH_DEFAULT,
        }
    }

    fn display_mut(&mut self, id: &str) -> Result<&mut ConnectionDisplay, String> {
        if id.is_empty() {
            return Err("Missing connection id".into());
        }
        if !self.connections.iter().any(|item| item.id == id) {
            self.connections.push(ConnectionDisplay {
                id: id.into(),
                group: None,
                order: usize::MAX,
                color: None,
                hidden: false,
            });
        }
        Ok(self
            .connections
            .iter_mut()
            .find(|item| item.id == id)
            .expect("inserted above"))
    }

    pub fn remove_group(&mut self, id: &str) -> Result<(), String> {
        let group = self
            .groups
            .iter()
            .find(|group| group.id == id)
            .ok_or("Missing connection group")?;
        let parent = group.parent.clone();
        for child in &mut self.groups {
            if child.parent.as_deref() == Some(id) {
                child.parent = parent.clone();
            }
        }
        for connection in &mut self.connections {
            if connection.group.as_deref() == Some(id) {
                connection.group = parent.clone();
            }
        }
        self.groups.retain(|group| group.id != id);
        self.reorder_siblings(parent.as_deref(), None, usize::MAX);
        Ok(())
    }

    pub fn remove_connection(&mut self, id: &str) {
        self.connections.retain(|item| item.id != id);
    }
}

fn valid_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_search_defaults_on_and_round_trips() {
        let mut prefs = SidebarPreferences::default();
        assert!(prefs.local_search());
        prefs.set_local_search(false);
        let json = serde_json::to_string(&prefs).unwrap();
        let restored: SidebarPreferences = serde_json::from_str(&json).unwrap();
        assert!(!restored.local_search());
        // Files written before the switch existed stay in local mode.
        let legacy: SidebarPreferences = serde_json::from_str("{}").unwrap();
        assert!(legacy.local_search());
    }

    #[test]
    fn tab_bar_preferences_default_off_and_round_trip_through_file() {
        let legacy: SidebarPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.tab_group_by, TabGroupBy::None);
        assert!(!legacy.compact_tab_titles);
        assert!(!legacy.tab_rows, "existing users keep the one-row strip");
        let path =
            std::env::temp_dir().join(format!("tablex-tab-prefs-{}.json", std::process::id()));
        let mut prefs = SidebarPreferences::default();
        prefs.set_tab_group_by(TabGroupBy::Database);
        prefs.set_compact_tab_titles(true);
        prefs.set_tab_rows(true);
        prefs.save(&path).unwrap();
        let restored = SidebarPreferences::load(&path).unwrap();
        assert_eq!(restored.tab_group_by, TabGroupBy::Database);
        assert!(restored.compact_tab_titles);
        assert!(restored.tab_rows);
        std::fs::remove_file(path).unwrap();
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains("\"tab_group_by\":\"database\""));
    }

    #[test]
    fn sidebar_width_defaults_clamps_and_rejects_nonfinite() {
        assert_eq!(SidebarPreferences::default().resolve_sidebar_width(), 260.0);
        for (stored, expected) in [
            (Some(260.0), 260.0),
            (Some(420.0), 420.0),
            (Some(180.0), 180.0),
            (Some(800.0), 800.0),
            (Some(100.0), 180.0),
            (Some(2000.0), 800.0),
            (Some(0.0), 260.0),
            (Some(-50.0), 260.0),
            (Some(f32::NAN), 260.0),
            (Some(f32::INFINITY), 260.0),
            (None, 260.0),
        ] {
            let prefs = SidebarPreferences {
                sidebar_width: stored,
                ..SidebarPreferences::default()
            };
            assert_eq!(prefs.resolve_sidebar_width(), expected, "stored={stored:?}");
        }
    }

    #[test]
    fn the_assistant_dock_is_closed_by_default_and_its_layout_round_trips() {
        let prefs = SidebarPreferences::default();
        assert!(!prefs.assistant_open);
        assert_eq!(prefs.resolve_assistant_width(), 380.0);
        let mut prefs = SidebarPreferences::default();
        prefs.set_assistant_open(true);
        prefs.set_assistant_width(500.0);
        prefs.set_assistant_width(f32::NAN);
        let restored: SidebarPreferences =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert!(restored.assistant_open);
        assert_eq!(restored.resolve_assistant_width(), 500.0);
        for (stored, expected) in [(100.0, 280.0), (5000.0, 720.0), (0.0, 380.0), (-3.0, 380.0)] {
            let prefs = SidebarPreferences {
                assistant_width: Some(stored),
                ..SidebarPreferences::default()
            };
            assert_eq!(prefs.resolve_assistant_width(), expected);
        }
        let legacy: SidebarPreferences = serde_json::from_str("{}").unwrap();
        assert!(!legacy.assistant_open && legacy.assistant_width.is_none());
    }

    #[test]
    fn default_database_round_trips_validates_and_clears() {
        let path =
            std::env::temp_dir().join(format!("tablex-default-db-{}.json", uuid::Uuid::new_v4()));
        let mut prefs = SidebarPreferences::default();
        assert_eq!(prefs.default_database("conn"), None);
        prefs.set_default_database("conn", Some("sales")).unwrap();
        assert!(prefs.set_default_database("conn", Some("")).is_err());
        assert!(prefs.set_default_database("conn", Some(" x")).is_err());
        assert!(prefs.set_default_database("conn", Some("a/b")).is_err());
        assert!(prefs.set_default_database("", Some("x")).is_err());
        assert_eq!(prefs.default_database("conn"), Some("sales"));
        prefs.save(&path).unwrap();
        let restored = SidebarPreferences::load(&path).unwrap();
        assert_eq!(restored.default_database("conn"), Some("sales"));
        let mut cleared = restored;
        cleared.set_default_database("conn", None).unwrap();
        assert_eq!(cleared.default_database("conn"), None);
        let legacy: SidebarPreferences = serde_json::from_str("{}").unwrap();
        assert!(legacy.default_databases.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sidebar_width_round_trips_through_file_and_ignores_missing_field() {
        let mut prefs = SidebarPreferences::default();
        prefs.set_sidebar_width(420.0);
        assert_eq!(prefs.resolve_sidebar_width(), 420.0);
        prefs.set_sidebar_width(50.0);
        assert_eq!(prefs.resolve_sidebar_width(), 180.0);
        let json = serde_json::to_string(&prefs).unwrap();
        let restored: SidebarPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.resolve_sidebar_width(), 180.0);
        let legacy: SidebarPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.sidebar_width, None);
        assert_eq!(legacy.resolve_sidebar_width(), 260.0);
    }

    #[test]
    fn visible_filter_persists_none_all_vs_explicit_empty_none_and_keeps_group_color() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("work", "Work", None).unwrap();
        prefs.move_connection("conn", Some("work"), 0).unwrap();
        prefs.set_color("conn", Some("#123ABC")).unwrap();
        assert!(!prefs.visible_filters.contains_key("conn"));
        prefs
            .set_visible_filter(
                "conn",
                VisibleObjectFilter {
                    databases: Some(Vec::new()),
                    patterns: vec!["^analytics_".into()],
                    schemas: HashMap::from([("app".into(), Some(vec!["public".into()]))]),
                    show_system: false,
                },
            )
            .unwrap();
        let restored: SidebarPreferences =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(restored.visible_filters["conn"].databases, Some(vec![]));
        assert_eq!(restored.connections[0].color.as_deref(), Some("#123ABC"));
        assert_eq!(restored.connections[0].group.as_deref(), Some("work"));
        assert_eq!(
            serde_json::from_str::<SidebarPreferences>("{}")
                .unwrap()
                .visible_filters
                .len(),
            0
        );
        let before = prefs.clone();
        assert!(prefs
            .set_visible_filter(
                "conn",
                VisibleObjectFilter {
                    patterns: vec!["[".into()],
                    ..Default::default()
                }
            )
            .is_err());
        assert_eq!(prefs, before);
    }

    #[test]
    fn groups_reject_cycles_duplicate_ids_and_invalid_parents_without_mutation() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("a", "Work", None).unwrap();
        prefs.add_group("b", "Nested", Some("a")).unwrap();
        let before = prefs.clone();
        assert!(prefs.move_group("a", Some("b"), 0).is_err());
        assert!(prefs.move_group("a", Some("missing"), 0).is_err());
        assert!(prefs.add_group("a", "Duplicate", None).is_err());
        assert_eq!(prefs, before);
    }

    #[test]
    fn group_reparent_and_connection_order_round_trip() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("a", "A", None).unwrap();
        prefs.add_group("b", "B", None).unwrap();
        prefs.move_group("b", Some("a"), 0).unwrap();
        prefs.move_connection("conn-2", Some("b"), 0).unwrap();
        prefs.move_connection("conn-1", Some("b"), 0).unwrap();
        prefs.set_color("conn-1", Some("#123ABC")).unwrap();
        prefs.set_hidden("conn-2", true).unwrap();
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(!json.contains("uri"));
        let restored: SidebarPreferences = serde_json::from_str(&json).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored, prefs);
        assert_eq!(restored.connections[1].id, "conn-1");
        let mut invalid = restored.clone();
        assert!(invalid.set_color("conn-1", Some("red")).is_err());
        assert_eq!(invalid, restored);
    }

    #[test]
    fn batch_move_normalizes_unplaced_connections_and_preserves_selected_order() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("target", "Target", None).unwrap();
        let all = vec!["a".into(), "b".into(), "c".into()];
        prefs.set_color("b", Some("#123ABC")).unwrap();
        prefs
            .move_connections(&all, &["c".into(), "a".into()], Some("target"), usize::MAX)
            .unwrap();
        assert_eq!(
            prefs
                .connections
                .iter()
                .find(|item| item.id == "b")
                .unwrap()
                .group,
            None
        );
        assert_eq!(
            prefs
                .connections
                .iter()
                .find(|item| item.id == "b")
                .unwrap()
                .color
                .as_deref(),
            Some("#123ABC")
        );
        let mut selected: Vec<_> = prefs
            .connections
            .iter()
            .filter(|item| item.group.as_deref() == Some("target"))
            .collect();
        selected.sort_by_key(|item| item.order);
        assert_eq!(
            selected
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            ["c", "a"]
        );
        let unchanged = prefs.clone();
        assert!(prefs
            .move_connections(&all, &["a".into(), "missing".into()], None, 0)
            .is_err());
        assert_eq!(prefs, unchanged);
    }

    #[test]
    fn batch_group_move_rejects_cycles_without_partial_reparenting() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("a", "A", None).unwrap();
        prefs.add_group("b", "B", None).unwrap();
        prefs.add_group("child", "Child", Some("a")).unwrap();
        let before = prefs.clone();
        assert!(prefs
            .move_groups(&["a".into(), "b".into()], Some("child"), 0)
            .is_err());
        assert_eq!(prefs, before);
        prefs.move_groups(&["b".into()], Some("a"), 0).unwrap();
        assert_eq!(
            prefs
                .groups
                .iter()
                .find(|item| item.id == "b")
                .unwrap()
                .parent
                .as_deref(),
            Some("a")
        );
        prefs.validate().unwrap();
    }

    #[test]
    fn rename_and_remove_group_reparent_children_and_connections() {
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("parent", "Parent", None).unwrap();
        prefs.add_group("child", "Child", Some("parent")).unwrap();
        prefs.move_connection("conn", Some("parent"), 0).unwrap();
        assert!(prefs.rename_group("parent", "  ").is_err());
        prefs.rename_group("parent", "Renamed").unwrap();
        assert_eq!(prefs.groups[0].name, "Renamed");
        prefs.remove_group("parent").unwrap();
        assert_eq!(prefs.groups[0].parent, None);
        assert_eq!(prefs.connections[0].group, None);
        prefs.validate().unwrap();
    }

    #[test]
    fn invalid_file_keeps_original_and_save_is_atomic() {
        let dir = std::env::temp_dir().join(format!("tablex-sidebar-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("preferences.json");
        std::fs::write(
            &path,
            r#"{"groups":[{"id":"a","name":"A","parent":"a","order":0}]}"#,
        )
        .unwrap();
        assert!(SidebarPreferences::load(&path).is_err());
        let mut prefs = SidebarPreferences::default();
        prefs.add_group("valid", "Valid", None).unwrap();
        prefs.save(&path).unwrap();
        assert_eq!(SidebarPreferences::load(&path).unwrap(), prefs);
        assert!(std::fs::read_dir(&dir).unwrap().count() == 1);
        assert!(prefs.save(&path.join("cannot-create-here.json")).is_err());
        assert_eq!(SidebarPreferences::load(&path).unwrap(), prefs);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
