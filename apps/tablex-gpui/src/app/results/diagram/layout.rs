//! Pure layout, routing and viewport math (no GPUI types except plain
//! numbers). Owner: DG-M.

use std::collections::HashMap;

use super::model::{DiagramColumn, DiagramGraph, EntityId};

/// Which columns an entity box shows (attribute visibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum AttributeMode {
    #[default]
    All,
    /// Primary-key and foreign-key columns only.
    KeysOnly,
    /// Primary-key columns only.
    PrimaryKeyOnly,
    /// Header only.
    None,
}

impl AttributeMode {
    pub(in crate::app) const ALL: [Self; 4] = [Self::All, Self::KeysOnly, Self::PrimaryKeyOnly, Self::None];

    pub(in crate::app) fn label(self) -> &'static str {
        match self {
            Self::All => "All columns",
            Self::KeysOnly => "Keys only",
            Self::PrimaryKeyOnly => "Primary key only",
            Self::None => "No columns",
        }
    }

    /// Whether `column` is drawn in this mode.
    pub(in crate::app) fn shows(self, column: &DiagramColumn) -> bool {
        match self {
            Self::All => true,
            Self::KeysOnly => column.primary_key || column.foreign_key,
            Self::PrimaryKeyOnly => column.primary_key,
            Self::None => false,
        }
    }
}

/// Box geometry constants in diagram units (= px at zoom 1.0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::app) struct Metrics {
    pub(in crate::app) header_height: f32,
    pub(in crate::app) row_height: f32,
    /// Extra height of the separator under the primary-key block.
    pub(in crate::app) separator_height: f32,
    pub(in crate::app) min_width: f32,
    pub(in crate::app) max_width: f32,
    /// Approximate glyph advance used to size boxes from text length.
    pub(in crate::app) char_width: f32,
    pub(in crate::app) column_gap: f32,
    pub(in crate::app) row_gap: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(in crate::app) struct DiagramRect {
    pub(in crate::app) x: f32,
    pub(in crate::app) y: f32,
    pub(in crate::app) width: f32,
    pub(in crate::app) height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(in crate::app) struct DiagramPoint {
    pub(in crate::app) x: f32,
    pub(in crate::app) y: f32,
}

/// One routed relation: an orthogonal polyline from the child's FK row edge
/// (first point) to the parent's PK row edge (last point).
#[derive(Debug, Clone, PartialEq)]
pub(in crate::app) struct RoutedRelation {
    pub(in crate::app) relation: usize,
    pub(in crate::app) points: Vec<DiagramPoint>,
}

/// Entity positions, indexed by `EntityId.0`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::app) struct DiagramLayout {
    pub(in crate::app) rects: Vec<DiagramRect>,
}

/// Horizontal distance between the vertical segments of parallel relations.
const PARALLEL_STEP: f32 = 6.0;

/// Which of the three layout columns an entity belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Focus,
    /// Children (tables referencing the focus).
    Left,
    /// Parents (tables the focus references); wins over `Left`.
    Right,
}

impl Role {
    /// One role per entity; entity 0 is the focus.
    fn assign(graph: &DiagramGraph) -> Vec<Role> {
        let mut roles = vec![Role::Left; graph.entities.len()];
        if let Some(focus) = roles.first_mut() {
            *focus = Role::Focus;
        }
        for r in graph
            .relations
            .iter()
            .filter(|r| r.child.0 == 0 && r.parent.0 != 0)
        {
            roles[r.parent.0] = Role::Right;
        }
        roles
    }
}

/// Screen-space margin (px) added around the viewport when culling.
const CULL_MARGIN: f32 = 32.0;
/// Width : height ratio the schema packer aims for.
const SCHEMA_ASPECT: f32 = 1.6;
/// Barycentre sweeps (alternating down / up) per component.
const SCHEMA_SWEEPS: usize = 4;

/// Longest-path ranks over `child -> parents` edges: every parent ranks above
/// its children. Cycles are broken by ignoring DFS back edges.
fn schema_ranks(parents_of: &[Vec<usize>]) -> Vec<usize> {
    let n = parents_of.len();
    let mut state = vec![0_u8; n];
    let mut finish = vec![0_usize; n];
    let mut order = Vec::with_capacity(n);
    let mut stack: Vec<(usize, usize)> = Vec::new();
    for root in 0..n {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        stack.push((root, 0));
        while let Some(top) = stack.last_mut() {
            let u = top.0;
            if let Some(&v) = parents_of[u].get(top.1) {
                top.1 += 1;
                if state[v] == 0 {
                    state[v] = 1;
                    stack.push((v, 0));
                }
            } else {
                state[u] = 2;
                finish[u] = order.len();
                order.push(u);
                stack.pop();
            }
        }
    }
    // Reverse finish order visits every node before its non-back-edge parents.
    let mut ranks = vec![0_usize; n];
    for &u in order.iter().rev() {
        for &v in parents_of[u].iter().filter(|&&v| finish[u] > finish[v]) {
            ranks[v] = ranks[v].max(ranks[u] + 1);
        }
    }
    ranks
}

/// Connected components over `neighbours`, members ascending, components
/// ordered by their lowest member.
fn connected_components(neighbours: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; neighbours.len()];
    let mut components = Vec::new();
    for root in 0..neighbours.len() {
        if seen[root] {
            continue;
        }
        seen[root] = true;
        let mut members = vec![root];
        let mut next = 0;
        while let Some(&u) = members.get(next) {
            next += 1;
            for &v in &neighbours[u] {
                if !std::mem::replace(&mut seen[v], true) {
                    members.push(v);
                }
            }
        }
        members.sort_unstable();
        components.push(members);
    }
    components
}

/// One laid-out connected component, relative to its own top-left corner.
struct Block {
    members: Vec<usize>,
    /// Top-left of `members[i]` inside the block.
    offsets: Vec<DiagramPoint>,
    width: f32,
    height: f32,
}

impl Block {
    fn layered(
        members: Vec<usize>,
        ranks: &[usize],
        neighbours: &[Vec<usize>],
        sizes: &[(f32, f32)],
        metrics: &Metrics,
    ) -> Self {
        let depth = members.iter().map(|&m| ranks[m]).max().map_or(0, |r| r + 1);
        let mut layers: Vec<Vec<usize>> = vec![Vec::new(); depth];
        for &m in &members {
            layers[ranks[m]].push(m);
        }
        let mut position = vec![0_usize; ranks.len()];
        for layer in &layers {
            for (i, &m) in layer.iter().enumerate() {
                position[m] = i;
            }
        }
        for sweep in 0..SCHEMA_SWEEPS {
            let down = sweep % 2 == 0;
            let order: Vec<usize> = if down {
                (1..depth).collect()
            } else {
                (0..depth.saturating_sub(1)).rev().collect()
            };
            for r in order {
                let key = |m: usize| -> f32 {
                    let (sum, count) = neighbours[m]
                        .iter()
                        .filter(|&&nb| if down { ranks[nb] < r } else { ranks[nb] > r })
                        .fold((0.0_f32, 0_u32), |(sum, count), &nb| {
                            let len = layers[ranks[nb]].len() as f32;
                            (sum + (position[nb] as f32 + 0.5) / len, count + 1)
                        });
                    if count == 0 {
                        (position[m] as f32 + 0.5) / layers[r].len() as f32
                    } else {
                        sum / count as f32
                    }
                };
                let mut keyed: Vec<(f32, usize)> = layers[r].iter().map(|&m| (key(m), m)).collect();
                keyed.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                for (i, (_, m)) in keyed.into_iter().enumerate() {
                    layers[r][i] = m;
                    position[m] = i;
                }
            }
        }

        let layer_width = |l: &Vec<usize>| l.iter().map(|&m| sizes[m].0).fold(0.0_f32, f32::max);
        let layer_height = |l: &Vec<usize>| {
            l.iter().map(|&m| sizes[m].1).sum::<f32>()
                + metrics.row_gap * l.len().saturating_sub(1) as f32
        };
        let height = layers.iter().map(layer_height).fold(0.0_f32, f32::max);
        let mut offsets = vec![DiagramPoint::default(); ranks.len()];
        let mut x = 0.0_f32;
        for layer in &layers {
            let mut y = (height - layer_height(layer)) / 2.0;
            for &m in layer {
                offsets[m] = DiagramPoint { x, y };
                y += sizes[m].1 + metrics.row_gap;
            }
            x += layer_width(layer) + metrics.column_gap;
        }
        Self {
            offsets: members.iter().map(|&m| offsets[m]).collect(),
            members,
            width: (x - metrics.column_gap).max(0.0),
            height,
        }
    }
}

/// Largest distance between neighbouring vertical channels in one gap.
const CHANNEL_MAX_STEP: f32 = 24.0;

/// Horizontal window in which a relation's vertical segment may run.
#[derive(Debug, Clone, Copy, Default)]
struct Lane {
    lo: f32,
    hi: f32,
    child_on_left: bool,
}

struct PendingRoute {
    relation: usize,
    start: DiagramPoint,
    end: DiagramPoint,
    mid_x: f32,
    lane: Option<Lane>,
}

/// Gives every laned route its own vertical x. Routes whose windows overlap
/// (same direction) share a gap; its channels are spread evenly inside the
/// common window, at most `CHANNEL_MAX_STEP` apart and centred. Ordering by
/// source then target y, nearest channel to the child first for the side the
/// lines mostly run towards, keeps crossings low. Fully deterministic.
fn assign_channels(pending: &mut [PendingRoute]) {
    let lane = |p: &PendingRoute| p.lane.unwrap_or_default();
    let mut laned: Vec<usize> = (0..pending.len())
        .filter(|&i| pending[i].lane.is_some())
        .collect();
    laned.sort_by(|&a, &b| {
        let (la, lb) = (lane(&pending[a]), lane(&pending[b]));
        la.child_on_left
            .cmp(&lb.child_on_left)
            .then(la.lo.total_cmp(&lb.lo))
            .then(la.hi.total_cmp(&lb.hi))
            .then(a.cmp(&b))
    });

    let mut from = 0;
    while from < laned.len() {
        let first = lane(&pending[laned[from]]);
        let mut reach = first.hi;
        let mut to = from + 1;
        while let Some(&next) = laned.get(to) {
            let l = lane(&pending[next]);
            if l.child_on_left != first.child_on_left || l.lo >= reach {
                break;
            }
            reach = reach.max(l.hi);
            to += 1;
        }
        let members = &mut laned[from..to];
        from = to;

        let (mut lo, mut hi) = (f32::MIN, f32::MAX);
        let (mut union_lo, mut union_hi) = (f32::MAX, f32::MIN);
        for &m in members.iter() {
            let l = lane(&pending[m]);
            (lo, hi) = (lo.max(l.lo), hi.min(l.hi));
            (union_lo, union_hi) = (union_lo.min(l.lo), union_hi.max(l.hi));
        }
        if lo >= hi {
            (lo, hi) = (union_lo, union_hi);
        }
        members.sort_by(|&a, &b| {
            let (pa, pb) = (&pending[a], &pending[b]);
            pa.start
                .y
                .total_cmp(&pb.start.y)
                .then(pa.end.y.total_cmp(&pb.end.y))
                .then(a.cmp(&b))
        });
        // Lines running down leave the lowest source nearest the child.
        let down = members
            .iter()
            .map(|&m| pending[m].end.y - pending[m].start.y)
            .sum::<f32>()
            >= 0.0;
        if down {
            members.reverse();
        }
        let count = members.len();
        let step = ((hi - lo) / (count + 1) as f32).min(CHANNEL_MAX_STEP);
        let first_x = (lo + hi) / 2.0 - step * (count - 1) as f32 / 2.0;
        for (rank, &m) in members.iter().enumerate() {
            // `rank` counts outwards from the child side.
            let slot = if first.child_on_left {
                rank
            } else {
                count - 1 - rank
            };
            pending[m].mid_x = first_x + step * slot as f32;
        }
    }
}

impl DiagramRect {
    pub(in crate::app) fn right(&self) -> f32 {
        self.x + self.width
    }

    pub(in crate::app) fn bottom(&self) -> f32 {
        self.y + self.height
    }

    pub(in crate::app) fn center(&self) -> DiagramPoint {
        DiagramPoint { x: self.x + self.width / 2.0, y: self.y + self.height / 2.0 }
    }

    pub(in crate::app) fn contains(&self, p: DiagramPoint) -> bool {
        p.x >= self.x && p.x < self.right() && p.y >= self.y && p.y < self.bottom()
    }

    /// The rectangles share area.
    pub(in crate::app) fn intersects(&self, other: &Self) -> bool {
        self.x < other.right() && other.x < self.right() && self.y < other.bottom() && other.y < self.bottom()
    }

    /// `self` lies inside `outer`, allowing `slack` units of overhang.
    pub(in crate::app) fn is_inside(&self, outer: &Self, slack: f32) -> bool {
        self.x >= outer.x - slack
            && self.y >= outer.y - slack
            && self.right() <= outer.right() + slack
            && self.bottom() <= outer.bottom() + slack
    }

    /// Grown by `amount` on every side.
    pub(in crate::app) fn inflate(&self, amount: f32) -> Self {
        Self {
            x: self.x - amount,
            y: self.y - amount,
            width: self.width + 2.0 * amount,
            height: self.height + 2.0 * amount,
        }
    }

    /// Smallest rectangle containing both.
    pub(in crate::app) fn union(&self, other: &Self) -> Self {
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Self {
            x,
            y,
            width: self.right().max(other.right()) - x,
            height: self.bottom().max(other.bottom()) - y,
        }
    }
}

/// Distance from `p` to the segment `a`-`b`.
fn segment_distance(p: DiagramPoint, a: DiagramPoint, b: DiagramPoint) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length_sq = dx * dx + dy * dy;
    let t = if length_sq > 0.0 {
        (((p.x - a.x) * dx + (p.y - a.y) * dy) / length_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((p.x - a.x - t * dx).powi(2) + (p.y - a.y - t * dy).powi(2)).sqrt()
}

/// The relation (index into `graph.relations`) whose route passes closest to
/// `point`, if within `tolerance` diagram units.
pub(in crate::app) fn hit_route(routes: &[RoutedRelation], point: DiagramPoint, tolerance: f32) -> Option<usize> {
    routes
        .iter()
        .filter_map(|route| {
            let distance = route
                .points
                .windows(2)
                .map(|w| segment_distance(point, w[0], w[1]))
                .fold(f32::MAX, f32::min);
            (distance <= tolerance).then_some((distance, route.relation))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, relation)| relation)
}

/// Mapping between diagram space and a `width` x `height` overview that shows
/// the whole content and the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::app) struct Minimap {
    /// Top-left of the mapped world (diagram units).
    origin: DiagramPoint,
    scale: f32,
    /// Where the world's top-left lands in the overview (overview pixels).
    offset: DiagramPoint,
}

impl Minimap {
    /// Fits `content` and `viewport` (both diagram space) into the overview
    /// with `padding` around them.
    pub(in crate::app) fn new(
        content: DiagramRect,
        viewport: DiagramRect,
        width: f32,
        height: f32,
        padding: f32,
    ) -> Self {
        let world = content.union(&viewport);
        let scale = ((width - 2.0 * padding) / world.width.max(1.0))
            .min((height - 2.0 * padding) / world.height.max(1.0))
            .max(f32::EPSILON);
        Self {
            origin: DiagramPoint { x: world.x, y: world.y },
            scale,
            offset: DiagramPoint {
                x: (width - world.width * scale) / 2.0,
                y: (height - world.height * scale) / 2.0,
            },
        }
    }

    pub(in crate::app) fn to_map(self, p: DiagramPoint) -> DiagramPoint {
        DiagramPoint {
            x: (p.x - self.origin.x) * self.scale + self.offset.x,
            y: (p.y - self.origin.y) * self.scale + self.offset.y,
        }
    }

    pub(in crate::app) fn to_diagram(self, p: DiagramPoint) -> DiagramPoint {
        DiagramPoint {
            x: (p.x - self.offset.x) / self.scale + self.origin.x,
            y: (p.y - self.offset.y) / self.scale + self.origin.y,
        }
    }

    pub(in crate::app) fn scale(&self) -> f32 {
        self.scale
    }
}

fn is_visible(column: &DiagramColumn, mode: AttributeMode) -> bool {
    mode.shows(column)
}

/// Visible rows and whether the separator under the PK block is drawn (both
/// PK and non-PK rows visible; PK columns always come first).
fn visible_rows(columns: &[DiagramColumn], mode: AttributeMode) -> (usize, bool) {
    let rows = columns.iter().filter(|c| is_visible(c, mode)).count();
    let pk_rows = columns.iter().take_while(|c| c.primary_key).count();
    (rows, pk_rows > 0 && rows > pk_rows)
}

/// Width from the longest of header label / visible `name  type` rows.
fn entity_width(
    graph: &DiagramGraph,
    entity: EntityId,
    mode: AttributeMode,
    metrics: &Metrics,
) -> f32 {
    let e = &graph.entities[entity.0];
    let longest_row = e
        .columns
        .iter()
        .filter(|c| is_visible(c, mode))
        .map(|c| c.name.chars().count() + 2 + c.data_type.chars().count())
        .max()
        .unwrap_or(0);
    let chars = longest_row.max(e.label.chars().count());
    (chars as f32 * metrics.char_width).clamp(metrics.min_width, metrics.max_width)
}

/// Offset from the box top to the vertical centre of `column`'s row, `None`
/// when the column is absent or hidden by `mode`.
fn row_center(
    graph: &DiagramGraph,
    entity: EntityId,
    column: usize,
    mode: AttributeMode,
    metrics: &Metrics,
) -> Option<f32> {
    let columns = &graph.entities[entity.0].columns;
    let col = columns.get(column).filter(|c| is_visible(c, mode))?;
    let index = columns[..column]
        .iter()
        .filter(|c| is_visible(c, mode))
        .count();
    let separator = if !col.primary_key && columns.first().is_some_and(|c| c.primary_key) {
        metrics.separator_height
    } else {
        0.0
    };
    Some(metrics.header_height + separator + (index as f32 + 0.5) * metrics.row_height)
}

impl DiagramLayout {
    /// Focus entity in the centre column, parents (tables the focus
    /// references) to the right, children (tables referencing the focus) to
    /// the left, each column stacked by name; no overlaps; deterministic.
    pub(in crate::app) fn auto(
        graph: &DiagramGraph,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> Self {
        if graph.entities.is_empty() {
            return Self::default();
        }
        let roles = Role::assign(graph);
        let sizes: Vec<(f32, f32)> = (0..graph.entities.len())
            .map(|i| {
                (
                    entity_width(graph, EntityId(i), mode, metrics),
                    Self::entity_height(graph, EntityId(i), mode, metrics),
                )
            })
            .collect();
        let column =
            |role: Role| -> Vec<usize> { (0..roles.len()).filter(|&i| roles[i] == role).collect() };
        let (left, right) = (column(Role::Left), column(Role::Right));
        let column_width = |ids: &[usize]| ids.iter().map(|&i| sizes[i].0).fold(0.0_f32, f32::max);
        let left_width = column_width(&left);
        let focus_x = if left.is_empty() {
            0.0
        } else {
            left_width + metrics.column_gap
        };
        let right_x = focus_x + sizes[0].0 + metrics.column_gap;

        let mut rects = vec![DiagramRect::default(); graph.entities.len()];
        rects[0] = DiagramRect {
            x: focus_x,
            y: 0.0,
            width: sizes[0].0,
            height: sizes[0].1,
        };
        let focus_center = sizes[0].1 / 2.0;
        for (ids, left_aligned) in [(&left, false), (&right, true)] {
            let total = ids.iter().map(|&i| sizes[i].1).sum::<f32>()
                + metrics.row_gap * ids.len().saturating_sub(1) as f32;
            let mut y = focus_center - total / 2.0;
            for &i in ids {
                let (width, height) = sizes[i];
                let x = if left_aligned {
                    right_x
                } else {
                    left_width - width
                };
                rects[i] = DiagramRect {
                    x,
                    y,
                    width,
                    height,
                };
                y += height + metrics.row_gap;
            }
        }
        Self { rects }
    }

    /// Layered layout for a whole-schema graph. Per connected component,
    /// entities are ranked by longest foreign-key path (cycles broken at DFS
    /// back edges) so parents sit to the right of their children as in
    /// `auto`, ordered within ranks by barycentre sweeps to cut crossings.
    /// Components are shelf-packed largest first; tables without relations
    /// follow in a grid below. No overlaps; deterministic. Edges spanning
    /// several ranks are not given waypoints (they cross intermediate boxes).
    pub(in crate::app) fn auto_schema(
        graph: &DiagramGraph,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> Self {
        let n = graph.entities.len();
        let sizes: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                (
                    entity_width(graph, EntityId(i), mode, metrics),
                    Self::entity_height(graph, EntityId(i), mode, metrics),
                )
            })
            .collect();
        let mut parents_of = vec![Vec::new(); n];
        let mut neighbours = vec![Vec::new(); n];
        for r in graph.relations.iter().filter(|r| r.child != r.parent) {
            parents_of[r.child.0].push(r.parent.0);
            neighbours[r.child.0].push(r.parent.0);
            neighbours[r.parent.0].push(r.child.0);
        }
        for list in parents_of.iter_mut().chain(neighbours.iter_mut()) {
            list.sort_unstable();
            list.dedup();
        }
        let ranks = schema_ranks(&parents_of);

        let mut blocks: Vec<Block> = connected_components(&neighbours)
            .into_iter()
            .filter(|members| members.len() > 1)
            .map(|members| Block::layered(members, &ranks, &neighbours, &sizes, metrics))
            .collect();
        blocks.sort_by(|a, b| {
            b.members
                .len()
                .cmp(&a.members.len())
                .then(a.members[0].cmp(&b.members[0]))
        });
        let isolated: Vec<usize> = (0..n).filter(|&i| neighbours[i].is_empty()).collect();

        let cell = |w: f32, h: f32| (w + metrics.column_gap) * (h + metrics.row_gap);
        let area: f32 = blocks.iter().map(|b| cell(b.width, b.height)).sum::<f32>()
            + isolated
                .iter()
                .map(|&i| cell(sizes[i].0, sizes[i].1))
                .sum::<f32>();
        let widest = blocks.iter().map(|b| b.width).fold(0.0_f32, f32::max);
        let target_width = widest.max((area * SCHEMA_ASPECT).sqrt());

        let mut rects = vec![DiagramRect::default(); n];
        let (mut x, mut y, mut shelf) = (0.0_f32, 0.0_f32, 0.0_f32);
        for block in &blocks {
            if x > 0.0 && x + block.width > target_width {
                (x, y, shelf) = (0.0, y + shelf + metrics.row_gap, 0.0);
            }
            for (&member, offset) in block.members.iter().zip(&block.offsets) {
                rects[member] = DiagramRect {
                    x: x + offset.x,
                    y: y + offset.y,
                    width: sizes[member].0,
                    height: sizes[member].1,
                };
            }
            x += block.width + metrics.column_gap;
            shelf = shelf.max(block.height);
        }

        if !isolated.is_empty() {
            let cell_width = isolated.iter().map(|&i| sizes[i].0).fold(0.0_f32, f32::max);
            let columns =
                (((target_width + metrics.column_gap) / (cell_width + metrics.column_gap)).floor()
                    as usize)
                    .max(1);
            let mut top = if blocks.is_empty() {
                0.0
            } else {
                y + shelf + metrics.row_gap * 2.0
            };
            for row in isolated.chunks(columns) {
                for (column, &i) in row.iter().enumerate() {
                    rects[i] = DiagramRect {
                        x: column as f32 * (cell_width + metrics.column_gap),
                        y: top,
                        width: sizes[i].0,
                        height: sizes[i].1,
                    };
                }
                top += row.iter().map(|&i| sizes[i].1).fold(0.0_f32, f32::max) + metrics.row_gap;
            }
        }
        Self { rects }
    }

    /// Height of an entity box for `mode` (used by the view and by `auto`).
    pub(in crate::app) fn entity_height(
        graph: &DiagramGraph,
        entity: EntityId,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> f32 {
        let (rows, separator) = visible_rows(&graph.entities[entity.0].columns, mode);
        metrics.header_height
            + rows as f32 * metrics.row_height
            + if separator {
                metrics.separator_height
            } else {
                0.0
            }
    }

    /// Re-measures every box after data arrived or the attribute mode changed
    /// (boxes grow as their columns load). Entities with `pinned[i]` (placed by
    /// the user: dragged, or restored from the store) keep their position; the
    /// others take the automatic place for the new sizes, then step down out of
    /// the way of any box they would overlap, so boxes never overlap unless the
    /// user put them there. With nothing pinned this is the automatic layout.
    pub(in crate::app) fn reflow(
        &mut self,
        graph: &DiagramGraph,
        mode: AttributeMode,
        metrics: &Metrics,
        schema: bool,
        pinned: &[bool],
    ) {
        let mut fresh = if schema {
            Self::auto_schema(graph, mode, metrics)
        } else {
            Self::auto(graph, mode, metrics)
        };
        if self.rects.len() == fresh.rects.len() {
            let is_pinned = |i: usize| pinned.get(i).copied().unwrap_or(false);
            for (i, rect) in fresh.rects.iter_mut().enumerate().filter(|(i, _)| is_pinned(*i)) {
                (rect.x, rect.y) = (self.rects[i].x, self.rects[i].y);
            }
            // The automatic layout has no overlaps: only boxes the user
            // placed can put another one in the way.
            if pinned.iter().any(|&p| p) {
                fresh.step_out_of_the_way(metrics, is_pinned);
            }
        }
        *self = fresh;
    }

    /// Moves every entity `fixed` does not hold down to the first free place
    /// below whatever it overlaps (fixed boxes and the unfixed ones before it).
    fn step_out_of_the_way(&mut self, metrics: &Metrics, fixed: impl Fn(usize) -> bool) {
        const EPSILON: f32 = 0.01;
        for i in (0..self.rects.len()).filter(|&i| !fixed(i)) {
            loop {
                let me = self.rects[i];
                let blocker = self.rects.iter().enumerate().find(|(j, r)| {
                    *j != i
                        && (fixed(*j) || *j < i)
                        && me.x < r.right() - EPSILON
                        && r.x < me.right() - EPSILON
                        && me.y < r.bottom() - EPSILON
                        && r.y < me.bottom() - EPSILON
                });
                let Some((_, blocker)) = blocker else {
                    break;
                };
                self.rects[i].y = blocker.bottom() + metrics.row_gap;
            }
        }
    }

    pub(in crate::app) fn move_entity(&mut self, entity: EntityId, dx: f32, dy: f32) {
        if let Some(rect) = self.rects.get_mut(entity.0) {
            rect.x += dx;
            rect.y += dy;
        }
    }

    /// Orthogonal routes for every relation (horizontal–vertical–horizontal),
    /// anchored at the visible FK / PK rows for `mode` (header centre when a
    /// row is hidden or the parent is still loading). Relations crossing the
    /// same column gap each get their own vertical channel, computed from the
    /// current rects on every call.
    pub(in crate::app) fn route(
        &self,
        graph: &DiagramGraph,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> Vec<RoutedRelation> {
        let pair = |r: &super::model::DiagramRelation| {
            (r.child.0.min(r.parent.0), r.child.0.max(r.parent.0))
        };
        let mut seen: HashMap<(usize, usize), usize> = HashMap::new();
        let anchor_y = |entity: EntityId, column: Option<&usize>| {
            let offset = column
                .and_then(|&c| row_center(graph, entity, c, mode, metrics))
                .unwrap_or(metrics.header_height / 2.0);
            self.rects[entity.0].y + offset
        };
        let mut pending = Vec::with_capacity(graph.relations.len());
        for (i, relation) in graph.relations.iter().enumerate() {
            if relation.child.0 >= self.rects.len() || relation.parent.0 >= self.rects.len() {
                continue;
            }
            let (child, parent) = (self.rects[relation.child.0], self.rects[relation.parent.0]);
            let start_y = anchor_y(relation.child, relation.child_columns.first());
            let end_y = anchor_y(relation.parent, relation.parent_columns.first());
            // Position of this relation among those joining the same pair
            // (only used by loops, which have no column gap to share).
            let rank = {
                let seen = seen.entry(pair(relation)).or_default();
                *seen += 1;
                (*seen - 1) as f32
            };

            let (start_x, end_x, mid_x, lane) = if relation.child == relation.parent {
                // Self reference: loop out of the right edge and back.
                let mid = child.right() + metrics.column_gap / 8.0 + rank * PARALLEL_STEP;
                (child.right(), child.right(), mid, None)
            } else if child.right() <= parent.x {
                // Channel window: the gap in front of the parent.
                let lo = child.right().max(parent.x - metrics.column_gap);
                let lane = Lane {
                    lo,
                    hi: parent.x,
                    child_on_left: true,
                };
                (child.right(), parent.x, 0.0, Some(lane))
            } else if child.x >= parent.right() {
                let hi = child.x.min(parent.right() + metrics.column_gap);
                let lane = Lane {
                    lo: parent.right(),
                    hi,
                    child_on_left: false,
                };
                (child.x, parent.right(), 0.0, Some(lane))
            } else {
                // Horizontally overlapping boxes: both legs leave on the right.
                let mid = child.right().max(parent.right())
                    + metrics.column_gap / 2.0
                    + rank * PARALLEL_STEP;
                (child.right(), parent.right(), mid, None)
            };
            pending.push(PendingRoute {
                relation: i,
                start: DiagramPoint {
                    x: start_x,
                    y: start_y,
                },
                end: DiagramPoint { x: end_x, y: end_y },
                mid_x,
                lane,
            });
        }
        assign_channels(&mut pending);
        pending
            .into_iter()
            .map(|p| RoutedRelation {
                relation: p.relation,
                points: vec![
                    p.start,
                    DiagramPoint {
                        x: p.mid_x,
                        y: p.start.y,
                    },
                    DiagramPoint {
                        x: p.mid_x,
                        y: p.end.y,
                    },
                    p.end,
                ],
            })
            .collect()
    }

    /// Bounding box of all entities (for Fit).
    pub(in crate::app) fn bounds(&self) -> DiagramRect {
        let Some(first) = self.rects.first() else {
            return DiagramRect::default();
        };
        let (min_x, min_y, max_x, max_y) = self.rects.iter().fold(
            (first.x, first.y, first.right(), first.bottom()),
            |(x0, y0, x1, y1), r| {
                (
                    x0.min(r.x),
                    y0.min(r.y),
                    x1.max(r.right()),
                    y1.max(r.bottom()),
                )
            },
        );
        DiagramRect {
            x: min_x,
            y: min_y,
            width: max_x - min_x,
            height: max_y - min_y,
        }
    }

    /// Where a link from `column` of `entity` leaves the box: the right edge,
    /// at the vertical centre of the row. `None` when the row is not shown.
    pub(in crate::app) fn row_anchor(
        &self,
        graph: &DiagramGraph,
        entity: EntityId,
        column: usize,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> Option<DiagramPoint> {
        let rect = self.rects.get(entity.0)?;
        let offset = row_center(graph, entity, column, mode, metrics)?;
        Some(DiagramPoint { x: rect.right(), y: rect.y + offset })
    }

    /// Smallest rectangle containing the boxes of `members`.
    pub(in crate::app) fn union_of(&self, members: &[usize]) -> Option<DiagramRect> {
        members.iter().filter_map(|&i| self.rects.get(i)).copied().reduce(|a, b| a.union(&b))
    }

    /// Topmost entity under a diagram-space point (later entities draw on top).
    pub(in crate::app) fn hit_test(&self, point: DiagramPoint) -> Option<EntityId> {
        self.hit_test_where(point, |_| true)
    }

    /// [`Self::hit_test`] over the entities `shown` accepts (hidden ones are
    /// not there to be hit).
    pub(in crate::app) fn hit_test_where(&self, point: DiagramPoint, shown: impl Fn(usize) -> bool) -> Option<EntityId> {
        self.rects.iter().enumerate().rev().find(|(i, r)| shown(*i) && r.contains(point)).map(|(i, _)| EntityId(i))
    }

    /// The row of `entity` under a diagram-space point: the index into the
    /// entity's `columns` of the visible column whose row contains `point`.
    /// The inverse of the row anchors the routes use.
    pub(in crate::app) fn row_at(
        &self,
        graph: &DiagramGraph,
        entity: EntityId,
        point: DiagramPoint,
        mode: AttributeMode,
        metrics: &Metrics,
    ) -> Option<usize> {
        let rect = self.rects.get(entity.0)?;
        if !rect.contains(point) {
            return None;
        }
        let columns = &graph.entities.get(entity.0)?.columns;
        let first_is_pk = columns.first().is_some_and(|c| c.primary_key);
        columns
            .iter()
            .enumerate()
            .filter(|(_, c)| is_visible(c, mode))
            .enumerate()
            .find_map(|(shown, (index, column))| {
                let separator = if !column.primary_key && first_is_pk { metrics.separator_height } else { 0.0 };
                let top = rect.y + metrics.header_height + separator + shown as f32 * metrics.row_height;
                (point.y >= top && point.y < top + metrics.row_height).then_some(index)
            })
    }

    /// Entities intersecting a `width`×`height` screen area (plus a small
    /// margin) under `viewport`, in draw order. Allocation-free.
    pub(in crate::app) fn visible(
        &self,
        viewport: &Viewport,
        width: f32,
        height: f32,
    ) -> impl Iterator<Item = EntityId> + '_ {
        let margin = CULL_MARGIN / viewport.zoom;
        let (x0, y0) = (viewport.origin.x - margin, viewport.origin.y - margin);
        let (x1, y1) = (
            viewport.origin.x + width / viewport.zoom + margin,
            viewport.origin.y + height / viewport.zoom + margin,
        );
        self.rects
            .iter()
            .enumerate()
            .filter(move |(_, r)| r.x < x1 && r.right() > x0 && r.y < y1 && r.bottom() > y0)
            .map(|(i, _)| EntityId(i))
    }
}

/// Pan + zoom. Screen = (diagram - origin) * zoom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::app) struct Viewport {
    pub(in crate::app) origin: DiagramPoint,
    pub(in crate::app) zoom: f32,
    /// Lowest zoom this viewport allows (`MIN_ZOOM`, or `SCHEMA_MIN_ZOOM` for
    /// whole-schema diagrams that must be overviewable).
    pub(in crate::app) min_zoom: f32,
}

impl Viewport {
    pub(in crate::app) const MIN_ZOOM: f32 = 0.5;
    pub(in crate::app) const SCHEMA_MIN_ZOOM: f32 = 0.1;
    pub(in crate::app) const MAX_ZOOM: f32 = 2.0;

    #[allow(clippy::wrong_self_convention)] // contract signature; `&self` keeps callers unchanged
    pub(in crate::app) fn to_screen(&self, point: DiagramPoint) -> DiagramPoint {
        DiagramPoint {
            x: (point.x - self.origin.x) * self.zoom,
            y: (point.y - self.origin.y) * self.zoom,
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub(in crate::app) fn to_diagram(&self, point: DiagramPoint) -> DiagramPoint {
        DiagramPoint {
            x: point.x / self.zoom + self.origin.x,
            y: point.y / self.zoom + self.origin.y,
        }
    }

    /// Zoom by `factor` keeping the screen point `anchor` fixed; clamped.
    pub(in crate::app) fn zoom_at(&mut self, factor: f32, anchor: DiagramPoint) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let fixed = self.to_diagram(anchor);
        self.zoom = (self.zoom * factor).clamp(self.min_zoom, Self::MAX_ZOOM);
        self.origin = DiagramPoint {
            x: fixed.x - anchor.x / self.zoom,
            y: fixed.y - anchor.y / self.zoom,
        };
    }

    /// The part of diagram space a `width` x `height` screen area shows.
    pub(in crate::app) fn visible_rect(&self, width: f32, height: f32) -> DiagramRect {
        DiagramRect {
            x: self.origin.x,
            y: self.origin.y,
            width: width / self.zoom,
            height: height / self.zoom,
        }
    }

    /// Pans (keeping the zoom) so diagram point `target` is at the centre of a
    /// `width` x `height` screen area.
    pub(in crate::app) fn center_on(&mut self, target: DiagramPoint, width: f32, height: f32) {
        self.origin = DiagramPoint {
            x: target.x - width / 2.0 / self.zoom,
            y: target.y - height / 2.0 / self.zoom,
        };
    }

    /// Moves the content by a screen-space delta (drag direction).
    pub(in crate::app) fn pan(&mut self, screen_dx: f32, screen_dy: f32) {
        self.origin.x -= screen_dx / self.zoom;
        self.origin.y -= screen_dy / self.zoom;
    }

    /// Zoom/origin that fit `bounds` into a `width`×`height` screen area with
    /// `padding`, clamped to the zoom range.
    pub(in crate::app) fn fit(
        bounds: DiagramRect,
        width: f32,
        height: f32,
        padding: f32,
        min_zoom: f32,
    ) -> Self {
        let available_w = (width - 2.0 * padding).max(1.0);
        let available_h = (height - 2.0 * padding).max(1.0);
        let zoom = if bounds.width > 0.0 && bounds.height > 0.0 {
            (available_w / bounds.width)
                .min(available_h / bounds.height)
                .clamp(min_zoom, Self::MAX_ZOOM)
        } else {
            1.0
        };
        Self {
            origin: DiagramPoint {
                x: bounds.x + bounds.width / 2.0 - width / 2.0 / zoom,
                y: bounds.y + bounds.height / 2.0 - height / 2.0 / zoom,
            },
            zoom,
            min_zoom,
        }
    }
}
