//! Relationship notation: the end markers of a routed relation, as pure
//! geometry shared by the canvas painter and the SVG export (no GPUI).
//!
//! Crow's foot: three lines meeting at the entity edge. From the
//! entity edge, the foot is two lines from the edge (+-5) to an apex 10 units
//! out along the line; a bar crosses the line at 10 (and 15), a ring of
//! radius 4 sits centred at 15. Child (FK) end: many (foot + bar), or a
//! double bar when the FK columns are unique. Parent end: a double bar
//! (mandatory) or bar + ring (the FK is nullable).

use super::layout::DiagramPoint;
use super::model::DiagramRelation;

/// How relations are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum RelationNotation {
    /// FK dot at the child, arrow head at the parent.
    Plain,
    #[default]
    CrowsFoot,
}

/// One end of a relation in crow's foot notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum EndMark {
    /// Exactly one: two bars.
    One,
    /// Zero or one: bar and ring.
    ZeroOrOne,
    /// One or many: foot and bar.
    OneOrMany,
}

/// Distance of the foot apex / first bar from the entity edge.
const NEAR: f32 = 10.0;
/// Distance of the second bar / ring centre.
const FAR: f32 = 15.0;
/// Half length of bars and foot spread.
const SPREAD: f32 = 5.0;
const RING_RADIUS: f32 = 4.0;

/// The strokes and the optional ring of one end marker, in the units of the
/// anchor (diagram units at `scale` 1).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(in crate::app) struct MarkShape {
    lines: [(DiagramPoint, DiagramPoint); 3],
    count: usize,
    /// Ring centre and radius.
    pub(in crate::app) ring: Option<(DiagramPoint, f32)>,
}

impl MarkShape {
    pub(in crate::app) fn lines(&self) -> &[(DiagramPoint, DiagramPoint)] {
        &self.lines[..self.count]
    }

    fn push(&mut self, from: DiagramPoint, to: DiagramPoint) {
        self.lines[self.count] = (from, to);
        self.count += 1;
    }
}

impl EndMark {
    /// End marker at the child (FK) side.
    pub(in crate::app) fn child(relation: &DiagramRelation) -> Self {
        if relation.unique {
            Self::One
        } else {
            Self::OneOrMany
        }
    }

    /// End marker at the parent side.
    pub(in crate::app) fn parent(relation: &DiagramRelation) -> Self {
        if relation.optional {
            Self::ZeroOrOne
        } else {
            Self::One
        }
    }

    /// Geometry at `tip` (the point on the entity edge). `toward` is +1 / -1:
    /// the x direction pointing away from the entity along the line. Lengths
    /// are multiplied by `scale`.
    pub(in crate::app) fn shape(self, tip: DiagramPoint, toward: f32, scale: f32) -> MarkShape {
        let at = |u: f32, v: f32| DiagramPoint { x: tip.x + toward * u * scale, y: tip.y + v * scale };
        let bar = |u: f32| (at(u, -SPREAD), at(u, SPREAD));
        let mut shape = MarkShape::default();
        match self {
            Self::One => {
                for u in [NEAR, FAR] {
                    let (a, b) = bar(u);
                    shape.push(a, b);
                }
            }
            Self::ZeroOrOne => {
                let (a, b) = bar(NEAR);
                shape.push(a, b);
                shape.ring = Some((at(FAR, 0.0), RING_RADIUS * scale));
            }
            Self::OneOrMany => {
                shape.push(at(0.0, -SPREAD), at(NEAR, 0.0));
                shape.push(at(NEAR, 0.0), at(0.0, SPREAD));
                let (a, b) = bar(NEAR);
                shape.push(a, b);
            }
        }
        shape
    }
}

/// Direction of the first and last segment of a route: x pointing away from
/// the child's edge and away from the parent's edge.
pub(in crate::app) fn route_directions(points: &[DiagramPoint]) -> Option<(f32, f32)> {
    let sign = |from: DiagramPoint, to: DiagramPoint| if to.x >= from.x { 1.0 } else { -1.0 };
    let (first, second) = (points.first()?, points.get(1)?);
    let (last, before) = (points.last()?, points.get(points.len().checked_sub(2)?)?);
    Some((sign(*first, *second), sign(*last, *before)))
}
