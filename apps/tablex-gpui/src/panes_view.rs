//! Rendering helpers for the editor-area pane tree (`crate::panes`): the
//! drop-zone math and overlay shown while a tab is dragged over a pane, and the
//! recursive split layout built on gpui-component's `resizable`.
//!
//! Nothing here knows about `AppRoot`: the caller supplies the pane content and
//! the resize callback, so the whole module is exercised by the mock harness in
//! the tests below.
//!
//! Hot-path notes: a frame builds one `ResizablePanelGroup` per split and one
//! panel per child (no entities of our own: the split state is gpui's own
//! element-keyed `ResizableState`); drag hit-testing uses the bounds gpui hands
//! to `on_drag_move` and allocates nothing.

use crate::panes::{Edge, Node, Pane, PaneDrop, PaneId, PaneTree, SplitAxis, SplitId};
use crate::theme;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    canvas, div, px, relative, AnyElement, App, Bounds, InteractiveElement as _, IntoElement,
    MouseButton, MouseDownEvent, ParentElement as _, Pixels, Point, Styled as _, Window,
};
use gpui_component::dock::DropPlaceholderBounds;
use gpui_component::resizable::{h_resizable, resizable_panel, v_resizable, ResizableState};
use gpui_component::{ActiveTheme as _, Placement};
use std::rc::Rc;

/// Smallest width of a pane in a row / height of a pane in a column (Zed's
/// values).
pub const MIN_PANE_WIDTH: f32 = 80.0;
pub const MIN_PANE_HEIGHT: f32 = 100.0;
/// Width of the band along each edge of a pane that turns a drag into a split,
/// as a fraction of the pane's shorter side (Zed's `drop_target_size`).
pub const DROP_BAND: f32 = 0.2;
/// Slack around a splitter for the double-click reset (the handle's hit area).
const SPLITTER_SLOP: f32 = 5.0;

/// Where in a pane's content area a dragged tab would land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropZone {
    /// Move into the pane's tab list.
    Center,
    /// Split the pane and put the tab in the new pane on that side.
    Edge(Edge),
}

/// The zone under the pointer, with Zed's rule (`Pane::handle_drag_move`): the
/// band is [`DROP_BAND`] of the pane's shorter side; a pointer strictly inside
/// that band from any edge splits towards the nearest edge (ties: up, right,
/// down, left), anywhere else drops into the pane. Unlike percentages of each
/// axis, the band does not stretch on very wide or very short panes. (The
/// dock's own `split_placement_at` is crate-private in gpui-base 0.7.1 and
/// uses 35% / 65% thresholds.)
pub fn drop_zone_at(bounds: Bounds<Pixels>, position: Point<Pixels>) -> DropZone {
    let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let x = f32::from(position.x - bounds.left());
    let y = f32::from(position.y - bounds.top());
    let band = width.min(height) * DROP_BAND;
    if !(x < band || x > width - band || y < band || y > height - band) {
        return DropZone::Center;
    }
    // Distance to each side, first minimum wins.
    let sides = [
        (Edge::Top, y),
        (Edge::Right, width - x),
        (Edge::Bottom, height - y),
        (Edge::Left, x),
    ];
    let nearest = sides.iter().fold(
        sides[0],
        |best, side| if side.1 < best.1 { *side } else { best },
    );
    DropZone::Edge(nearest.0)
}

/// The pane to focus when moving in `direction` from `from`, given the panes'
/// painted bounds: among the panes lying entirely beyond that edge of `from`
/// and overlapping it on the other axis, the nearest one (ties: the larger
/// overlap). Zed steps a few pixels past the edge from the pane centre and
/// hit-tests; this also finds the neighbour when the centre line falls into
/// the gap between two stacked panes.
pub fn neighbor_in_direction(
    panes: &[(PaneId, Bounds<Pixels>)],
    from: PaneId,
    direction: Edge,
) -> Option<PaneId> {
    let (_, origin) = panes.iter().find(|(id, _)| *id == from)?;
    let span = |bounds: &Bounds<Pixels>, horizontal: bool| {
        if horizontal {
            (f32::from(bounds.left()), f32::from(bounds.right()))
        } else {
            (f32::from(bounds.top()), f32::from(bounds.bottom()))
        }
    };
    // Main axis is the one we move along, cross axis the other.
    let main_horizontal = matches!(direction, Edge::Left | Edge::Right);
    let (origin_start, origin_end) = span(origin, main_horizontal);
    let (cross_start, cross_end) = span(origin, !main_horizontal);
    panes
        .iter()
        .filter(|(id, _)| *id != from)
        .filter_map(|(id, bounds)| {
            let (start, end) = span(bounds, main_horizontal);
            let gap = match direction {
                Edge::Left | Edge::Top => origin_start - end,
                Edge::Right | Edge::Bottom => start - origin_end,
            };
            let (other_start, other_end) = span(bounds, !main_horizontal);
            let overlap = cross_end.min(other_end) - cross_start.max(other_start);
            // A pixel of slack for rounding between adjacent panes.
            (gap >= -1.0 && overlap > 0.0).then_some((*id, gap.max(0.0), overlap))
        })
        .min_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        })
        .map(|(id, _, _)| id)
}

impl DropZone {
    fn placement(self) -> Option<Placement> {
        match self {
            Self::Center => None,
            Self::Edge(Edge::Left) => Some(Placement::Left),
            Self::Edge(Edge::Right) => Some(Placement::Right),
            Self::Edge(Edge::Top) => Some(Placement::Top),
            Self::Edge(Edge::Bottom) => Some(Placement::Bottom),
        }
    }
}

/// The zone a drag is currently over, kept in the shell state while a tab is
/// dragged (cleared when the drag ends).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DropHover {
    pub pane: PaneId,
    pub zone: DropZone,
}

impl DropHover {
    pub fn to_drop(self) -> PaneDrop {
        match self.zone {
            DropZone::Center => PaneDrop::Tabs {
                pane: self.pane,
                index: None,
            },
            DropZone::Edge(edge) => PaneDrop::Split {
                pane: self.pane,
                edge,
            },
        }
    }
}

/// The highlighted area for `zone` inside a pane content area of size
/// `bounds`: a half of the pane for an edge, all of it for the centre. Geometry
/// is the dock's own `DropPlaceholderBounds`; the fill is the theme's
/// `drop_target` token, like the dock skin's placeholder. Place it as an
/// absolute child of the `relative()` pane content container. It carries no
/// listeners, so it never intercepts the drag.
pub fn drop_overlay(bounds: Bounds<Pixels>, zone: DropZone, cx: &App) -> impl IntoElement {
    let rect = DropPlaceholderBounds::for_placement(bounds, zone.placement());
    div()
        .debug_selector(|| "pane-drop-overlay".into())
        .absolute()
        .left(rect.origin().x)
        .top(rect.origin().y)
        .w(rect.size().width)
        .h(rect.size().height)
        .rounded(px(4.0))
        .bg(cx.theme().drop_target)
        .border_1()
        .border_color(theme::accent())
}

/// Called when a splitter drag ends, with the split and its children's new
/// shares (pixel sizes normalized to sum to one).
pub type ResizeFn = Rc<dyn Fn(SplitId, Vec<f32>, &mut Window, &mut App)>;

/// Lays the tree out: panes through `render_pane`, splits as nested
/// `resizable` groups seeded with the tree's ratios. A maximized pane is
/// rendered alone (the other panes are not built at all).
pub fn render_tree(
    tree: &PaneTree,
    on_resize: &ResizeFn,
    window: &mut Window,
    cx: &mut App,
    render_pane: &mut dyn FnMut(&Pane) -> AnyElement,
) -> AnyElement {
    match tree.maximized().and_then(|id| tree.pane(id)) {
        Some(pane) => render_pane(pane),
        None => render_node(tree.root(), on_resize, window, cx, render_pane),
    }
}

fn render_node(
    node: &Node,
    on_resize: &ResizeFn,
    window: &mut Window,
    cx: &mut App,
    render_pane: &mut dyn FnMut(&Pane) -> AnyElement,
) -> AnyElement {
    let split = match node {
        Node::Pane(pane) => return render_pane(pane),
        Node::Split(split) => split,
    };
    let row = split.axis() == SplitAxis::Row;
    // The resizable state is keyed by element id and sized per child index, so
    // a change of children (a pane added or removed) must start a fresh state
    // seeded from the ratios instead of reusing sizes of other panes.
    // The stored shares are part of the key too: whoever changes them (a
    // drag end, the double-click reset, a restored session) hands the layout
    // back to the ratios instead of leaving stale pixel sizes in the state.
    let shape = split.children().iter().zip(split.ratios()).fold(
        split.id().get(),
        |hash, (child, ratio)| {
            (hash.wrapping_mul(0x0000_0100_0000_01b3) ^ child.key())
                .wrapping_mul(0x0000_0100_0000_01b3)
                ^ (ratio * 1000.0).round() as u64
        },
    );
    let id = ("pane-split", shape);
    // The group would fetch this same keyed state itself; holding it here lets
    // the double-click reset drive it.
    let state = window.use_keyed_state(id, cx, |_, _| ResizableState::default());
    let group = if row {
        h_resizable(id)
    } else {
        v_resizable(id)
    }
    .with_state(&state);
    let split_id = split.id();
    let notify = on_resize.clone();
    let group = group.on_resize(move |state, window, cx| {
        let sizes = state.read(cx).sizes();
        let total: f32 = sizes.iter().map(|size| f32::from(*size)).sum();
        if total <= 0.0 {
            return;
        }
        let ratios = sizes.iter().map(|size| f32::from(*size) / total).collect();
        notify(split_id, ratios, window, cx);
    });
    let (min, axis_len) = if row {
        (MIN_PANE_WIDTH, true)
    } else {
        (MIN_PANE_HEIGHT, false)
    };
    let children: Vec<_> = split
        .children()
        .iter()
        .zip(split.ratios())
        .map(|(child, ratio)| {
            resizable_panel()
                .size_range(px(min)..Pixels::MAX)
                .when(axis_len, |panel| panel.w(relative(*ratio)))
                .when(!axis_len, |panel| panel.h(relative(*ratio)))
                .child(render_node(child, on_resize, window, cx, render_pane))
        })
        .collect();
    // Double-click on a splitter gives every pane of this split an equal share.
    // One window-level listener per split (the handle exposes no click and
    // occludes the group beneath it, so an element listener never sees the
    // press); the canvas measures the group at prepaint and installs it.
    let on_reset = on_resize.clone();
    let count = children.len();
    let reset_listener = canvas(
        |bounds, _, _| bounds,
        move |bounds, _, window, _| {
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.capture() || event.button != MouseButton::Left || event.click_count < 2 {
                    return;
                }
                if !bounds.contains(&event.position) {
                    return;
                }
                let (pointer, mut edge) = if row {
                    (event.position.x, bounds.left())
                } else {
                    (event.position.y, bounds.top())
                };
                let sizes = state.read(cx).sizes().clone();
                let on_splitter = sizes.iter().take(count.saturating_sub(1)).any(|size| {
                    edge += *size;
                    f32::from((pointer - edge).abs()) <= SPLITTER_SLOP
                });
                if !on_splitter {
                    return;
                }
                let equal = bounds_len(bounds, row) / count as f32;
                for index in 0..count.saturating_sub(1) {
                    state.update(cx, |state, cx| {
                        state.resize_panel(index, px(equal), window, cx)
                    });
                }
                on_reset(split_id, vec![1.0 / count as f32; count], window, cx);
            });
        },
    )
    .absolute()
    .size_full();
    div()
        .relative()
        .size_full()
        .child(reset_listener)
        .child(group.children(children))
        .into_any_element()
}

fn bounds_len(bounds: Bounds<Pixels>, row: bool) -> f32 {
    f32::from(if row {
        bounds.size.width
    } else {
        bounds.size.height
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        point, size, AppContext as _, Context, DragMoveEvent, Entity, MouseButton, Render,
        StatefulInteractiveElement as _, TestAppContext, VisualTestContext,
    };

    fn bounds_400x300() -> Bounds<Pixels> {
        Bounds {
            origin: point(px(120.0), px(80.0)),
            size: size(px(400.0), px(300.0)),
        }
    }

    #[test]
    fn drop_zones_follow_zeds_band_rule() {
        // 400x300 at (120, 80): the band is 20% of the shorter side = 60px.
        let bounds = bounds_400x300();
        let at = |x: f32, y: f32| drop_zone_at(bounds, point(px(x), px(y)));
        assert_eq!(at(130.0, 230.0), DropZone::Edge(Edge::Left));
        assert_eq!(at(510.0, 230.0), DropZone::Edge(Edge::Right));
        assert_eq!(at(320.0, 90.0), DropZone::Edge(Edge::Top));
        assert_eq!(at(320.0, 370.0), DropZone::Edge(Edge::Bottom));
        assert_eq!(at(320.0, 230.0), DropZone::Center);
        // A corner goes to the nearer edge; equal distances: up, right, down, left.
        assert_eq!(at(125.0, 85.0), DropZone::Edge(Edge::Top));
        assert_eq!(at(515.0, 375.0), DropZone::Edge(Edge::Right));
        assert_eq!(at(125.0, 100.0), DropZone::Edge(Edge::Left));
        assert_eq!(at(300.0, 372.0), DropZone::Edge(Edge::Bottom));
        // Strictly inside the band splits; the band's inner edge is the centre.
        assert_eq!(at(179.0, 230.0), DropZone::Edge(Edge::Left));
        assert_eq!(at(180.0, 230.0), DropZone::Center);
        assert_eq!(at(320.0, 139.0), DropZone::Edge(Edge::Top));
        assert_eq!(at(320.0, 140.0), DropZone::Center);
        // The band is the same width on every side, however long the pane:
        // a 1000x100 strip has a 20px band, so most of it is the centre
        // (percentages of each axis would make it almost all edge).
        let wide = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1000.0), px(100.0)),
        };
        assert_eq!(
            drop_zone_at(wide, point(px(500.0), px(10.0))),
            DropZone::Edge(Edge::Top)
        );
        assert_eq!(
            drop_zone_at(wide, point(px(500.0), px(50.0))),
            DropZone::Center
        );
        assert_eq!(
            drop_zone_at(wide, point(px(100.0), px(50.0))),
            DropZone::Center
        );
        assert_eq!(
            drop_zone_at(wide, point(px(990.0), px(50.0))),
            DropZone::Edge(Edge::Right)
        );
        // A pane without area never splits.
        let empty = Bounds {
            origin: point(px(5.0), px(5.0)),
            size: size(px(0.0), px(0.0)),
        };
        assert_eq!(
            drop_zone_at(empty, point(px(5.0), px(5.0))),
            DropZone::Center
        );
    }

    #[test]
    fn directional_focus_picks_the_nearest_overlapping_pane() {
        let pane = |n: u64| PaneId::for_test(n);
        let rect = |x: f32, y: f32, w: f32, h: f32| Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        };
        // A | B over C (B and C stacked at the right).
        let panes = [
            (pane(1), rect(0.0, 0.0, 400.0, 600.0)),
            (pane(2), rect(400.0, 0.0, 600.0, 300.0)),
            (pane(3), rect(400.0, 300.0, 600.0, 300.0)),
        ];
        let go = |from, dir| neighbor_in_direction(&panes, pane(from), dir);
        assert_eq!(go(1, Edge::Right), Some(pane(2)), "ties go to the first");
        assert_eq!(go(2, Edge::Left), Some(pane(1)));
        assert_eq!(go(3, Edge::Left), Some(pane(1)));
        assert_eq!(go(2, Edge::Bottom), Some(pane(3)));
        assert_eq!(go(3, Edge::Top), Some(pane(2)));
        assert_eq!(go(1, Edge::Left), None, "nothing beyond the edge");
        assert_eq!(go(1, Edge::Top), None);
        assert_eq!(go(3, Edge::Bottom), None);
        assert_eq!(go(3, Edge::Right), None);
        assert_eq!(go(9, Edge::Right), None, "unknown pane");
        // A pane offset from the centre line is still found (a centre-point
        // hit test would land between B and C).
        let skewed = [
            (pane(1), rect(0.0, 0.0, 400.0, 600.0)),
            (pane(2), rect(400.0, 0.0, 600.0, 100.0)),
            (pane(3), rect(400.0, 100.0, 600.0, 500.0)),
        ];
        assert_eq!(
            neighbor_in_direction(&skewed, pane(1), Edge::Right),
            Some(pane(3)),
            "larger overlap wins"
        );
    }

    #[test]
    fn hover_converts_to_the_matching_tree_drop() {
        let pane = PaneTree::single(vec![1], None).focused();
        let hover = |zone| DropHover { pane, zone }.to_drop();
        assert_eq!(
            hover(DropZone::Center),
            PaneDrop::Tabs { pane, index: None }
        );
        assert_eq!(
            hover(DropZone::Edge(Edge::Top)),
            PaneDrop::Split {
                pane,
                edge: Edge::Top
            }
        );
    }

    /// Mock drag payload standing in for `TabDrag`.
    #[derive(Clone)]
    struct Payload;

    struct DragGhost;

    impl Render for DragGhost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size(px(10.0))
        }
    }

    /// A tree rendered the way the shell will: one `relative()` content box per
    /// pane with a drag-move listener feeding the hover and the overlay.
    struct Harness {
        tree: PaneTree,
        hover: Option<DropHover>,
        /// Bounds of the pane under the pointer (the shell captures these at
        /// prepaint; the drag-move event carries them).
        hover_bounds: Bounds<Pixels>,
        dropped: Option<DropHover>,
        resized: usize,
        width: f32,
        height: f32,
    }

    impl Render for Harness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let this = cx.entity().downgrade();
            let on_resize: ResizeFn = Rc::new(move |split, ratios, _, cx| {
                let _ = this.update(cx, |harness, cx| {
                    harness.tree.set_ratios(split, &ratios);
                    harness.resized += 1;
                    cx.notify();
                });
            });
            let (hover, hover_bounds) = (self.hover, self.hover_bounds);
            let build_pane = |pane: &Pane| -> AnyElement {
                let id = pane.id();
                let selector = format!("pane-{}", id.get());
                div()
                    .id(("pane", id.get()))
                    .relative()
                    .size_full()
                    .debug_selector(move || selector.clone())
                    .on_drag_move(cx.listener(
                        move |this, event: &DragMoveEvent<Payload>, _, cx| {
                            if !event.bounds.contains(&event.event.position) {
                                return;
                            }
                            let zone = drop_zone_at(event.bounds, event.event.position);
                            let next = Some(DropHover { pane: id, zone });
                            if this.hover != next {
                                this.hover = next;
                                this.hover_bounds = event.bounds;
                                cx.notify();
                            }
                        },
                    ))
                    .on_drop(cx.listener(move |this, _: &Payload, _, cx| {
                        this.dropped = this.hover.take();
                        cx.notify();
                    }))
                    .when(pane.tabs().contains(&1), |content| {
                        content.child(
                            div()
                                .id("drag-source")
                                .debug_selector(|| "drag-source".into())
                                .size(px(20.0))
                                .on_drag(Payload, |_, _, _, cx| cx.new(|_| DragGhost)),
                        )
                    })
                    .when_some(hover.filter(|hover| hover.pane == id), |content, hover| {
                        content.child(drop_overlay(hover_bounds, hover.zone, cx))
                    })
                    .into_any_element()
            };
            // Panes first (they need the entity context), then the layout.
            let mut panes: Vec<(PaneId, Option<AnyElement>)> = self
                .tree
                .panes()
                .map(|pane| (pane.id(), Some(build_pane(pane))))
                .collect();
            let mut render_pane = |pane: &Pane| {
                panes
                    .iter_mut()
                    .find(|(id, _)| *id == pane.id())
                    .and_then(|(_, element)| element.take())
                    .unwrap_or_else(|| div().into_any_element())
            };
            div()
                .w(px(self.width))
                .h(px(self.height))
                .child(render_tree(
                    &self.tree,
                    &on_resize,
                    window,
                    cx,
                    &mut render_pane,
                ))
        }
    }

    fn selector_for(id: PaneId) -> String {
        format!("pane-{}", id.get())
    }

    fn harness(
        cx: &mut TestAppContext,
        tree: PaneTree,
    ) -> (Entity<Harness>, &mut VisualTestContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            theme::init(cx);
        });
        let (harness, visual) = cx.add_window_view(|_, _| Harness {
            tree,
            hover: None,
            hover_bounds: Bounds::default(),
            dropped: None,
            resized: 0,
            width: 1000.0,
            height: 600.0,
        });
        settle(visual);
        (harness, visual)
    }

    fn settle(visual: &mut VisualTestContext) {
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }

    fn pane_bounds(visual: &mut VisualTestContext, id: PaneId) -> Bounds<Pixels> {
        let selector: &'static str = Box::leak(selector_for(id).into_boxed_str());
        visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("pane {} is rendered", id.get()))
    }

    fn two_pane_tree(ratio: f32) -> (PaneTree, PaneId, PaneId) {
        let mut tree = PaneTree::single(vec![1, 2], Some(1));
        let left = tree.focused();
        assert!(tree.move_tab(
            2,
            PaneDrop::Split {
                pane: left,
                edge: Edge::Right
            }
        ));
        let right = tree.pane_of(2).unwrap();
        let Node::Split(split) = tree.root() else {
            panic!("split root")
        };
        let id = split.id();
        tree.set_ratios(id, &[1.0 - ratio, ratio]);
        (tree, left, right)
    }

    fn approx(a: Pixels, b: f32, what: &str) {
        assert!(
            (f32::from(a) - b).abs() < 2.0,
            "{what}: expected ~{b}, got {a:?}"
        );
    }

    #[gpui::test]
    fn panes_are_laid_out_by_ratio_and_the_splitter_drag_updates_the_tree(cx: &mut TestAppContext) {
        let (tree, left, right) = two_pane_tree(0.6);
        let (harness, visual) = harness(cx, tree);
        let (a, b) = (pane_bounds(visual, left), pane_bounds(visual, right));
        approx(a.size.width, 400.0, "left width");
        approx(b.size.width, 600.0, "right width");
        approx(a.size.height, 600.0, "full height");

        // Drag the splitter 100px to the right with the real mouse pipeline.
        let handle = point(a.right(), a.center().y);
        let left_button = Some(MouseButton::Left);
        visual.simulate_mouse_down(handle, MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            handle + point(px(50.0), px(0.0)),
            left_button,
            Default::default(),
        );
        visual.simulate_mouse_move(
            handle + point(px(100.0), px(0.0)),
            left_button,
            Default::default(),
        );
        visual.simulate_mouse_up(
            handle + point(px(100.0), px(0.0)),
            MouseButton::Left,
            Default::default(),
        );
        settle(visual);

        harness.read_with(visual, |harness, _| {
            assert_eq!(harness.resized, 1, "one resize notification at drag end");
            let Node::Split(split) = harness.tree.root() else {
                panic!("split root")
            };
            assert!(
                (split.ratios()[0] - 0.5).abs() < 0.01,
                "{:?}",
                split.ratios()
            );
        });
        let a = pane_bounds(visual, left);
        approx(a.size.width, 500.0, "left width after the drag");
    }

    #[gpui::test]
    fn double_clicking_a_splitter_resets_the_split_to_equal_shares(cx: &mut TestAppContext) {
        let (tree, left, right) = two_pane_tree(0.7);
        let (harness, visual) = harness(cx, tree);
        let a = pane_bounds(visual, left);
        approx(a.size.width, 300.0, "left before");
        let handle = point(a.right(), a.center().y);
        // A double-click away from the splitter does nothing.
        let away = point(a.center().x, a.center().y);
        visual.simulate_event(gpui::MouseDownEvent {
            position: away,
            modifiers: Default::default(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        settle(visual);
        approx(
            pane_bounds(visual, left).size.width,
            300.0,
            "left after a stray double-click",
        );
        visual.simulate_event(gpui::MouseDownEvent {
            position: handle,
            modifiers: Default::default(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        visual.simulate_mouse_up(handle, MouseButton::Left, Default::default());
        settle(visual);
        approx(
            pane_bounds(visual, left).size.width,
            500.0,
            "left after the reset",
        );
        approx(
            pane_bounds(visual, right).size.width,
            500.0,
            "right after the reset",
        );
        harness.read_with(visual, |harness, _| {
            let Node::Split(split) = harness.tree.root() else {
                panic!("split root")
            };
            assert!(
                (split.ratios()[0] - 0.5).abs() < 0.01,
                "{:?}",
                split.ratios()
            );
        });
        let _ = right;
    }

    #[gpui::test]
    fn the_splitter_cannot_shrink_a_pane_below_the_minimum(cx: &mut TestAppContext) {
        let (tree, left, _right) = two_pane_tree(0.5);
        let (harness, visual) = harness(cx, tree);
        let a = pane_bounds(visual, left);
        let handle = point(a.right(), a.center().y);
        let left_button = Some(MouseButton::Left);
        visual.simulate_mouse_down(handle, MouseButton::Left, Default::default());
        visual.simulate_mouse_move(
            handle - point(px(100.0), px(0.0)),
            left_button,
            Default::default(),
        );
        visual.simulate_mouse_move(
            handle - point(px(450.0), px(0.0)),
            left_button,
            Default::default(),
        );
        visual.simulate_mouse_up(
            handle - point(px(450.0), px(0.0)),
            MouseButton::Left,
            Default::default(),
        );
        settle(visual);
        let a = pane_bounds(visual, left);
        assert!(
            a.size.width >= px(MIN_PANE_WIDTH - 1.0),
            "left pane kept its minimum width, got {:?}",
            a.size.width
        );
        harness.read_with(visual, |harness, _| {
            let Node::Split(split) = harness.tree.root() else {
                panic!("split root")
            };
            assert!(split.ratios()[0] >= 0.07, "{:?}", split.ratios());
        });
    }

    #[gpui::test]
    fn nested_splits_lay_out_columns_inside_rows(cx: &mut TestAppContext) {
        let (mut tree, _left, right) = two_pane_tree(0.5);
        tree.add_tab(right, 3, None);
        assert!(tree.move_tab(
            3,
            PaneDrop::Split {
                pane: right,
                edge: Edge::Bottom
            }
        ));
        let bottom = tree.pane_of(3).unwrap();
        let (_harness, visual) = harness(cx, tree);
        let (top, low) = (pane_bounds(visual, right), pane_bounds(visual, bottom));
        approx(
            top.size.width,
            500.0,
            "stacked panes share the column width",
        );
        approx(
            low.size.width,
            500.0,
            "stacked panes share the column width",
        );
        approx(top.size.height, 300.0, "top half");
        approx(low.size.height, 300.0, "bottom half");
        assert!(low.top() >= top.bottom() - px(1.0), "below, not beside");
    }

    #[gpui::test]
    fn removing_a_pane_relayouts_without_stale_sizes(cx: &mut TestAppContext) {
        let (mut tree, left, right) = two_pane_tree(0.5);
        tree.add_tab(right, 3, None);
        assert!(tree.move_tab(
            3,
            PaneDrop::Split {
                pane: right,
                edge: Edge::Right
            }
        ));
        let third = tree.pane_of(3).unwrap();
        let (harness, visual) = harness(cx, tree);
        approx(pane_bounds(visual, third).size.width, 250.0, "quarter");
        // Dropping the middle pane re-keys the split: no leftover state.
        harness.update(visual, |harness, cx| {
            harness.tree.remove_tab(2);
            cx.notify();
        });
        settle(visual);
        // The survivors keep their relative shares: 0.5 : 0.25 -> 2/3 : 1/3.
        approx(pane_bounds(visual, left).size.width, 666.7, "left");
        approx(pane_bounds(visual, third).size.width, 333.3, "right");
    }

    #[gpui::test]
    fn a_maximized_pane_is_rendered_alone(cx: &mut TestAppContext) {
        let (mut tree, left, right) = two_pane_tree(0.5);
        assert!(tree.toggle_maximize(right));
        let (_harness, visual) = harness(cx, tree);
        approx(
            pane_bounds(visual, right).size.width,
            1000.0,
            "fills the area",
        );
        assert!(visual.debug_bounds("pane-1").is_none() || left == right);
    }

    #[gpui::test]
    fn dragging_over_a_pane_shows_the_zone_overlay_and_dropping_reports_it(
        cx: &mut TestAppContext,
    ) {
        let (tree, left, right) = two_pane_tree(0.5);
        let (harness, visual) = harness(cx, tree);
        let source = visual
            .debug_bounds("drag-source")
            .expect("drag source")
            .center();
        let b = pane_bounds(visual, right);
        let cases = [
            (
                point(b.left() + b.size.width * 0.9, b.center().y),
                DropZone::Edge(Edge::Right),
                (0.5, 0.0, 0.5, 1.0),
            ),
            (
                point(b.left() + b.size.width * 0.1, b.center().y),
                DropZone::Edge(Edge::Left),
                (0.0, 0.0, 0.5, 1.0),
            ),
            (
                point(b.center().x, b.top() + b.size.height * 0.1),
                DropZone::Edge(Edge::Top),
                (0.0, 0.0, 1.0, 0.5),
            ),
            (
                point(b.center().x, b.top() + b.size.height * 0.9),
                DropZone::Edge(Edge::Bottom),
                (0.0, 0.5, 1.0, 0.5),
            ),
            (b.center(), DropZone::Center, (0.0, 0.0, 1.0, 1.0)),
        ];
        for (target, zone, (fx, fy, fw, fh)) in cases {
            let left_button = Some(MouseButton::Left);
            visual.simulate_mouse_down(source, MouseButton::Left, Default::default());
            visual.simulate_mouse_move(
                source + point(px(8.0), px(0.0)),
                left_button,
                Default::default(),
            );
            visual.simulate_mouse_move(target, left_button, Default::default());
            visual.simulate_mouse_move(
                target + point(px(1.0), px(0.0)),
                left_button,
                Default::default(),
            );
            settle(visual);
            harness.read_with(visual, |harness, _| {
                assert_eq!(
                    harness.hover,
                    Some(DropHover { pane: right, zone }),
                    "{zone:?}"
                );
            });
            let overlay = visual
                .debug_bounds("pane-drop-overlay")
                .expect("overlay while hovering");
            approx(
                overlay.left(),
                f32::from(b.left()) + fx * f32::from(b.size.width),
                "overlay x",
            );
            approx(
                overlay.top(),
                f32::from(b.top()) + fy * f32::from(b.size.height),
                "overlay y",
            );
            approx(
                overlay.size.width,
                fw * f32::from(b.size.width),
                "overlay width",
            );
            approx(
                overlay.size.height,
                fh * f32::from(b.size.height),
                "overlay height",
            );
            visual.simulate_mouse_up(target, MouseButton::Left, Default::default());
            settle(visual);
            harness.update(visual, |harness, _| {
                assert_eq!(
                    harness.dropped.take(),
                    Some(DropHover { pane: right, zone })
                );
            });
            assert!(
                visual.debug_bounds("pane-drop-overlay").is_none(),
                "overlay gone after the drop"
            );
        }
        let _ = left;
    }
}
