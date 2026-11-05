//! D2 annotations: what the user adds to a diagram on top of the catalog.
//! Logical (virtual) relationships, notes, entity colors and groups, plus the
//! ignored relationship suggestions. Pure data (no GPUI); persisted by
//! `store.rs` next to the entity positions and never written to the database.
//!
//! Every mutation is a method on [`Annotations`]. The view clones the value
//! behind an `Arc`, mutates the clone and records `(before, after)` for undo,
//! so a step costs one small clone (a few notes, relations and groups).

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::layout::DiagramRect;
use super::model::{DiagramGraph, DiagramRelation, RelationOrigin, TableRef};

/// A table inside one diagram. The database is part of the diagram's key, so
/// schema and table identify an entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct TableKey {
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: String,
}

impl TableKey {
    pub(in crate::app) fn of(table: &TableRef) -> Self {
        Self { schema: table.schema.clone(), table: table.table.clone() }
    }
}

/// An sRGB color, `#rrggbb` in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct Rgb(pub(in crate::app) u32);

impl Rgb {
    /// Parses `#rrggbb` (the `#` is optional); anything else is `None`.
    pub(in crate::app) fn parse(text: &str) -> Option<Self> {
        let digits = text.trim().trim_start_matches('#');
        if digits.len() != 6 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(digits, 16).ok().map(Self)
    }

    pub(in crate::app) fn hex(self) -> String {
        format!("#{:06x}", self.0)
    }

    /// Channels as 0..=255.
    pub(in crate::app) fn channels(self) -> (u8, u8, u8) {
        ((self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8)
    }

    /// This color mixed over white by `amount` (0 = white, 1 = this color), as
    /// `#rrggbb`: the header tint of the fixed light export palette.
    pub(in crate::app) fn over_white(self, amount: f32) -> String {
        let mix = |c: u8| (255.0 - (255.0 - f32::from(c)) * amount).round() as u8;
        let (r, g, b) = self.channels();
        format!("#{:02x}{:02x}{:02x}", mix(r), mix(g), mix(b))
    }
}

/// The palette of the color menu and of new groups.
pub(in crate::app) const SWATCHES: [(&str, Rgb); 8] = [
    ("Blue", Rgb(0x3b82f6)),
    ("Green", Rgb(0x10b981)),
    ("Amber", Rgb(0xf59e0b)),
    ("Red", Rgb(0xef4444)),
    ("Purple", Rgb(0x8b5cf6)),
    ("Pink", Rgb(0xec4899)),
    ("Cyan", Rgb(0x06b6d4)),
    ("Lime", Rgb(0x84cc16)),
];

/// One side of a logical relationship.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct RelationEnd {
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: String,
    pub(in crate::app) columns: Vec<String>,
}

impl RelationEnd {
    pub(in crate::app) fn key(&self) -> TableKey {
        TableKey { schema: self.schema.clone(), table: self.table.clone() }
    }
}

/// A user-declared foreign key: `child.columns` reference `parent.columns`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct LogicalRelation {
    pub(in crate::app) id: u64,
    pub(in crate::app) name: String,
    pub(in crate::app) child: RelationEnd,
    pub(in crate::app) parent: RelationEnd,
    /// A child row may have no parent (parent end drawn as zero-or-one).
    pub(in crate::app) optional: bool,
    /// At most one child row per parent row (one-to-one).
    pub(in crate::app) unique: bool,
    /// Fields written by newer builds.
    #[serde(flatten)]
    pub(in crate::app) extra: BTreeMap<String, serde_json::Value>,
}

/// A relationship suggestion the user dismissed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct IgnoredLink {
    pub(in crate::app) child: RelationEnd,
    pub(in crate::app) parent: RelationEnd,
}

impl LogicalRelation {
    pub(in crate::app) fn link(&self) -> IgnoredLink {
        IgnoredLink { child: self.child.clone(), parent: self.parent.clone() }
    }

    /// Human readable cardinality of the relationship.
    pub(in crate::app) fn cardinality(&self) -> &'static str {
        cardinality_label(self.optional, self.unique)
    }
}

/// Label of a (parent optional, child unique) combination.
pub(in crate::app) fn cardinality_label(optional: bool, unique: bool) -> &'static str {
    match (unique, optional) {
        (false, false) => "Many to one",
        (false, true) => "Many to one (optional)",
        (true, false) => "One to one",
        (true, true) => "One to one (optional)",
    }
}

/// A sticky note on the canvas, in diagram units.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct Note {
    pub(in crate::app) id: u64,
    pub(in crate::app) text: String,
    pub(in crate::app) x: f32,
    pub(in crate::app) y: f32,
    pub(in crate::app) width: f32,
    pub(in crate::app) height: f32,
    #[serde(flatten)]
    pub(in crate::app) extra: BTreeMap<String, serde_json::Value>,
}

impl Note {
    pub(in crate::app) const DEFAULT_WIDTH: f32 = 200.0;
    pub(in crate::app) const DEFAULT_HEIGHT: f32 = 110.0;
    pub(in crate::app) const MIN_WIDTH: f32 = 80.0;
    pub(in crate::app) const MIN_HEIGHT: f32 = 48.0;

    /// The note's rectangle; a size from a hand-edited file is clamped.
    pub(in crate::app) fn rect(&self) -> DiagramRect {
        DiagramRect {
            x: self.x,
            y: self.y,
            width: if self.width.is_finite() { self.width.max(Self::MIN_WIDTH) } else { Self::DEFAULT_WIDTH },
            height: if self.height.is_finite() { self.height.max(Self::MIN_HEIGHT) } else { Self::DEFAULT_HEIGHT },
        }
    }
}

impl Default for Note {
    fn default() -> Self {
        Self {
            id: 0,
            text: String::new(),
            x: 0.0,
            y: 0.0,
            width: Self::DEFAULT_WIDTH,
            height: Self::DEFAULT_HEIGHT,
            extra: BTreeMap::new(),
        }
    }
}

/// A named, colored set of entities (a layer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct Group {
    pub(in crate::app) id: u64,
    pub(in crate::app) name: String,
    /// `#rrggbb`; an unreadable value falls back to the first swatch.
    pub(in crate::app) color: String,
    pub(in crate::app) members: Vec<TableKey>,
    /// Its entities (and the lines touching them) are not shown.
    pub(in crate::app) hidden: bool,
    /// Shown as one compact chip instead of its entities.
    pub(in crate::app) collapsed: bool,
    #[serde(flatten)]
    pub(in crate::app) extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Group {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            color: SWATCHES[0].1.hex(),
            members: Vec::new(),
            hidden: false,
            collapsed: false,
            extra: BTreeMap::new(),
        }
    }
}

impl Group {
    pub(in crate::app) fn rgb(&self) -> Rgb {
        Rgb::parse(&self.color).unwrap_or(SWATCHES[0].1)
    }

    /// Its entities are not drawn.
    pub(in crate::app) fn hides_members(&self) -> bool {
        self.hidden || self.collapsed
    }
}

/// An entity color as it is stored: `{schema, table, color}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct SavedColor {
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: String,
    pub(in crate::app) color: String,
}

/// Why an annotation could not be added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum AnnotationError {
    /// The same child columns already reference the same parent.
    DuplicateRelation,
    /// An end without columns.
    NoColumns,
}

impl std::fmt::Display for AnnotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateRelation => f.write_str("That relationship already exists"),
            Self::NoColumns => f.write_str("A relationship needs at least one column on each side"),
        }
    }
}

impl std::error::Error for AnnotationError {}

/// Everything the user added to one diagram.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::app) struct Annotations {
    pub(in crate::app) relations: Vec<LogicalRelation>,
    pub(in crate::app) ignored: Vec<IgnoredLink>,
    pub(in crate::app) notes: Vec<Note>,
    pub(in crate::app) groups: Vec<Group>,
    pub(in crate::app) colors: BTreeMap<TableKey, Rgb>,
    /// D3: tables taken out of this diagram ("Remove from diagram": the view
    /// only, the table stays in the database).
    pub(in crate::app) removed: Vec<TableKey>,
}

impl Annotations {
    pub(in crate::app) fn is_empty(&self) -> bool {
        self.relations.is_empty()
            && self.ignored.is_empty()
            && self.notes.is_empty()
            && self.groups.is_empty()
            && self.colors.is_empty()
            && self.removed.is_empty()
    }

    /// Takes `tables` out of the diagram (and out of their groups).
    pub(in crate::app) fn remove_tables(&mut self, tables: &[TableKey]) {
        for table in tables {
            if !self.removed.contains(table) {
                self.removed.push(table.clone());
            }
            self.leave_group(table);
        }
    }

    /// Puts every removed table back.
    pub(in crate::app) fn restore_removed(&mut self) {
        self.removed.clear();
    }

    /// A column the database renamed: the logical relationships that use it follow.
    pub(in crate::app) fn rename_column(&mut self, table: &TableKey, from: &str, to: &str) {
        for relation in &mut self.relations {
            for end in [&mut relation.child, &mut relation.parent] {
                if end.schema == table.schema && end.table == table.table {
                    for column in end.columns.iter_mut().filter(|c| c.as_str() == from) {
                        *column = to.to_owned();
                    }
                }
            }
        }
        for link in &mut self.ignored {
            for end in [&mut link.child, &mut link.parent] {
                if end.schema == table.schema && end.table == table.table {
                    for column in end.columns.iter_mut().filter(|c| c.as_str() == from) {
                        *column = to.to_owned();
                    }
                }
            }
        }
    }

    /// A table the database renamed (an applied `RenameTable`): everything
    /// keyed by the old name follows it.
    pub(in crate::app) fn rename_table(&mut self, from: &TableKey, to: &TableKey) {
        let swap = |key: &mut TableKey| {
            if key == from {
                *key = to.clone();
            }
        };
        for relation in &mut self.relations {
            for end in [&mut relation.child, &mut relation.parent] {
                if end.schema == from.schema && end.table == from.table {
                    end.schema = to.schema.clone();
                    end.table = to.table.clone();
                }
            }
        }
        for link in &mut self.ignored {
            for end in [&mut link.child, &mut link.parent] {
                if end.schema == from.schema && end.table == from.table {
                    end.schema = to.schema.clone();
                    end.table = to.table.clone();
                }
            }
        }
        for group in &mut self.groups {
            group.members.iter_mut().for_each(swap);
        }
        self.removed.iter_mut().for_each(swap);
        if let Some(color) = self.colors.remove(from) {
            self.colors.insert(to.clone(), color);
        }
    }

    /// An id no relation, note or group uses.
    pub(in crate::app) fn fresh_id(&self) -> u64 {
        let used = self
            .relations
            .iter()
            .map(|r| r.id)
            .chain(self.notes.iter().map(|n| n.id))
            .chain(self.groups.iter().map(|g| g.id));
        used.max().map_or(1, |max| max + 1)
    }

    // ---- logical relationships ----

    pub(in crate::app) fn relation(&self, id: u64) -> Option<&LogicalRelation> {
        self.relations.iter().find(|r| r.id == id)
    }

    /// Adds a relationship; the same child and parent columns twice is refused.
    pub(in crate::app) fn add_relation(
        &mut self,
        child: RelationEnd,
        parent: RelationEnd,
        optional: bool,
        unique: bool,
    ) -> Result<u64, AnnotationError> {
        if child.columns.is_empty() || parent.columns.is_empty() {
            return Err(AnnotationError::NoColumns);
        }
        if self.relations.iter().any(|r| r.child == child && r.parent == parent) {
            return Err(AnnotationError::DuplicateRelation);
        }
        let id = self.fresh_id();
        let name = format!("{}_{}_logical", child.table, child.columns.join("_"));
        self.relations.push(LogicalRelation { id, name, child, parent, optional, unique, extra: BTreeMap::new() });
        Ok(id)
    }

    /// Edits the cardinality; `false` when the relationship is gone.
    pub(in crate::app) fn set_cardinality(&mut self, id: u64, optional: bool, unique: bool) -> bool {
        let Some(relation) = self.relations.iter_mut().find(|r| r.id == id) else {
            return false;
        };
        (relation.optional, relation.unique) = (optional, unique);
        true
    }

    pub(in crate::app) fn remove_relation(&mut self, id: u64) -> bool {
        let before = self.relations.len();
        self.relations.retain(|r| r.id != id);
        self.relations.len() != before
    }

    /// Dismisses a suggestion for good.
    pub(in crate::app) fn ignore(&mut self, link: IgnoredLink) {
        if !self.ignored.contains(&link) {
            self.ignored.push(link);
        }
    }

    pub(in crate::app) fn is_ignored(&self, link: &IgnoredLink) -> bool {
        self.ignored.contains(link)
    }

    // ---- notes ----

    pub(in crate::app) fn add_note(&mut self, x: f32, y: f32) -> u64 {
        let id = self.fresh_id();
        self.notes.push(Note { id, x, y, ..Note::default() });
        id
    }

    pub(in crate::app) fn note(&self, id: u64) -> Option<&Note> {
        self.notes.iter().find(|n| n.id == id)
    }

    pub(in crate::app) fn note_mut(&mut self, id: u64) -> Option<&mut Note> {
        self.notes.iter_mut().find(|n| n.id == id)
    }

    pub(in crate::app) fn remove_note(&mut self, id: u64) -> bool {
        let before = self.notes.len();
        self.notes.retain(|n| n.id != id);
        self.notes.len() != before
    }

    // ---- colors ----

    /// Sets (or with `None` resets) the header color of one table.
    pub(in crate::app) fn set_color(&mut self, table: TableKey, color: Option<Rgb>) {
        match color {
            Some(color) => {
                self.colors.insert(table, color);
            }
            None => {
                self.colors.remove(&table);
            }
        }
    }

    // ---- groups ----

    /// A group of `members`; they leave any group they were in (a group left
    /// empty disappears).
    pub(in crate::app) fn add_group(&mut self, name: String, color: Rgb, members: Vec<TableKey>) -> u64 {
        for group in &mut self.groups {
            group.members.retain(|m| !members.contains(m));
        }
        self.groups.retain(|g| !g.members.is_empty());
        let id = self.fresh_id();
        self.groups.push(Group { id, name, color: color.hex(), members, ..Group::default() });
        id
    }

    pub(in crate::app) fn group(&self, id: u64) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }

    pub(in crate::app) fn group_mut(&mut self, id: u64) -> Option<&mut Group> {
        self.groups.iter_mut().find(|g| g.id == id)
    }

    pub(in crate::app) fn remove_group(&mut self, id: u64) -> bool {
        let before = self.groups.len();
        self.groups.retain(|g| g.id != id);
        self.groups.len() != before
    }

    /// Takes `table` out of its group (an emptied group disappears).
    pub(in crate::app) fn leave_group(&mut self, table: &TableKey) {
        for group in &mut self.groups {
            group.members.retain(|m| m != table);
        }
        self.groups.retain(|g| !g.members.is_empty());
    }

    pub(in crate::app) fn group_of(&self, table: &TableKey) -> Option<&Group> {
        self.groups.iter().find(|g| g.members.contains(table))
    }
}

// ---- the graph with the logical relationships ------------------------------------

impl DiagramGraph {
    /// This graph plus `logical`: each logical relationship whose tables and
    /// columns are in the graph and that no catalog foreign key already
    /// declares. Clones the graph only when something is added.
    pub(in crate::app) fn with_logical(&self, logical: &[LogicalRelation]) -> Self {
        let index: HashMap<(Option<&str>, &str), usize> = self
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| ((e.table.schema.as_deref(), e.table.table.as_str()), i))
            .collect();
        let columns_of = |end: &RelationEnd| -> Option<(usize, Vec<usize>)> {
            let entity = *index.get(&(end.schema.as_deref(), end.table.as_str()))?;
            let columns = end
                .columns
                .iter()
                .map(|name| self.entities[entity].columns.iter().position(|c| c.name == *name))
                .collect::<Option<Vec<usize>>>()?;
            (!columns.is_empty()).then_some((entity, columns))
        };
        let extra: Vec<DiagramRelation> = logical
            .iter()
            .filter_map(|l| {
                let ((child, child_columns), (parent, parent_columns)) = (columns_of(&l.child)?, columns_of(&l.parent)?);
                let declared = self.relations.iter().any(|r| {
                    r.child.0 == child
                        && r.parent.0 == parent
                        && r.child_columns == child_columns
                        && r.parent_columns == parent_columns
                });
                (!declared).then(|| DiagramRelation {
                    name: l.name.clone(),
                    child: super::model::EntityId(child),
                    child_columns,
                    parent: super::model::EntityId(parent),
                    parent_columns,
                    optional: l.optional,
                    unique: l.unique,
                    origin: RelationOrigin::Logical(l.id),
                })
            })
            .collect();
        let mut graph = self.clone();
        graph.relations.extend(extra);
        graph
    }
}

// ---- groups: members, visibility, region ----------------------------------------

/// Padding of a group's frame around its entities, diagram units.
pub(in crate::app) const GROUP_PADDING: f32 = 18.0;
/// Height of a group's title bar, diagram units.
pub(in crate::app) const GROUP_TITLE: f32 = 26.0;
/// Size of a collapsed group's chip, diagram units.
pub(in crate::app) const COLLAPSED_WIDTH: f32 = 200.0;
pub(in crate::app) const COLLAPSED_HEIGHT: f32 = 34.0;

/// The frame drawn for a group whose entities occupy `union`: the entities
/// plus padding and a title bar, or the chip of a collapsed group (at the
/// frame's top-left).
pub(in crate::app) fn group_frame(union: DiagramRect, collapsed: bool) -> DiagramRect {
    let (x, y) = (union.x - GROUP_PADDING, union.y - GROUP_PADDING - GROUP_TITLE);
    if collapsed {
        DiagramRect { x, y, width: COLLAPSED_WIDTH, height: COLLAPSED_HEIGHT }
    } else {
        DiagramRect {
            x,
            y,
            width: union.width + 2.0 * GROUP_PADDING,
            height: union.height + 2.0 * GROUP_PADDING + GROUP_TITLE,
        }
    }
}

/// Entity indices per group and what the groups hide, derived from the graph
/// and the groups whenever either changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::app) struct GroupIndex {
    /// `members[g]`: entity indices of `groups[g]` present in the graph.
    pub(in crate::app) members: Vec<Vec<usize>>,
    /// `hidden[i]`: entity `i` is in a hidden or collapsed group.
    pub(in crate::app) hidden: Vec<bool>,
}

impl GroupIndex {
    pub(in crate::app) fn build(graph: &DiagramGraph, groups: &[Group]) -> Self {
        let mut hidden = vec![false; graph.entities.len()];
        if groups.is_empty() {
            return Self { members: Vec::new(), hidden };
        }
        let index: HashMap<(Option<&str>, &str), usize> = graph
            .entities
            .iter()
            .enumerate()
            .map(|(i, e)| ((e.table.schema.as_deref(), e.table.table.as_str()), i))
            .collect();
        let members: Vec<Vec<usize>> = groups
            .iter()
            .map(|group| {
                group
                    .members
                    .iter()
                    .filter_map(|m| index.get(&(m.schema.as_deref(), m.table.as_str())).copied())
                    .collect()
            })
            .collect();
        for (group, ids) in groups.iter().zip(&members) {
            if group.hides_members() {
                for &i in ids {
                    hidden[i] = true;
                }
            }
        }
        Self { members, hidden }
    }

    /// Hides the entities of `removed` too.
    pub(in crate::app) fn hide(&mut self, graph: &DiagramGraph, removed: &[TableKey]) {
        for (index, entity) in graph.entities.iter().enumerate() {
            if removed.iter().any(|key| key.schema == entity.table.schema && key.table == entity.table.table) {
                self.hidden[index] = true;
            }
        }
    }

    /// Entity `i` is drawn.
    pub(in crate::app) fn shown(&self, i: usize) -> bool {
        !self.hidden.get(i).copied().unwrap_or(false)
    }
}
