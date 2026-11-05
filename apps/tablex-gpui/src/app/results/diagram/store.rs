//! Persisted state of one diagram: the entity positions the user dragged and
//! the D2 annotations (logical relationships, ignored suggestions, notes,
//! groups, entity colors).
//!
//! One JSON file per diagram in `<app data dir>/diagrams/`, named after a
//! hash of the diagram key (connection id + database + schema, plus the focus
//! table for table diagrams), so two diagrams never contend for a file and a
//! write only rewrites the diagram it belongs to. The key is stored inside the
//! file too (readable, and a hash collision cannot hand out foreign data).
//!
//! Forward compatibility: every struct is `#[serde(default)]`, so a field a
//! later phase adds (logical foreign keys, notes, colors, layers, ...) is
//! missing in older files and defaults; fields this build does not know are
//! kept in `extra` and written back, so an older build never drops them. A
//! file with a newer `version` than [`SavedDiagram::VERSION`] is left alone
//! ([`StoreError::Newer`]): it is neither applied nor overwritten.
//!
//! All I/O here is blocking; callers run it on the background executor.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::annotations::{Annotations, Group, IgnoredLink, LogicalRelation, Note, Rgb, SavedColor, TableKey};
use super::layout::{DiagramLayout, DiagramPoint};
use super::model::{DiagramGraph, TableRef};
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::Change;

/// Identity of a diagram. `table` is the focus table of a table diagram and
/// `None` for a whole-schema diagram.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct DiagramKey {
    pub(in crate::app) connection: String,
    pub(in crate::app) database: String,
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: Option<String>,
}

/// A saved entity position. Entities are matched by table identity, never by
/// index, so a changed table set cannot shift positions onto other tables.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct SavedEntity {
    pub(in crate::app) schema: Option<String>,
    pub(in crate::app) table: String,
    pub(in crate::app) x: f32,
    pub(in crate::app) y: f32,
    /// Fields written by newer builds (per-entity color, note, layer, ...).
    #[serde(flatten)]
    pub(in crate::app) extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(in crate::app) struct SavedDiagram {
    pub(in crate::app) version: u32,
    pub(in crate::app) key: DiagramKey,
    /// Positions the user chose (dragged, or restored from here); entities
    /// without an entry keep their automatic place.
    pub(in crate::app) entities: Vec<SavedEntity>,
    /// D2: logical (user-declared) relationships.
    pub(in crate::app) relations: Vec<LogicalRelation>,
    /// D2: dismissed relationship suggestions.
    pub(in crate::app) ignored: Vec<IgnoredLink>,
    /// D2: sticky notes.
    pub(in crate::app) notes: Vec<Note>,
    /// D2: named, colored groups of entities.
    pub(in crate::app) groups: Vec<Group>,
    /// D2: entity header colors.
    pub(in crate::app) colors: Vec<SavedColor>,
    /// D3: structure changes not applied to the database yet (the pending
    /// change-set of this diagram).
    pub(in crate::app) draft: Vec<Change>,
    /// D3: the changes an apply was running when the document was last written (empty when
    /// none is): what is still here after a crash is checked against the catalog on load.
    pub(in crate::app) applying: Vec<Change>,
    /// D3: tables removed from this diagram (the view only).
    pub(in crate::app) removed: Vec<TableKey>,
    /// Top-level fields written by newer builds.
    #[serde(flatten)]
    pub(in crate::app) extra: BTreeMap<String, serde_json::Value>,
}

impl Default for SavedDiagram {
    fn default() -> Self {
        Self {
            version: Self::VERSION,
            key: DiagramKey::default(),
            entities: Vec::new(),
            relations: Vec::new(),
            ignored: Vec::new(),
            notes: Vec::new(),
            groups: Vec::new(),
            colors: Vec::new(),
            draft: Vec::new(),
            applying: Vec::new(),
            removed: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

impl SavedDiagram {
    /// Schema version this build reads and writes.
    pub(in crate::app) const VERSION: u32 = 1;

    /// This diagram with the positions of `graph`'s entities under `layout`.
    /// Everything else, including per-entity fields this build does not know,
    /// is carried over from `self`.
    pub(in crate::app) fn with_positions(self, graph: &DiagramGraph, layout: &DiagramLayout) -> Self {
        self.with_entities(graph, layout, |_| true)
    }

    /// Like [`Self::with_positions`] for the entities `keep` accepts only: the
    /// ones the user placed, so automatic places are never frozen into the file.
    fn with_entities(mut self, graph: &DiagramGraph, layout: &DiagramLayout, keep: impl Fn(usize) -> bool) -> Self {
        let mut previous: std::collections::HashMap<(Option<String>, String), BTreeMap<String, serde_json::Value>> =
            self.entities.drain(..).map(|e| ((e.schema, e.table), e.extra)).collect();
        self.version = Self::VERSION;
        self.entities = graph
            .entities
            .iter()
            .zip(&layout.rects)
            .enumerate()
            .filter(|(index, _)| keep(*index))
            .map(|(_, (entity, rect))| SavedEntity {
                extra: previous.remove(&(entity.table.schema.clone(), entity.table.table.clone())).unwrap_or_default(),
                schema: entity.table.schema.clone(),
                table: entity.table.table.clone(),
                x: rect.x,
                y: rect.y,
            })
            .collect();
        self
    }

    /// The whole document: the positions of the entities with `pinned[i]`
    /// and the annotations.
    pub(in crate::app) fn with_document(
        self,
        graph: &DiagramGraph,
        layout: &DiagramLayout,
        pinned: &[bool],
        annotations: &Annotations,
        draft: &Draft,
    ) -> Self {
        self.with_entities(graph, layout, |index| pinned.get(index).copied().unwrap_or(false))
            .with_annotations(annotations)
            .with_draft(draft)
    }

    /// This diagram with `annotations` in place of the saved ones.
    pub(in crate::app) fn with_annotations(mut self, annotations: &Annotations) -> Self {
        self.relations = annotations.relations.clone();
        self.ignored = annotations.ignored.clone();
        self.notes = annotations.notes.clone();
        self.groups = annotations.groups.clone();
        self.removed = annotations.removed.clone();
        self.colors = annotations
            .colors
            .iter()
            .map(|(table, color)| SavedColor { schema: table.schema.clone(), table: table.table.clone(), color: color.hex() })
            .collect();
        self
    }

    /// This diagram with `draft` as its pending structure changes.
    pub(in crate::app) fn with_draft(mut self, draft: &Draft) -> Self {
        self.draft = draft.changes.clone();
        self
    }

    /// This diagram with the changes an apply is running (`&[]`: none).
    pub(in crate::app) fn with_applying(mut self, changes: &[Change]) -> Self {
        self.applying = changes.to_vec();
        self
    }

    /// The saved pending structure changes.
    pub(in crate::app) fn draft(&self) -> Draft {
        Draft::from_changes(self.draft.clone())
    }

    /// The saved annotations. Colors that are not `#rrggbb` are dropped.
    pub(in crate::app) fn annotations(&self) -> Annotations {
        Annotations {
            relations: self.relations.clone(),
            ignored: self.ignored.clone(),
            notes: self.notes.clone(),
            groups: self.groups.clone(),
            removed: self.removed.clone(),
            colors: self
                .colors
                .iter()
                .filter_map(|c| {
                    let color = Rgb::parse(&c.color)?;
                    Some((TableKey { schema: c.schema.clone(), table: c.table.clone() }, color))
                })
                .collect(),
        }
    }

    /// A table the database renamed: its saved position follows.
    pub(in crate::app) fn rename_table(&mut self, from: &TableKey, to: &TableKey) {
        for entity in &mut self.entities {
            if entity.schema == from.schema && entity.table == from.table {
                entity.schema = to.schema.clone();
                entity.table = to.table.clone();
            }
        }
    }

    /// The same diagram without any saved position (Re-layout).
    pub(in crate::app) fn without_positions(mut self) -> Self {
        self.entities.clear();
        self
    }

    /// Nothing left worth a file: no position, no annotation, no field of a
    /// newer build.
    pub(in crate::app) fn is_blank(&self) -> bool {
        self.entities.is_empty() && self.annotations().is_empty() && self.draft.is_empty() && self.applying.is_empty() && self.extra.is_empty()
    }

    /// A new diagram for `key` holding the positions of `layout`.
    pub(in crate::app) fn capture(key: &DiagramKey, graph: &DiagramGraph, layout: &DiagramLayout) -> Self {
        Self { key: key.clone(), ..Self::default() }.with_positions(graph, layout)
    }

    /// Moves the entities of `layout` that have a saved position there.
    /// Returns how many matched; entries of tables that are gone, and
    /// non-finite coordinates, are ignored.
    pub(in crate::app) fn apply(&self, graph: &DiagramGraph, layout: &mut DiagramLayout) -> usize {
        self.apply_entities(graph, layout).len()
    }

    /// [`Self::apply`], returning the indices of the entities it moved.
    pub(in crate::app) fn apply_entities(&self, graph: &DiagramGraph, layout: &mut DiagramLayout) -> Vec<usize> {
        let saved: std::collections::HashMap<(&Option<String>, &str), DiagramPoint> = self
            .entities
            .iter()
            .filter(|e| e.x.is_finite() && e.y.is_finite())
            .map(|e| ((&e.schema, e.table.as_str()), DiagramPoint { x: e.x, y: e.y }))
            .collect();
        let mut applied = Vec::new();
        for (index, (entity, rect)) in graph.entities.iter().zip(layout.rects.iter_mut()).enumerate() {
            let TableRef { schema, table, .. } = &entity.table;
            if let Some(at) = saved.get(&(schema, table.as_str())) {
                (rect.x, rect.y) = (at.x, at.y);
                applied.push(index);
            }
        }
        applied
    }
}

/// Why a diagram file could not be read or written.
#[derive(Debug)]
pub(in crate::app) enum StoreError {
    Io(io::Error),
    /// Not valid diagram JSON (a hand edit or a torn write).
    Corrupt(serde_json::Error),
    /// Written by a newer build; left untouched.
    Newer(u32),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "{error}"),
            Self::Corrupt(error) => write!(f, "unreadable diagram layout: {error}"),
            Self::Newer(version) => write!(
                f,
                "diagram layout version {version} is newer than this build supports ({})",
                SavedDiagram::VERSION
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The directory of diagram files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct LayoutStore {
    dir: PathBuf,
}

impl LayoutStore {
    pub(in crate::app) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// `<app data dir>/diagrams`; `None` when the platform has no data dir.
    pub(in crate::app) fn default_location() -> Option<Self> {
        crate::platform::app_data_dir().map(|dir| Self::new(dir.join("diagrams")))
    }

    pub(in crate::app) fn path(&self, key: &DiagramKey) -> PathBuf {
        let identity = serde_json::to_vec(key).unwrap_or_default();
        let digest = Sha256::digest(identity);
        self.dir.join(format!("{}.json", hex::encode(&digest[..12])))
    }

    /// `Ok(None)` when nothing was saved yet.
    pub(in crate::app) fn load(&self, key: &DiagramKey) -> Result<Option<SavedDiagram>, StoreError> {
        let bytes = match std::fs::read(self.path(key)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let saved: SavedDiagram = serde_json::from_slice(&bytes).map_err(StoreError::Corrupt)?;
        if saved.version > SavedDiagram::VERSION {
            return Err(StoreError::Newer(saved.version));
        }
        // A colliding hash must not hand out another diagram's positions.
        Ok((saved.key == *key).then_some(saved))
    }

    /// Writes `diagram` through a temporary file and a rename, so a crash
    /// never leaves a torn file. Refuses to replace a newer build's file.
    pub(in crate::app) fn save(&self, key: &DiagramKey, diagram: &SavedDiagram) -> Result<(), StoreError> {
        let path = self.path(key);
        if let Err(newer @ StoreError::Newer(_)) = self.load(key) {
            return Err(newer);
        }
        let data = serde_json::to_vec_pretty(diagram).map_err(StoreError::Corrupt)?;
        std::fs::create_dir_all(&self.dir)?;
        write_atomically(&self.dir, &path, &data)?;
        Ok(())
    }

    /// Deletes the saved positions (Re-layout). A missing file is fine.
    pub(in crate::app) fn forget(&self, key: &DiagramKey) -> Result<(), StoreError> {
        match std::fs::remove_file(self.path(key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }
}

fn write_atomically(dir: &Path, path: &Path, data: &[u8]) -> io::Result<()> {
    let temporary = dir.join(format!(".diagram-{}.tmp", uuid::Uuid::new_v4()));
    let write = (|| -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write
}
