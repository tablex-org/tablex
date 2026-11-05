//! The identity of a pinned result column.
//!
//! A result may repeat a column name (`SELECT a.id, b.id ...`), so a pin is the
//! name plus the column's occurrence among the same-named columns, counted in
//! source order over the whole result (hidden columns included, so hiding one
//! never moves a pin). Unique names are occurrence 0.
//!
//! Session format: occurrence 0 is stored as the plain name, which is what
//! older snapshots hold; any other occurrence is `{"name": .., "occurrence": ..}`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "PinnedColumnRepr", into = "PinnedColumnRepr")]
pub struct PinnedColumn {
    pub name: String,
    pub occurrence: usize,
}

impl PinnedColumn {
    pub fn new(name: impl Into<String>, occurrence: usize) -> Self {
        Self { name: name.into(), occurrence }
    }

    pub fn matches(&self, name: &str, occurrence: usize) -> bool {
        self.occurrence == occurrence && self.name == name
    }
}

/// The serialized shape: a plain name (occurrence 0, the old format) or a
/// name with its occurrence.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PinnedColumnRepr {
    Name(String),
    Nth { name: String, occurrence: usize },
}

impl From<PinnedColumnRepr> for PinnedColumn {
    fn from(repr: PinnedColumnRepr) -> Self {
        match repr {
            PinnedColumnRepr::Name(name) => Self { name, occurrence: 0 },
            PinnedColumnRepr::Nth { name, occurrence } => Self { name, occurrence },
        }
    }
}

impl From<PinnedColumn> for PinnedColumnRepr {
    fn from(pin: PinnedColumn) -> Self {
        match pin.occurrence {
            0 => Self::Name(pin.name),
            occurrence => Self::Nth { name: pin.name, occurrence },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_occurrence_is_stored_as_the_plain_name() {
        let pins = vec![PinnedColumn::new("id", 0), PinnedColumn::new("id", 1)];
        let json = serde_json::to_string(&pins).unwrap();
        assert_eq!(json, r#"["id",{"name":"id","occurrence":1}]"#);
        assert_eq!(serde_json::from_str::<Vec<PinnedColumn>>(&json).unwrap(), pins);
    }

    #[test]
    fn plain_names_from_old_snapshots_are_occurrence_zero() {
        let pins: Vec<PinnedColumn> = serde_json::from_str(r#"["status","score"]"#).unwrap();
        assert_eq!(pins, [PinnedColumn::new("status", 0), PinnedColumn::new("score", 0)]);
    }
}
