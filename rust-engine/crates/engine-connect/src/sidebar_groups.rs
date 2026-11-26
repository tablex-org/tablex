//! The folders of the sidebar a connection is in, read from the app's `sidebar-preferences.json`
//! (the same file the app writes), for `list_connections`: only the group names and how they nest.
//! Anything unreadable is "no folders": the list is a convenience, never a gate.

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Deserialize)]
struct Group {
    id: String,
    name: String,
    #[serde(default)]
    parent: Option<String>,
}

#[derive(Deserialize)]
struct Placement {
    id: String,
    #[serde(default)]
    group: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Preferences {
    groups: Vec<Group>,
    connections: Vec<Placement>,
}

/// The most levels of folders followed (the app forbids cycles; a hand-edited file is not trusted).
const MAX_DEPTH: usize = 16;

/// Connection id (lower case) to its folders, outermost first, from the app's sidebar file.
pub fn folders_by_connection() -> HashMap<String, Vec<String>> {
    let Some(path) = crate::platform::app_data_dir().map(|dir| dir.join("sidebar-preferences.json")) else {
        return HashMap::new();
    };
    let Ok(bytes) = std::fs::read(path) else {
        return HashMap::new();
    };
    folders_in(&bytes)
}

/// [`folders_by_connection`] for the text of the file.
pub fn folders_in(bytes: &[u8]) -> HashMap<String, Vec<String>> {
    let preferences: Preferences = serde_json::from_slice(bytes).unwrap_or_default();
    let groups: HashMap<&str, &Group> = preferences.groups.iter().map(|group| (group.id.as_str(), group)).collect();
    let mut folders = HashMap::new();
    for placement in &preferences.connections {
        let mut path = Vec::new();
        let mut next = placement.group.as_deref();
        while let Some(group) = next.and_then(|id| groups.get(id)) {
            if path.len() >= MAX_DEPTH {
                break;
            }
            path.push(group.name.trim().chars().take(100).collect::<String>());
            next = group.parent.as_deref();
        }
        path.reverse();
        if !path.is_empty() {
            folders.insert(placement.id.to_ascii_lowercase(), path);
        }
    }
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connection_gets_the_names_of_its_folders_outermost_first() {
        let file = br#"{"groups":[{"id":"g1","name":"Work","parent":null,"order":0},{"id":"g2","name":"Shop","parent":"g1","order":0}],
            "connections":[{"id":"ABC","group":"g2","order":0},{"id":"def","group":null,"order":1},{"id":"ghi","group":"missing","order":2}],"sort":"name"}"#;
        let folders = folders_in(file);
        assert_eq!(folders.get("abc"), Some(&vec!["Work".to_owned(), "Shop".to_owned()]));
        assert!(!folders.contains_key("def") && !folders.contains_key("ghi"));
    }

    #[test]
    fn a_cycle_or_garbage_gives_a_short_path_or_none() {
        let cycle = br#"{"groups":[{"id":"a","name":"A","parent":"b"},{"id":"b","name":"B","parent":"a"}],"connections":[{"id":"x","group":"a"}]}"#;
        assert!(folders_in(cycle)["x"].len() <= MAX_DEPTH);
        assert!(folders_in(b"not json").is_empty());
        assert!(folders_in(b"[]").is_empty());
    }
}
