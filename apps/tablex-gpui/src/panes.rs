//! Editor-area pane tree: the window manager model.
//!
//! Pure data (std + serde, no GPUI): leaves are panes (ordered tab ids plus
//! the pane's active tab), inner nodes are splits with ratios. The tree owns
//! *which tabs live in which pane, in which order*; the tabs themselves and
//! their runtimes stay in `WorkspaceState`. Every edit keeps the tree
//! normalized, so a caller never observes an empty pane (unless it is the only
//! one), a one-child split, a same-axis split nested in a split, ratios that
//! do not sum to one, or a dangling focus.
//!
//! Ids (`PaneId`, `SplitId`) are stable across edits for the lifetime of the
//! tree and are never reused, so they key per-pane view state. Tab ids are
//! opaque `u64`s from the workspace; sessions persist tab *indices* (see
//! [`PaneLayoutSpec`]) because restored tabs get fresh ids.
//!
//! Why not `gpui_component::dock::PaneTree`: it is the dock's layout algebra,
//! but it keeps the active tab as an index into a panel list that only it can
//! reorder (the workspace re-sorts a pane's tabs for pinning and grouping),
//! stores pixel sizes instead of ratios and can only be rebuilt from
//! persisted state through `PanelBuilder`/`Panel` entities. See
//! `docs/plans/2026-10-08-split-panes.md`.

use crate::sidebar_preferences::TabGroupBy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// How one pane's tab strip groups and draws its tabs. Each pane has its own;
/// a pane created by a split inherits the source pane's, and the session
/// persists them per pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StripSettings {
    /// "Group Tabs By" of this pane.
    pub group_by: TabGroupBy,
    /// Group chips on one row, the shown group's tabs on a second (only
    /// effective while `group_by` is not `None`).
    pub rows: bool,
}

/// Identity of a Chrome-style tab group. `database` is empty when grouping
/// by connection. Lives here because each pane remembers which groups it has
/// collapsed (and the session persists them per pane).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TabGroupKey {
    pub connection_id: String,
    pub database: String,
}

/// Smallest share a pane may hold of its split (the view enforces a pixel
/// minimum on top of this).
pub const MIN_RATIO: f32 = 0.05;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(u64);

impl PaneId {
    pub fn get(self) -> u64 {
        self.0
    }

    #[cfg(test)]
    pub fn for_test(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SplitId(u64);

impl SplitId {
    pub fn get(self) -> u64 {
        self.0
    }
}

/// `Row` lays children out side by side, `Column` stacks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Row,
    Column,
}

/// A side of a pane a tab can be dropped on to split it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn axis(self) -> SplitAxis {
        match self {
            Self::Left | Self::Right => SplitAxis::Row,
            Self::Top | Self::Bottom => SplitAxis::Column,
        }
    }

    /// The new pane goes before (left of / above) the target.
    fn leads(self) -> bool {
        matches!(self, Self::Left | Self::Top)
    }
}

/// Where a tab is dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneDrop {
    /// Into a pane's tab list. `index` is the position in the list *without*
    /// the dragged tab (`None` = append), like `TabMovePlan::Move`.
    Tabs { pane: PaneId, index: Option<usize> },
    /// Into a new pane created beside `pane`.
    Split { pane: PaneId, edge: Edge },
}

/// What removing a tab did to the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Removal {
    /// The pane the tab was in.
    pub pane: PaneId,
    /// The pane became empty and was removed from the tree.
    pub pane_removed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    id: PaneId,
    tabs: Vec<u64>,
    active: Option<u64>,
    /// Tab groups collapsed to their chip in this pane's strip.
    collapsed: Vec<TabGroupKey>,
    strip: StripSettings,
}

impl Pane {
    pub fn id(&self) -> PaneId {
        self.id
    }

    pub fn tabs(&self) -> &[u64] {
        &self.tabs
    }

    pub fn active(&self) -> Option<u64> {
        self.active
    }

    pub fn collapsed(&self) -> &[TabGroupKey] {
        &self.collapsed
    }

    pub fn strip(&self) -> StripSettings {
        self.strip
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    id: SplitId,
    axis: SplitAxis,
    children: Vec<Node>,
    ratios: Vec<f32>,
}

impl Split {
    pub fn id(&self) -> SplitId {
        self.id
    }

    pub fn axis(&self) -> SplitAxis {
        self.axis
    }

    pub fn children(&self) -> &[Node] {
        &self.children
    }

    /// Share of each child along the axis; sums to one.
    pub fn ratios(&self) -> &[f32] {
        &self.ratios
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Pane(Pane),
    Split(Split),
}

impl Node {
    /// Identity of the node (pane and split ids never collide), for keying
    /// per-split view state on the shape of its children.
    pub fn key(&self) -> u64 {
        match self {
            Self::Pane(pane) => pane.id.0 << 1,
            Self::Split(split) => (split.id.0 << 1) | 1,
        }
    }

    fn placeholder() -> Self {
        Self::Split(Split {
            id: SplitId(0),
            axis: SplitAxis::Row,
            children: Vec::new(),
            ratios: Vec::new(),
        })
    }

    fn pane(&self, id: PaneId) -> Option<&Pane> {
        match self {
            Self::Pane(pane) => (pane.id == id).then_some(pane),
            Self::Split(split) => split.children.iter().find_map(|child| child.pane(id)),
        }
    }

    fn pane_mut(&mut self, id: PaneId) -> Option<&mut Pane> {
        match self {
            Self::Pane(pane) => (pane.id == id).then_some(pane),
            Self::Split(split) => split
                .children
                .iter_mut()
                .find_map(|child| child.pane_mut(id)),
        }
    }

    /// The panes sitting side by side with `id` in the row-axis split that
    /// directly holds it (its other direct pane children). Empty when the pane
    /// is not a direct child of a row split.
    fn row_siblings(&self, id: PaneId, out: &mut Vec<PaneId>) -> bool {
        let Self::Split(split) = self else {
            return false;
        };
        if split.axis == SplitAxis::Row
            && split
                .children
                .iter()
                .any(|child| matches!(child, Self::Pane(pane) if pane.id == id))
        {
            out.extend(split.children.iter().filter_map(|child| match child {
                Self::Pane(pane) if pane.id != id => Some(pane.id),
                _ => None,
            }));
            return true;
        }
        split
            .children
            .iter()
            .any(|child| child.row_siblings(id, out))
    }

    fn pane_of(&self, tab: u64) -> Option<&Pane> {
        match self {
            Self::Pane(pane) => pane.tabs.contains(&tab).then_some(pane),
            Self::Split(split) => split.children.iter().find_map(|child| child.pane_of(tab)),
        }
    }

    fn split_mut(&mut self, id: SplitId) -> Option<&mut Split> {
        let Self::Split(split) = self else {
            return None;
        };
        if split.id == id {
            return Some(split);
        }
        split
            .children
            .iter_mut()
            .find_map(|child| child.split_mut(id))
    }

    fn pane_count(&self) -> usize {
        match self {
            Self::Pane(_) => 1,
            Self::Split(split) => split.children.iter().map(Self::pane_count).sum(),
        }
    }
}

/// Visual-order (left to right, top to bottom) iterator over the panes.
pub struct Panes<'a> {
    stack: Vec<&'a Node>,
}

impl<'a> Iterator for Panes<'a> {
    type Item = &'a Pane;

    fn next(&mut self) -> Option<&'a Pane> {
        while let Some(node) = self.stack.pop() {
            match node {
                Node::Pane(pane) => return Some(pane),
                Node::Split(split) => self.stack.extend(split.children.iter().rev()),
            }
        }
        None
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PaneTree {
    root: Node,
    focused: PaneId,
    maximized: Option<PaneId>,
    next_id: u64,
}

impl PaneTree {
    /// One pane holding `tabs`.
    pub fn single(tabs: Vec<u64>, active: Option<u64>) -> Self {
        let active = active
            .filter(|id| tabs.contains(id))
            .or_else(|| tabs.first().copied());
        let id = PaneId(1);
        Self {
            root: Node::Pane(Pane {
                id,
                tabs,
                active,
                collapsed: Vec::new(),
                strip: StripSettings::default(),
            }),
            focused: id,
            maximized: None,
            next_id: 1,
        }
    }

    fn alloc(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    // ---- queries ---------------------------------------------------------

    pub fn root(&self) -> &Node {
        &self.root
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    pub fn pane(&self, id: PaneId) -> Option<&Pane> {
        self.root.pane(id)
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.pane(self.focused)
    }

    pub fn panes(&self) -> Panes<'_> {
        Panes {
            stack: vec![&self.root],
        }
    }

    /// The other panes directly beside `pane` in its row-axis (side by side)
    /// split. Panes inside nested splits are not siblings; a column split
    /// has none.
    pub fn row_siblings(&self, pane: PaneId) -> Vec<PaneId> {
        let mut siblings = Vec::new();
        self.root.row_siblings(pane, &mut siblings);
        siblings
    }

    pub fn pane_count(&self) -> usize {
        self.root.pane_count()
    }

    pub fn pane_of(&self, tab: u64) -> Option<PaneId> {
        self.root.pane_of(tab).map(Pane::id)
    }

    pub fn contains_tab(&self, tab: u64) -> bool {
        self.root.pane_of(tab).is_some()
    }

    /// The pane shown alone while maximized.
    pub fn maximized(&self) -> Option<PaneId> {
        self.maximized
    }

    /// The pane after `id` in visual order, wrapping; `None` with one pane.
    pub fn other_pane(&self, id: PaneId) -> Option<PaneId> {
        self.step_from(id, 1).filter(|other| *other != id)
    }

    fn step_from(&self, id: PaneId, step: isize) -> Option<PaneId> {
        let order: Vec<PaneId> = self.panes().map(Pane::id).collect();
        let at = order.iter().position(|pane| *pane == id)?;
        let next = (at as isize + step).rem_euclid(order.len() as isize) as usize;
        order.get(next).copied()
    }

    // ---- focus -------------------------------------------------------------

    pub fn focus(&mut self, id: PaneId) -> bool {
        if self.focused == id || self.pane(id).is_none() {
            return false;
        }
        self.focused = id;
        true
    }

    /// Moves focus `step` panes through visual order, wrapping.
    pub fn focus_next(&mut self, step: isize) -> bool {
        match self.step_from(self.focused, step) {
            Some(id) => self.focus(id),
            None => false,
        }
    }

    // ---- tabs --------------------------------------------------------------

    /// Adds a tab that is not in the tree yet to `pane` (`None` = at the end),
    /// activates it and focuses the pane.
    pub fn add_tab(&mut self, pane: PaneId, tab: u64, index: Option<usize>) -> bool {
        if self.contains_tab(tab) {
            return false;
        }
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let at = index.unwrap_or(target.tabs.len()).min(target.tabs.len());
        target.tabs.insert(at, tab);
        target.active = Some(tab);
        self.focused = pane;
        true
    }

    /// Makes `tab` the active tab of its pane and focuses that pane.
    pub fn activate(&mut self, tab: u64) -> bool {
        let Some(pane) = self.pane_of(tab) else {
            return false;
        };
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let changed = target.active != Some(tab) || self.focused != pane;
        target.active = Some(tab);
        self.focused = pane;
        changed
    }

    /// Replaces a pane's tab order (e.g. after pinning or regrouping).
    /// `order` must be a permutation of the pane's tabs.
    pub fn set_order(&mut self, pane: PaneId, order: Vec<u64>) -> bool {
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let same_members = order.len() == target.tabs.len()
            && order.iter().collect::<HashSet<_>>().len() == order.len()
            && order.iter().all(|tab| target.tabs.contains(tab));
        if !same_members || target.tabs == order {
            return false;
        }
        target.tabs = order;
        true
    }

    /// Sets a pane's active tab without moving the focus.
    pub fn set_active(&mut self, pane: PaneId, tab: u64) -> bool {
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        if !target.tabs.contains(&tab) || target.active == Some(tab) {
            return false;
        }
        target.active = Some(tab);
        true
    }

    /// Collapses or expands a tab group in one pane's strip.
    pub fn set_group_collapsed(
        &mut self,
        pane: PaneId,
        key: &TabGroupKey,
        collapsed: bool,
    ) -> bool {
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let at = target.collapsed.iter().position(|other| other == key);
        match (collapsed, at) {
            (true, None) => target.collapsed.push(key.clone()),
            (false, Some(at)) => {
                target.collapsed.remove(at);
            }
            _ => return false,
        }
        true
    }

    /// Drops collapsed groups of one pane that `keep` rejects.
    pub fn retain_collapsed(&mut self, pane: PaneId, keep: impl Fn(&TabGroupKey) -> bool) {
        if let Some(target) = self.root.pane_mut(pane) {
            target.collapsed.retain(|key| keep(key));
        }
    }

    /// Expands every group of one pane (its grouping mode changed).
    pub fn clear_collapsed(&mut self, pane: PaneId) -> bool {
        self.root
            .pane_mut(pane)
            .is_some_and(|pane| !std::mem::take(&mut pane.collapsed).is_empty())
    }

    /// One pane's strip settings changed. Returns whether they differed.
    pub fn set_strip(&mut self, pane: PaneId, strip: StripSettings) -> bool {
        match self.root.pane_mut(pane) {
            Some(target) if target.strip != strip => {
                target.strip = strip;
                true
            }
            _ => false,
        }
    }

    /// Gives every pane the same strip settings (startup / preference reload).
    pub fn set_all_strips(&mut self, strip: StripSettings) {
        fn apply(node: &mut Node, strip: StripSettings) {
            match node {
                Node::Pane(pane) => pane.strip = strip,
                Node::Split(split) => split.children.iter_mut().for_each(|child| apply(child, strip)),
            }
        }
        apply(&mut self.root, strip);
    }

    /// Removes a tab from the tree. An emptied pane is removed too (unless it
    /// is the only one) and the tree collapses around it.
    pub fn remove_tab(&mut self, tab: u64) -> Option<Removal> {
        let pane = self.pane_of(tab)?;
        let pane_removed = self.take_tab(tab, pane);
        Some(Removal { pane, pane_removed })
    }

    /// Moves a tab. Returns whether the tree changed; refused (and unchanged)
    /// for unknown tabs/panes and for splitting a pane off its only tab.
    pub fn move_tab(&mut self, tab: u64, drop: PaneDrop) -> bool {
        let Some(source) = self.pane_of(tab) else {
            return false;
        };
        match drop {
            PaneDrop::Tabs { pane, index } => {
                if self.pane(pane).is_none() {
                    return false;
                }
                if source == pane {
                    return self.reorder(pane, tab, index);
                }
                self.take_tab(tab, source);
                let Some(target) = self.root.pane_mut(pane) else {
                    return false;
                };
                let at = index.unwrap_or(target.tabs.len()).min(target.tabs.len());
                target.tabs.insert(at, tab);
                target.active = Some(tab);
                self.focused = pane;
                true
            }
            PaneDrop::Split { pane, edge } => {
                let Some(target) = self.pane(pane) else {
                    return false;
                };
                if source == pane && target.tabs.len() == 1 {
                    return false;
                }
                self.take_tab(tab, source);
                let id = PaneId(self.alloc());
                let split_id = SplitId(self.alloc());
                // The new pane starts with the strip settings of the pane it
                // splits off.
                let created = Pane {
                    id,
                    tabs: vec![tab],
                    active: Some(tab),
                    collapsed: Vec::new(),
                    strip: self.pane(pane).map(Pane::strip).unwrap_or_default(),
                };
                if !place(&mut self.root, pane, edge, &mut Some(created), split_id) {
                    return false;
                }
                self.focused = id;
                true
            }
        }
    }

    fn reorder(&mut self, pane: PaneId, tab: u64, index: Option<usize>) -> bool {
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let mut order = target.tabs.clone();
        order.retain(|id| *id != tab);
        order.insert(index.unwrap_or(order.len()).min(order.len()), tab);
        if order == target.tabs {
            return false;
        }
        target.tabs = order;
        true
    }

    /// Removes `tab` from `pane`, picking the neighbouring tab as the new
    /// active one, and drops the pane when it ends up empty. Returns whether
    /// the pane was removed.
    fn take_tab(&mut self, tab: u64, pane: PaneId) -> bool {
        let Some(target) = self.root.pane_mut(pane) else {
            return false;
        };
        let Some(position) = target.tabs.iter().position(|id| *id == tab) else {
            return false;
        };
        target.tabs.remove(position);
        if target.active == Some(tab) {
            target.active = position
                .checked_sub(1)
                .and_then(|previous| target.tabs.get(previous))
                .or_else(|| target.tabs.get(position))
                .copied();
        }
        if target.tabs.is_empty() && self.pane_count() > 1 {
            self.detach_pane(pane);
            return true;
        }
        false
    }

    /// Removes a pane from the tree, collapsing the split around it and moving
    /// focus to the previous (else next) pane when it held the focus.
    fn detach_pane(&mut self, id: PaneId) {
        let order: Vec<PaneId> = self.panes().map(Pane::id).collect();
        let Some(at) = order.iter().position(|pane| *pane == id) else {
            return;
        };
        if !detach(&mut self.root, id) {
            return;
        }
        normalize(&mut self.root);
        if self.focused == id {
            self.focused = at
                .checked_sub(1)
                .and_then(|previous| order.get(previous))
                .or_else(|| order.get(at + 1))
                .copied()
                .unwrap_or(self.focused);
        }
        if self.maximized == Some(id) || self.pane_count() < 2 {
            self.maximized = None;
        }
    }

    // ---- panes -------------------------------------------------------------

    /// "Close Pane": folds the pane's tabs into its neighbour (the previous
    /// pane, else the next) and removes it. Nothing is closed. Returns the
    /// receiving pane; `None` when `pane` is the only one or unknown.
    pub fn close_pane(&mut self, pane: PaneId) -> Option<PaneId> {
        if self.pane_count() < 2 {
            return None;
        }
        let order: Vec<PaneId> = self.panes().map(Pane::id).collect();
        let at = order.iter().position(|id| *id == pane)?;
        let receiver = at
            .checked_sub(1)
            .and_then(|previous| order.get(previous))
            .or_else(|| order.get(at + 1))
            .copied()?;
        let closing = self.pane(pane)?;
        let (tabs, active) = (closing.tabs.clone(), closing.active);
        let was_focused = self.focused == pane;
        let target = self.root.pane_mut(receiver)?;
        target.tabs.extend(tabs);
        if was_focused {
            target.active = active.or(target.active);
        }
        self.detach_pane(pane);
        self.focused = receiver;
        Some(receiver)
    }

    /// Shows one pane alone, or restores the layout when it already is.
    /// Needs at least two panes.
    pub fn toggle_maximize(&mut self, pane: PaneId) -> bool {
        if self.pane_count() < 2 || self.pane(pane).is_none() {
            return false;
        }
        self.maximized = (self.maximized != Some(pane)).then_some(pane);
        self.focused = pane;
        true
    }

    /// Sets the ratios of a split (e.g. after a splitter drag). They are
    /// clamped to [`MIN_RATIO`] and normalized to sum to one.
    pub fn set_ratios(&mut self, split: SplitId, ratios: &[f32]) -> bool {
        let Some(target) = self.root.split_mut(split) else {
            return false;
        };
        if ratios.len() != target.children.len()
            || ratios
                .iter()
                .any(|ratio| !ratio.is_finite() || *ratio <= 0.0)
        {
            return false;
        }
        let mut next = ratios.to_vec();
        normalize_ratios(&mut next);
        let changed = next
            .iter()
            .zip(&target.ratios)
            .any(|(a, b)| (a - b).abs() > 1e-4);
        if changed {
            target.ratios = next;
        }
        changed
    }
}

/// Inserts `new` beside the pane `target`. Joins the parent split when it
/// already runs along the edge's axis (the target's share is halved), otherwise
/// wraps the target in a fresh two-child split.
fn place(
    node: &mut Node,
    target: PaneId,
    edge: Edge,
    new: &mut Option<Pane>,
    split_id: SplitId,
) -> bool {
    match node {
        Node::Pane(pane) if pane.id == target => {
            let Some(created) = new.take() else {
                return false;
            };
            let old = std::mem::replace(node, Node::placeholder());
            let children = if edge.leads() {
                vec![Node::Pane(created), old]
            } else {
                vec![old, Node::Pane(created)]
            };
            *node = Node::Split(Split {
                id: split_id,
                axis: edge.axis(),
                children,
                ratios: vec![0.5, 0.5],
            });
            true
        }
        Node::Pane(_) => false,
        Node::Split(split) => {
            let direct = split
                .children
                .iter()
                .position(|child| matches!(child, Node::Pane(pane) if pane.id == target));
            if let Some(at) = direct.filter(|_| split.axis == edge.axis()) {
                let Some(created) = new.take() else {
                    return false;
                };
                let half = split.ratios[at] / 2.0;
                split.ratios[at] = half;
                let insert_at = if edge.leads() { at } else { at + 1 };
                split.ratios.insert(insert_at, half);
                split.children.insert(insert_at, Node::Pane(created));
                normalize_ratios(&mut split.ratios);
                return true;
            }
            split
                .children
                .iter_mut()
                .any(|child| place(child, target, edge, new, split_id))
        }
    }
}

/// Drops the leaf `id` from its parent split (the caller normalizes).
fn detach(node: &mut Node, id: PaneId) -> bool {
    let Node::Split(split) = node else {
        return false;
    };
    if let Some(at) = split
        .children
        .iter()
        .position(|child| matches!(child, Node::Pane(pane) if pane.id == id))
    {
        split.children.remove(at);
        split.ratios.remove(at);
        normalize_ratios(&mut split.ratios);
        return true;
    }
    split.children.iter_mut().any(|child| detach(child, id))
}

/// Restores the structural invariants bottom-up: same-axis child splits are
/// spliced into their parent (ratios multiplied) and a single-child split is
/// replaced by that child.
fn normalize(node: &mut Node) {
    let Node::Split(split) = node else {
        return;
    };
    for child in &mut split.children {
        normalize(child);
    }
    let axis = split.axis;
    let children = std::mem::take(&mut split.children);
    let ratios = std::mem::take(&mut split.ratios);
    for (child, ratio) in children.into_iter().zip(ratios) {
        match child {
            Node::Split(inner) if inner.axis == axis => {
                for (grandchild, share) in inner.children.into_iter().zip(inner.ratios) {
                    split.children.push(grandchild);
                    split.ratios.push(ratio * share);
                }
            }
            other => {
                split.children.push(other);
                split.ratios.push(ratio);
            }
        }
    }
    normalize_ratios(&mut split.ratios);
    if split.children.len() == 1 {
        if let Some(only) = split.children.pop() {
            *node = only;
        }
    }
}

/// Scales `ratios` to sum to one with every share at least [`MIN_RATIO`]
/// (shares below it are pinned there and the rest scaled to fill the
/// remainder). Non-positive or non-finite input falls back to equal shares.
fn normalize_ratios(ratios: &mut [f32]) {
    let count = ratios.len();
    if count == 0 {
        return;
    }
    let total: f32 = ratios.iter().sum();
    let invalid =
        !total.is_finite() || total <= 0.0 || ratios.iter().any(|r| !r.is_finite() || *r < 0.0);
    if invalid || count as f32 * MIN_RATIO >= 1.0 {
        ratios.fill(1.0 / count as f32);
        return;
    }
    for ratio in ratios.iter_mut() {
        *ratio /= total;
    }
    let mut pinned = vec![false; count];
    loop {
        let mut newly_pinned = false;
        for (ratio, pinned) in ratios.iter_mut().zip(pinned.iter_mut()) {
            if !*pinned && *ratio < MIN_RATIO {
                *ratio = MIN_RATIO;
                *pinned = true;
                newly_pinned = true;
            }
        }
        if !newly_pinned {
            break;
        }
        let budget = 1.0 - MIN_RATIO * pinned.iter().filter(|p| **p).count() as f32;
        let free: f32 = ratios
            .iter()
            .zip(&pinned)
            .filter(|(_, pinned)| !**pinned)
            .map(|(ratio, _)| *ratio)
            .sum();
        for (ratio, _) in ratios
            .iter_mut()
            .zip(&pinned)
            .filter(|(_, pinned)| !**pinned)
        {
            *ratio *= budget / free;
        }
    }
}

// ---- session format -----------------------------------------------------------

/// Persisted pane layout. Tabs are referenced by their index in the session's
/// tab list (ids are reallocated on restore); ratios are permille so the type
/// stays `Eq` and round-trips exactly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PaneLayoutSpec {
    pub root: PaneNodeSpec,
    /// Index of the focused pane in visual order.
    pub focused: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneNodeSpec {
    Pane {
        #[serde(default)]
        tabs: Vec<usize>,
        #[serde(default)]
        active: Option<usize>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        collapsed: Vec<TabGroupKey>,
        /// Absent in sessions written before per-pane strips: the pane then
        /// takes the restore defaults (the old global preferences).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        strip: Option<StripSettings>,
    },
    Split {
        axis: SplitAxis,
        #[serde(default)]
        ratios: Vec<u16>,
        #[serde(default)]
        children: Vec<PaneNodeSpec>,
    },
}

impl Default for PaneNodeSpec {
    fn default() -> Self {
        Self::Pane {
            tabs: Vec::new(),
            active: None,
            collapsed: Vec::new(),
            strip: None,
        }
    }
}

impl PaneTree {
    /// Snapshot for the session file. `index_of` maps a tab id to its index in
    /// the persisted tab list; tabs it rejects (not persisted) are skipped.
    pub fn to_spec(&self, index_of: &impl Fn(u64) -> Option<usize>) -> PaneLayoutSpec {
        let focused = self
            .panes()
            .position(|pane| pane.id == self.focused)
            .unwrap_or(0);
        PaneLayoutSpec {
            root: node_spec(&self.root, index_of),
            focused,
        }
    }

    /// Rebuilds the tree from a saved layout. `tabs[i]` is the id of persisted
    /// tab `i` (`None` when it was dropped on restore). Unknown, duplicate and
    /// dropped tabs vanish, panes left empty vanish, and tabs the layout does
    /// not mention join the focused pane. Without a (usable) layout every tab
    /// lands in one pane, which is how sessions that predate panes load;
    /// `active` is the legacy global active tab index used in that case.
    #[cfg(test)]
    pub fn restore(
        spec: Option<&PaneLayoutSpec>,
        tabs: &[Option<u64>],
        active: Option<usize>,
    ) -> Self {
        Self::restore_with(spec, tabs, active, StripSettings::default())
    }

    /// [`restore`](Self::restore) with the strip settings of panes whose spec
    /// has none (sessions that predate per-pane strips, or no layout at all).
    pub fn restore_with(
        spec: Option<&PaneLayoutSpec>,
        tabs: &[Option<u64>],
        active: Option<usize>,
        defaults: StripSettings,
    ) -> Self {
        let mut tree = Self::restore_layout(spec, tabs, active, defaults);
        if spec.is_none() {
            tree.set_all_strips(defaults);
        }
        tree
    }

    fn restore_layout(
        spec: Option<&PaneLayoutSpec>,
        tabs: &[Option<u64>],
        active: Option<usize>,
        defaults: StripSettings,
    ) -> Self {
        let all: Vec<u64> = tabs.iter().flatten().copied().collect();
        let active_id = active.and_then(|index| tabs.get(index).copied().flatten());
        let Some(spec) = spec else {
            return Self::single(all, active_id);
        };
        let mut tree = Self {
            root: Node::placeholder(),
            focused: PaneId(0),
            maximized: None,
            next_id: 0,
        };
        let mut seen = HashSet::new();
        let Some(root) = tree.build(&spec.root, tabs, &mut seen, defaults) else {
            let mut single = Self::single(all, active_id);
            single.set_all_strips(defaults);
            return single;
        };
        tree.root = root;
        normalize(&mut tree.root);
        let order: Vec<PaneId> = tree.panes().map(Pane::id).collect();
        tree.focused = order
            .get(spec.focused)
            .or_else(|| order.first())
            .copied()
            .unwrap_or(PaneId(1));
        let leftovers: Vec<u64> = all.into_iter().filter(|id| !seen.contains(id)).collect();
        if let Some(pane) = tree.root.pane_mut(tree.focused) {
            pane.tabs.extend(leftovers);
            pane.active = pane.active.or_else(|| pane.tabs.first().copied());
        }
        tree
    }

    fn build(
        &mut self,
        spec: &PaneNodeSpec,
        tabs: &[Option<u64>],
        seen: &mut HashSet<u64>,
        defaults: StripSettings,
    ) -> Option<Node> {
        match spec {
            PaneNodeSpec::Pane {
                tabs: indices,
                active,
                collapsed,
                strip,
            } => {
                let ids: Vec<u64> = indices
                    .iter()
                    .filter_map(|index| tabs.get(*index).copied().flatten())
                    .filter(|id| seen.insert(*id))
                    .collect();
                if ids.is_empty() {
                    return None;
                }
                let active = active
                    .and_then(|index| tabs.get(index).copied().flatten())
                    .filter(|id| ids.contains(id))
                    .or_else(|| ids.first().copied());
                Some(Node::Pane(Pane {
                    id: PaneId(self.alloc()),
                    tabs: ids,
                    active,
                    collapsed: collapsed.clone(),
                    strip: strip.unwrap_or(defaults),
                }))
            }
            PaneNodeSpec::Split {
                axis,
                ratios,
                children,
            } => {
                let usable = ratios.len() == children.len();
                let mut nodes = Vec::new();
                let mut shares = Vec::new();
                for (index, child) in children.iter().enumerate() {
                    if let Some(node) = self.build(child, tabs, seen, defaults) {
                        nodes.push(node);
                        shares.push(if usable {
                            f32::from(ratios[index])
                        } else {
                            1.0
                        });
                    }
                }
                match nodes.len() {
                    0 => None,
                    1 => nodes.pop(),
                    _ => {
                        normalize_ratios(&mut shares);
                        Some(Node::Split(Split {
                            id: SplitId(self.alloc()),
                            axis: *axis,
                            children: nodes,
                            ratios: shares,
                        }))
                    }
                }
            }
        }
    }
}

fn node_spec(node: &Node, index_of: &impl Fn(u64) -> Option<usize>) -> PaneNodeSpec {
    match node {
        Node::Pane(pane) => PaneNodeSpec::Pane {
            tabs: pane.tabs.iter().filter_map(|id| index_of(*id)).collect(),
            active: pane.active.and_then(index_of),
            collapsed: pane.collapsed.clone(),
            strip: Some(pane.strip),
        },
        Node::Split(split) => PaneNodeSpec::Split {
            axis: split.axis,
            ratios: split
                .ratios
                .iter()
                .map(|ratio| (ratio * 1000.0).round() as u16)
                .collect(),
            children: split
                .children
                .iter()
                .map(|child| node_spec(child, index_of))
                .collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(tree: &PaneTree) -> Vec<Vec<u64>> {
        tree.panes().map(|pane| pane.tabs.clone()).collect()
    }

    /// Structural invariants every edit must preserve.
    fn assert_valid(tree: &PaneTree) {
        fn check(node: &Node, parent_axis: Option<SplitAxis>, single: bool) {
            match node {
                Node::Pane(pane) => {
                    assert!(single || !pane.tabs.is_empty(), "empty pane {:?}", pane.id);
                    if let Some(active) = pane.active {
                        assert!(pane.tabs.contains(&active), "active outside pane");
                    } else {
                        assert!(pane.tabs.is_empty(), "tabs without an active tab");
                    }
                }
                Node::Split(split) => {
                    assert!(split.children.len() >= 2, "split with <2 children");
                    assert_eq!(split.children.len(), split.ratios.len());
                    assert_ne!(Some(split.axis), parent_axis, "same-axis nesting");
                    let total: f32 = split.ratios.iter().sum();
                    assert!((total - 1.0).abs() < 1e-4, "ratios sum to {total}");
                    assert!(split.ratios.iter().all(|r| *r >= MIN_RATIO - 1e-6));
                    for child in &split.children {
                        check(child, Some(split.axis), single);
                    }
                }
            }
        }
        check(&tree.root, None, tree.pane_count() == 1);
        assert!(tree.pane(tree.focused).is_some(), "dangling focus");
        if let Some(max) = tree.maximized {
            assert!(tree.pane(max).is_some() && tree.pane_count() > 1);
        }
        let mut seen = HashSet::new();
        for pane in tree.panes() {
            for tab in &pane.tabs {
                assert!(seen.insert(*tab), "tab {tab} in two panes");
            }
        }
    }

    fn two_panes() -> (PaneTree, PaneId, PaneId) {
        // [1 2 3] | [4]: tab 4 split off to the right.
        let mut tree = PaneTree::single(vec![1, 2, 3, 4], Some(1));
        let left = tree.focused();
        assert!(tree.move_tab(
            4,
            PaneDrop::Split {
                pane: left,
                edge: Edge::Right
            }
        ));
        let right = tree.pane_of(4).unwrap();
        assert_valid(&tree);
        (tree, left, right)
    }

    #[test]
    fn single_pane_normalizes_its_active_tab() {
        let tree = PaneTree::single(vec![5, 6], Some(99));
        assert_eq!(tree.focused_pane().unwrap().active(), Some(5));
        assert_eq!(
            PaneTree::single(vec![], None)
                .focused_pane()
                .unwrap()
                .active(),
            None
        );
        assert_valid(&tree);
    }

    #[test]
    fn splitting_each_edge_places_the_new_pane_on_that_side() {
        for (edge, axis, new_first) in [
            (Edge::Left, SplitAxis::Row, true),
            (Edge::Right, SplitAxis::Row, false),
            (Edge::Top, SplitAxis::Column, true),
            (Edge::Bottom, SplitAxis::Column, false),
        ] {
            let mut tree = PaneTree::single(vec![1, 2], Some(1));
            let pane = tree.focused();
            assert!(tree.move_tab(2, PaneDrop::Split { pane, edge }), "{edge:?}");
            assert_valid(&tree);
            let Node::Split(split) = tree.root() else {
                panic!("root is a split for {edge:?}");
            };
            assert_eq!(split.axis(), axis);
            assert_eq!(split.ratios(), &[0.5, 0.5]);
            let expected = if new_first {
                vec![vec![2], vec![1]]
            } else {
                vec![vec![1], vec![2]]
            };
            assert_eq!(ids(&tree), expected, "{edge:?}");
            // The new pane takes the focus and its tab is active.
            assert_eq!(tree.focused_pane().unwrap().tabs(), &[2]);
            assert_eq!(tree.focused_pane().unwrap().active(), Some(2));
            // The source keeps a valid active tab.
            let source = tree.pane_of(1).unwrap();
            assert_eq!(tree.pane(source).unwrap().active(), Some(1));
        }
    }

    #[test]
    fn splitting_a_pane_off_its_only_tab_is_refused() {
        let mut tree = PaneTree::single(vec![1], Some(1));
        let before = tree.clone();
        let pane = tree.focused();
        assert!(!tree.move_tab(
            1,
            PaneDrop::Split {
                pane,
                edge: Edge::Right
            }
        ));
        assert_eq!(tree, before);
    }

    #[test]
    fn same_axis_splits_join_the_parent_and_halve_the_target_share() {
        let (mut tree, left, _right) = two_panes();
        // Split the left pane (share 0.5) again on the right: sibling, not nested.
        assert!(tree.move_tab(
            2,
            PaneDrop::Split {
                pane: left,
                edge: Edge::Right
            }
        ));
        assert_valid(&tree);
        let Node::Split(split) = tree.root() else {
            panic!("split root")
        };
        assert_eq!(split.children().len(), 3);
        assert_eq!(split.ratios(), &[0.25, 0.25, 0.5]);
        assert_eq!(ids(&tree), vec![vec![1, 3], vec![2], vec![4]]);
    }

    #[test]
    fn perpendicular_splits_nest() {
        let (mut tree, _left, right) = two_panes();
        // Tab 4 is alone in the right pane, so split a left tab below it.
        assert!(tree.move_tab(
            3,
            PaneDrop::Split {
                pane: right,
                edge: Edge::Bottom
            }
        ));
        assert_valid(&tree);
        let Node::Split(outer) = tree.root() else {
            panic!("split root")
        };
        assert_eq!(outer.axis(), SplitAxis::Row);
        let Node::Split(inner) = &outer.children()[1] else {
            panic!("nested column")
        };
        assert_eq!(inner.axis(), SplitAxis::Column);
        assert_eq!(inner.ratios(), &[0.5, 0.5]);
        assert_eq!(ids(&tree), vec![vec![1, 2], vec![4], vec![3]]);
        assert_eq!(tree.pane_count(), 3);
    }

    #[test]
    fn dropping_into_another_pane_inserts_at_the_index_and_activates() {
        let (mut tree, left, right) = two_panes();
        assert!(tree.move_tab(
            2,
            PaneDrop::Tabs {
                pane: right,
                index: Some(0)
            }
        ));
        assert_valid(&tree);
        assert_eq!(ids(&tree), vec![vec![1, 3], vec![2, 4]]);
        assert_eq!(tree.pane(right).unwrap().active(), Some(2));
        assert_eq!(tree.focused(), right);
        // Append (centre of the pane).
        assert!(tree.move_tab(
            1,
            PaneDrop::Tabs {
                pane: right,
                index: None
            }
        ));
        assert_eq!(ids(&tree), vec![vec![3], vec![2, 4, 1]]);
        assert_eq!(tree.pane(left).unwrap().active(), Some(3));
        assert_valid(&tree);
    }

    #[test]
    fn reordering_inside_a_pane_keeps_focus_and_reports_noops() {
        let (mut tree, left, _right) = two_panes();
        assert!(tree.move_tab(
            1,
            PaneDrop::Tabs {
                pane: left,
                index: Some(2)
            }
        ));
        assert_eq!(ids(&tree)[0], vec![2, 3, 1]);
        // Already there.
        assert!(!tree.move_tab(
            1,
            PaneDrop::Tabs {
                pane: left,
                index: Some(2)
            }
        ));
        assert!(!tree.move_tab(
            1,
            PaneDrop::Tabs {
                pane: left,
                index: None
            }
        ));
        assert_valid(&tree);
    }

    #[test]
    fn moving_the_last_tab_out_removes_the_pane_and_renormalizes() {
        let (mut tree, left, right) = two_panes();
        assert!(tree.move_tab(
            4,
            PaneDrop::Tabs {
                pane: left,
                index: None
            }
        ));
        assert_valid(&tree);
        assert_eq!(tree.pane_count(), 1);
        assert!(tree.pane(right).is_none());
        assert!(matches!(tree.root(), Node::Pane(_)), "single child hoisted");
        assert_eq!(tree.focused(), left);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[1, 2, 3, 4]);
    }

    #[test]
    fn removing_a_middle_pane_hands_its_share_to_the_neighbours() {
        let mut tree = PaneTree::single(vec![1, 2, 3, 4], Some(1));
        let first = tree.focused();
        assert!(tree.move_tab(
            4,
            PaneDrop::Split {
                pane: first,
                edge: Edge::Right
            }
        ));
        assert!(tree.move_tab(
            3,
            PaneDrop::Split {
                pane: first,
                edge: Edge::Right
            }
        ));
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        let id = split.id();
        assert!(tree.set_ratios(id, &[0.2, 0.3, 0.5]));
        assert_eq!(ids(&tree), vec![vec![1, 2], vec![3], vec![4]]);
        tree.remove_tab(3).unwrap();
        assert_valid(&tree);
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        let [a, b] = [split.ratios()[0], split.ratios()[1]];
        assert!((a - 0.2 / 0.7).abs() < 1e-4 && (b - 0.5 / 0.7).abs() < 1e-4);
    }

    #[test]
    fn removing_a_nested_pane_collapses_and_merges_axes() {
        let (mut tree, _left, right) = two_panes();
        assert!(tree.move_tab(
            3,
            PaneDrop::Split {
                pane: right,
                edge: Edge::Bottom
            }
        ));
        // Row[ [1 2], Column[ [4], [3] ] ]; remove 3 -> Column collapses to [4].
        let removal = tree.remove_tab(3).unwrap();
        assert!(removal.pane_removed);
        assert_valid(&tree);
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        assert_eq!(split.axis(), SplitAxis::Row);
        assert!(split.children().iter().all(|c| matches!(c, Node::Pane(_))));
        assert_eq!(ids(&tree), vec![vec![1, 2], vec![4]]);
    }

    #[test]
    fn removing_a_tab_activates_a_neighbour_and_keeps_the_last_pane() {
        let mut tree = PaneTree::single(vec![1, 2, 3], Some(2));
        let pane = tree.focused();
        tree.remove_tab(2).unwrap();
        assert_eq!(tree.pane(pane).unwrap().active(), Some(1));
        tree.remove_tab(1).unwrap();
        assert_eq!(tree.pane(pane).unwrap().active(), Some(3));
        let removal = tree.remove_tab(3).unwrap();
        assert!(!removal.pane_removed, "the only pane stays, empty");
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.pane(pane).unwrap().active(), None);
        assert_valid(&tree);
        assert!(tree.remove_tab(3).is_none());
    }

    #[test]
    fn removing_the_focused_panes_last_tab_moves_focus_to_the_previous_pane() {
        let (mut tree, left, right) = two_panes();
        tree.focus(right);
        tree.remove_tab(4).unwrap();
        assert_eq!(tree.focused(), left);
        // And forwards: first pane removed -> next pane.
        let (mut tree, left, right) = two_panes();
        tree.focus(left);
        for tab in [1, 2, 3] {
            tree.remove_tab(tab).unwrap();
        }
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.focused(), right);
        assert_valid(&tree);
    }

    #[test]
    fn focus_navigation_wraps_in_visual_order() {
        let (mut tree, left, right) = two_panes();
        let bottom_left = {
            assert!(tree.move_tab(
                3,
                PaneDrop::Split {
                    pane: left,
                    edge: Edge::Bottom
                }
            ));
            tree.pane_of(3).unwrap()
        };
        // Visual order: left-top, bottom-left, right.
        let order: Vec<PaneId> = tree.panes().map(Pane::id).collect();
        assert_eq!(order, vec![left, bottom_left, right]);
        tree.focus(left);
        assert!(tree.focus_next(1));
        assert_eq!(tree.focused(), bottom_left);
        assert!(tree.focus_next(1));
        assert!(tree.focus_next(1));
        assert_eq!(tree.focused(), left, "wraps forwards");
        assert!(tree.focus_next(-1));
        assert_eq!(tree.focused(), right, "wraps backwards");
        assert_eq!(tree.other_pane(right), Some(left));
        assert!(!tree.focus(right), "already focused");
        assert!(!PaneTree::single(vec![1], None).clone().focus_next(1));
        assert_eq!(PaneTree::single(vec![1], None).other_pane(PaneId(1)), None);
    }

    #[test]
    fn activating_a_tab_focuses_its_pane() {
        let (mut tree, left, right) = two_panes();
        tree.focus(right);
        assert!(tree.activate(2));
        assert_eq!(tree.focused(), left);
        assert_eq!(tree.pane(left).unwrap().active(), Some(2));
        assert!(!tree.activate(2), "no change");
        assert!(!tree.activate(99));
    }

    #[test]
    fn add_tab_opens_in_the_given_pane() {
        let (mut tree, left, right) = two_panes();
        assert!(tree.add_tab(right, 9, None));
        assert_eq!(tree.focused(), right);
        assert_eq!(tree.pane(right).unwrap().tabs(), &[4, 9]);
        assert_eq!(tree.pane(right).unwrap().active(), Some(9));
        assert!(!tree.add_tab(left, 9, None), "already open");
        assert!(tree.add_tab(left, 10, Some(0)));
        assert_eq!(tree.pane(left).unwrap().tabs(), &[10, 1, 2, 3]);
        assert_valid(&tree);
    }

    #[test]
    fn set_order_accepts_only_permutations() {
        let (mut tree, left, _right) = two_panes();
        assert!(tree.set_order(left, vec![3, 1, 2]));
        assert!(!tree.set_order(left, vec![3, 1, 2]), "unchanged");
        assert!(!tree.set_order(left, vec![3, 1]), "missing a tab");
        assert!(!tree.set_order(left, vec![3, 1, 4]), "foreign tab");
        assert!(!tree.set_order(left, vec![3, 3, 1]), "duplicate");
        assert_eq!(tree.pane(left).unwrap().tabs(), &[3, 1, 2]);
    }

    #[test]
    fn close_pane_folds_its_tabs_into_the_neighbour() {
        let (mut tree, left, right) = two_panes();
        tree.focus(right);
        assert_eq!(tree.close_pane(right), Some(left));
        assert_valid(&tree);
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[1, 2, 3, 4]);
        assert_eq!(
            tree.focused_pane().unwrap().active(),
            Some(4),
            "keeps what was shown"
        );
        assert_eq!(tree.close_pane(left), None, "the last pane stays");

        let (mut tree, left, right) = two_panes();
        assert_eq!(
            tree.close_pane(left),
            Some(right),
            "first pane folds forwards"
        );
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[4, 1, 2, 3]);
    }

    #[test]
    fn maximize_toggles_and_clears_when_the_layout_changes() {
        let (mut tree, left, right) = two_panes();
        assert!(tree.toggle_maximize(right));
        assert_eq!(tree.maximized(), Some(right));
        assert!(tree.toggle_maximize(right));
        assert_eq!(tree.maximized(), None);
        assert!(tree.toggle_maximize(left));
        tree.remove_tab(1).unwrap();
        tree.remove_tab(2).unwrap();
        tree.remove_tab(3).unwrap();
        assert_eq!(tree.maximized(), None, "maximized pane removed");
        assert!(!tree.toggle_maximize(right), "needs two panes");
        assert_valid(&tree);
    }

    #[test]
    fn ratios_are_clamped_normalized_and_validated() {
        let (mut tree, _left, _right) = two_panes();
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        let id = split.id();
        assert!(
            tree.set_ratios(id, &[300.0, 700.0]),
            "pixel sizes normalize"
        );
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        assert!((split.ratios()[0] - 0.3).abs() < 1e-5);
        assert!(!tree.set_ratios(id, &[300.0, 700.0]), "no change");
        assert!(tree.set_ratios(id, &[1.0, 10_000.0]));
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        assert!(split.ratios()[0] >= MIN_RATIO - 1e-6, "minimum share kept");
        assert!(!tree.set_ratios(id, &[1.0]), "wrong length");
        assert!(!tree.set_ratios(id, &[1.0, f32::NAN]));
        assert!(!tree.set_ratios(id, &[1.0, 0.0]));
        assert!(!tree.set_ratios(SplitId(999), &[1.0, 1.0]));
        assert_valid(&tree);
    }

    #[test]
    fn unknown_tabs_and_panes_leave_the_tree_untouched() {
        let (mut tree, left, _right) = two_panes();
        let before = tree.clone();
        assert!(!tree.move_tab(
            99,
            PaneDrop::Tabs {
                pane: left,
                index: None
            }
        ));
        assert!(!tree.move_tab(
            1,
            PaneDrop::Tabs {
                pane: PaneId(99),
                index: None
            }
        ));
        assert!(!tree.move_tab(
            1,
            PaneDrop::Split {
                pane: PaneId(99),
                edge: Edge::Left
            }
        ));
        assert!(!tree.focus(PaneId(99)));
        assert_eq!(tree, before);
    }

    fn key(connection: &str) -> TabGroupKey {
        TabGroupKey {
            connection_id: connection.into(),
            database: String::new(),
        }
    }

    #[test]
    fn set_active_changes_the_pane_but_not_the_focus() {
        let (mut tree, left, right) = two_panes();
        tree.focus(right);
        assert!(tree.set_active(left, 3));
        assert_eq!(tree.pane(left).unwrap().active(), Some(3));
        assert_eq!(tree.focused(), right);
        assert!(!tree.set_active(left, 3), "unchanged");
        assert!(!tree.set_active(left, 4), "not a tab of that pane");
        assert!(!tree.set_active(PaneId(99), 1));
    }

    #[test]
    fn collapsed_groups_belong_to_one_pane_and_travel_with_the_spec() {
        let (mut tree, left, right) = two_panes();
        assert!(tree.set_group_collapsed(left, &key("pg"), true));
        assert!(
            !tree.set_group_collapsed(left, &key("pg"), true),
            "already collapsed"
        );
        assert!(tree.set_group_collapsed(right, &key("my"), true));
        assert_eq!(tree.pane(left).unwrap().collapsed(), &[key("pg")]);
        assert_eq!(tree.pane(right).unwrap().collapsed(), &[key("my")]);
        // Pruning keeps what `keep` accepts.
        tree.retain_collapsed(right, |key| key.connection_id != "my");
        assert!(tree.pane(right).unwrap().collapsed().is_empty());
        assert!(tree.set_group_collapsed(right, &key("my"), true));

        let index_of = |id: u64| (1..=4).contains(&id).then(|| (id - 1) as usize);
        let spec = tree.to_spec(&index_of);
        let json = serde_json::to_string(&spec).unwrap();
        let restored = PaneTree::restore(
            Some(&serde_json::from_str(&json).unwrap()),
            &[Some(1), Some(2), Some(3), Some(4)],
            None,
        );
        let collapsed: Vec<Vec<TabGroupKey>> =
            restored.panes().map(|p| p.collapsed().to_vec()).collect();
        assert_eq!(collapsed, vec![vec![key("pg")], vec![key("my")]]);
        // Moving the pane's tabs away does not drop its collapsed set; an
        // emptied pane takes it along; regrouping clears everything.
        assert!(tree.clear_collapsed(left));
        assert!(!tree.clear_collapsed(left));
        assert!(tree.panes().filter(|pane| pane.id() == left).all(|pane| pane.collapsed().is_empty()));
        assert!(!tree.set_group_collapsed(PaneId(99), &key("pg"), true));
    }

    // ---- session ------------------------------------------------------------

    fn session_ids(count: u64) -> Vec<Option<u64>> {
        (0..count).map(|i| Some(100 + i)).collect()
    }

    #[test]
    fn spec_round_trips_through_json_with_fresh_tab_ids() {
        let mut tree = PaneTree::single(vec![10, 11, 12, 13], Some(11));
        let first = tree.focused();
        assert!(tree.move_tab(
            13,
            PaneDrop::Split {
                pane: first,
                edge: Edge::Right
            }
        ));
        let right = tree.pane_of(13).unwrap();
        assert!(tree.move_tab(
            12,
            PaneDrop::Split {
                pane: right,
                edge: Edge::Bottom
            }
        ));
        let Node::Split(outer) = tree.root() else {
            panic!()
        };
        let outer_id = outer.id();
        assert!(tree.set_ratios(outer_id, &[0.4, 0.6]));
        tree.focus(tree.pane_of(12).unwrap());

        // Persisted order: 10 11 12 13 -> indices 0..4, restored as 100..104.
        let index_of = |id: u64| (10..14).contains(&id).then(|| (id - 10) as usize);
        let spec = tree.to_spec(&index_of);
        let json = serde_json::to_string(&spec).unwrap();
        let loaded: PaneLayoutSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, spec);

        let restored = PaneTree::restore(Some(&loaded), &session_ids(4), Some(1));
        assert_valid(&restored);
        assert_eq!(ids(&restored), vec![vec![100, 101], vec![103], vec![102]]);
        assert_eq!(restored.focused_pane().unwrap().tabs(), &[102]);
        assert_eq!(
            restored
                .pane(restored.pane_of(101).unwrap())
                .unwrap()
                .active(),
            Some(101)
        );
        let Node::Split(outer) = restored.root() else {
            panic!()
        };
        assert!((outer.ratios()[0] - 0.4).abs() < 1e-3);
        let Node::Split(inner) = &outer.children()[1] else {
            panic!()
        };
        assert_eq!(inner.axis(), SplitAxis::Column);
    }

    #[test]
    fn sessions_without_a_layout_load_as_one_pane() {
        let tabs = vec![Some(7), None, Some(9)];
        let tree = PaneTree::restore(None, &tabs, Some(2));
        assert_valid(&tree);
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[7, 9]);
        assert_eq!(tree.focused_pane().unwrap().active(), Some(9));
        // A legacy active index of a dropped tab falls back to the first tab.
        assert_eq!(
            PaneTree::restore(None, &tabs, Some(1))
                .focused_pane()
                .unwrap()
                .active(),
            Some(7)
        );
        // An old session file has no `panes` key at all.
        let old: Option<PaneLayoutSpec> = serde_json::from_str("null").unwrap();
        assert!(old.is_none());
    }

    #[test]
    fn restore_repairs_unusable_layouts() {
        let tabs = session_ids(4);
        let pane = |tabs: Vec<usize>, active| PaneNodeSpec::Pane {
            tabs,
            active,
            collapsed: vec![],
            strip: None,
        };
        let spec = PaneLayoutSpec {
            root: PaneNodeSpec::Split {
                axis: SplitAxis::Row,
                // Wrong ratio count -> equal shares.
                ratios: vec![900],
                children: vec![
                    pane(vec![0, 0, 9], Some(0)), // duplicate + out of range
                    pane(vec![], None),           // empty -> dropped
                    pane(vec![1], Some(3)),       // active not in pane -> first tab
                ],
            },
            focused: 7, // out of range -> first pane
        };
        let tree = PaneTree::restore(Some(&spec), &tabs, None);
        assert_valid(&tree);
        // Tabs 2 and 3 are not mentioned: they join the focused (first) pane.
        assert_eq!(ids(&tree), vec![vec![100, 102, 103], vec![101]]);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[100, 102, 103]);
        let Node::Split(split) = tree.root() else {
            panic!()
        };
        assert_eq!(split.ratios(), &[0.5, 0.5]);
        assert_eq!(
            tree.pane(tree.pane_of(101).unwrap()).unwrap().active(),
            Some(101)
        );

        // Nothing usable at all -> one pane with every restored tab.
        let nothing = PaneLayoutSpec::default();
        let tree = PaneTree::restore(Some(&nothing), &tabs, Some(3));
        assert_eq!(tree.pane_count(), 1);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[100, 101, 102, 103]);
        assert_eq!(tree.focused_pane().unwrap().active(), Some(103));
    }

    #[test]
    fn restore_skips_tabs_that_were_not_kept_and_flattens_same_axis_splits() {
        let tabs = vec![Some(1), None, Some(3)];
        let leaf = |index| PaneNodeSpec::Pane {
            tabs: vec![index],
            active: None,
            collapsed: vec![],
            strip: None,
        };
        let spec = PaneLayoutSpec {
            root: PaneNodeSpec::Split {
                axis: SplitAxis::Row,
                ratios: vec![500, 500],
                children: vec![
                    leaf(0),
                    PaneNodeSpec::Split {
                        axis: SplitAxis::Row,
                        ratios: vec![500, 500],
                        children: vec![leaf(1), leaf(2)], // tab 1 dropped
                    },
                ],
            },
            focused: 1,
        };
        let tree = PaneTree::restore(Some(&spec), &tabs, None);
        assert_valid(&tree);
        assert_eq!(ids(&tree), vec![vec![1], vec![3]]);
        assert_eq!(tree.focused_pane().unwrap().tabs(), &[3]);
    }

    #[test]
    fn to_spec_skips_tabs_that_are_not_persisted() {
        let (tree, _left, _right) = two_panes();
        // Tab 2 is a utility tab (e.g. Driver Manager): not in the session.
        let index_of = |id: u64| match id {
            1 => Some(0),
            3 => Some(1),
            4 => Some(2),
            _ => None,
        };
        let spec = tree.to_spec(&index_of);
        let PaneNodeSpec::Split {
            children, ratios, ..
        } = &spec.root
        else {
            panic!()
        };
        assert_eq!(ratios, &[500, 500]);
        assert_eq!(
            children[0],
            PaneNodeSpec::Pane {
                tabs: vec![0, 1],
                active: Some(0),
                collapsed: vec![],
                strip: Some(StripSettings::default()),
            }
        );
        assert_eq!(
            children[1],
            PaneNodeSpec::Pane {
                tabs: vec![2],
                active: Some(2),
                collapsed: vec![],
                strip: Some(StripSettings::default()),
            }
        );
    }

    #[test]
    fn spec_json_shape_is_stable() {
        let spec = PaneLayoutSpec {
            root: PaneNodeSpec::Split {
                axis: SplitAxis::Row,
                ratios: vec![400, 600],
                children: vec![
                    PaneNodeSpec::Pane {
                        tabs: vec![0, 1],
                        active: Some(1),
                        collapsed: vec![],
                        strip: None,
                    },
                    PaneNodeSpec::Pane {
                        tabs: vec![2],
                        active: Some(2),
                        collapsed: vec![],
                        strip: None,
                    },
                ],
            },
            focused: 1,
        };
        assert_eq!(
            serde_json::to_string(&spec).unwrap(),
            r#"{"root":{"split":{"axis":"row","ratios":[400,600],"children":[{"pane":{"tabs":[0,1],"active":1}},{"pane":{"tabs":[2],"active":2}}]}},"focused":1}"#
        );
        // Missing optional keys load (forward compatibility).
        let sparse: PaneLayoutSpec =
            serde_json::from_str(r#"{"root":{"pane":{"tabs":[0]}}}"#).unwrap();
        assert_eq!(
            sparse.root,
            PaneNodeSpec::Pane {
                tabs: vec![0],
                active: None,
                collapsed: vec![],
                strip: None,
            }
        );
        assert_eq!(sparse.focused, 0);
    }

    /// A random sequence of edits never breaks the invariants.
    #[test]
    fn random_edit_sequences_preserve_invariants() {
        let mut tree = PaneTree::single((1..=8).collect(), Some(1));
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let edges = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];
        for _ in 0..400 {
            let panes: Vec<PaneId> = tree.panes().map(Pane::id).collect();
            let pane = panes[(next() % panes.len() as u64) as usize];
            let tab = 1 + next() % 8;
            match next() % 5 {
                0 => {
                    tree.move_tab(
                        tab,
                        PaneDrop::Split {
                            pane,
                            edge: edges[(next() % 4) as usize],
                        },
                    );
                }
                1 => {
                    tree.move_tab(
                        tab,
                        PaneDrop::Tabs {
                            pane,
                            index: Some((next() % 4) as usize),
                        },
                    );
                }
                2 => {
                    tree.close_pane(pane);
                }
                3 => {
                    tree.focus_next(1);
                }
                _ => {
                    tree.toggle_maximize(pane);
                }
            }
            assert_valid(&tree);
            assert_eq!(tree.panes().map(|p| p.tabs.len()).sum::<usize>(), 8);
        }
    }
    #[test]
    fn strip_settings_are_per_pane_inherited_by_splits_and_default_when_missing_in_a_spec() {
        use crate::sidebar_preferences::TabGroupBy;
        let mut tree = PaneTree::single(vec![1, 2, 3], Some(1));
        let first = tree.focused();
        let database = StripSettings { group_by: TabGroupBy::Database, rows: true };
        assert!(tree.set_strip(first, database));
        assert!(!tree.set_strip(first, database), "unchanged");
        assert!(tree.move_tab(2, PaneDrop::Split { pane: first, edge: Edge::Right }));
        let second = tree.focused();
        assert_ne!(first, second);
        assert_eq!(tree.pane(second).unwrap().strip(), database, "inherited");
        // Independent afterwards.
        let none = StripSettings::default();
        assert!(tree.set_strip(second, none));
        assert_eq!(tree.pane(first).unwrap().strip(), database);

        // The spec round-trips both; a spec without the key takes the defaults.
        let spec = tree.to_spec(&|id| Some(id as usize - 1));
        let ids: Vec<Option<u64>> = vec![Some(1), Some(2), Some(3)];
        let restored = PaneTree::restore(Some(&spec), &ids, None);
        let strips: Vec<_> = restored.panes().map(|pane| pane.strip()).collect();
        assert_eq!(strips, vec![database, none]);
        let mut json = serde_json::to_value(&spec).unwrap();
        fn drop_strip(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::Object(map) => {
                    map.remove("strip");
                    map.values_mut().for_each(drop_strip);
                }
                serde_json::Value::Array(items) => items.iter_mut().for_each(drop_strip),
                _ => {}
            }
        }
        drop_strip(&mut json);
        let old: PaneLayoutSpec = serde_json::from_value(json).unwrap();
        let migrated = PaneTree::restore_with(Some(&old), &ids, None, database);
        assert!(migrated.panes().all(|pane| pane.strip() == database));
        assert!(PaneTree::restore_with(None, &ids, None, database)
            .panes()
            .all(|pane| pane.strip() == database));
    }
    #[test]
    fn row_siblings_are_the_direct_panes_of_a_side_by_side_split_only() {
        let mut tree = PaneTree::single(vec![1, 2, 3, 4], Some(1));
        let a = tree.focused();
        assert!(tree.row_siblings(a).is_empty(), "alone");
        assert!(tree.move_tab(2, PaneDrop::Split { pane: a, edge: Edge::Right }));
        let b = tree.focused();
        assert_eq!(tree.row_siblings(a), vec![b]);
        assert_eq!(tree.row_siblings(b), vec![a]);
        // A column split inside the right pane: its panes are not siblings of
        // the left pane's row, nor of each other.
        assert!(tree.move_tab(3, PaneDrop::Tabs { pane: b, index: None }));
        assert!(tree.move_tab(4, PaneDrop::Split { pane: b, edge: Edge::Bottom }));
        let c = tree.focused();
        assert!(tree.row_siblings(b).is_empty());
        assert!(tree.row_siblings(c).is_empty());
        assert!(tree.row_siblings(a).is_empty(), "the other child is a split, not a pane");
    }
}
