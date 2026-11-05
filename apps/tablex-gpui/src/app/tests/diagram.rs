// ER diagram presentation: the rail button, the view's interactions (drag,
// pan, zoom, fit, hover/select, double-click) and a real SQLite database
// described by the real driver.

use crate::app::results::diagram::layout::AttributeMode as DiagramMode;
use crate::app::results::diagram::model::EntityState as DiagramEntityState;
use crate::app::results::diagram::view::DiagramView;
use crate::columns_panel::Presentation as DiagramPresentation;

fn diagram_view(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> gpui::Entity<DiagramView> {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.diagram_views.get(&tab).cloned().expect("diagram view")
    })
}

fn diagram_open(visual: &mut gpui::VisualTestContext) {
    let button = visual.debug_bounds("diagram-rail-toggle").expect("Diagram rail button");
    visual.simulate_click(button.center(), Default::default());
    settle(visual);
}

fn presentation(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> DiagramPresentation {
    app.read_with(visual, |this, _| {
        let tab = this.workspace.active_tab().unwrap();
        this.workspace.runtime(tab.id).unwrap().columns_panel.presentation
    })
}

fn entity_bounds(visual: &mut gpui::VisualTestContext, index: usize) -> gpui::Bounds<gpui::Pixels> {
    let selector: &'static str = Box::leak(format!("diagram-entity-{index}").into_boxed_str());
    visual.debug_bounds(selector).unwrap_or_else(|| panic!("entity {index} is rendered"))
}

fn zoom_of(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> f32 {
    view.read_with(visual, |view, _| view.viewport.zoom)
}

fn diagram_click(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    let bounds = visual.debug_bounds(selector).unwrap_or_else(|| panic!("{selector} is rendered"));
    visual.simulate_click(bounds.center(), Default::default());
    settle(visual);
}

/// Clicks `selector`, which opens the review dialog. Its entrance and exit run on the executor's clock,
/// which these tests leave still: the dialog stays where it is first painted, so clicks land on what
/// `debug_bounds` reports, and a close unmounts it at once.
fn open_review(visual: &mut gpui::VisualTestContext, selector: &'static str) {
    diagram_click(visual, selector);
}

fn diagram_double_click(visual: &mut gpui::VisualTestContext, position: gpui::Point<gpui::Pixels>) {
    visual.simulate_event(gpui::MouseDownEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui::MouseUpEvent {
        position,
        modifiers: Default::default(),
        button: gpui::MouseButton::Left,
        click_count: 2,
    });
    settle(visual);
}

/// Opens the column-visibility dropdown and picks `mode` with the keyboard.
fn choose_mode(visual: &mut gpui::VisualTestContext, mode: DiagramMode) {
    diagram_click(visual, "diagram-mode");
    let index = DiagramMode::ALL.iter().position(|candidate| *candidate == mode).unwrap();
    for _ in 0..=index {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
}

fn drag(
    visual: &mut gpui::VisualTestContext,
    from: gpui::Point<gpui::Pixels>,
    delta: gpui::Point<gpui::Pixels>,
) {
    visual.simulate_mouse_down(from, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(from + delta * 0.5, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_move(from + delta, gpui::MouseButton::Left, Default::default());
    visual.simulate_mouse_up(from + delta, gpui::MouseButton::Left, Default::default());
    settle(visual);
}

#[gpui::test]
fn diagram_rail_button_sits_under_properties_for_sql_tables_only(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    let properties = visual.debug_bounds("properties-rail-button").expect("Properties button");
    let diagram = visual.debug_bounds("diagram-rail-toggle").expect("Diagram button");
    assert!(diagram.top() >= properties.bottom(), "directly below Properties");
    assert!(diagram.top() - properties.bottom() < gpui::px(8.0), "right below, not elsewhere");
    assert_eq!((diagram.size, diagram.center().x), (properties.size, properties.center().x));
    assert_eq!(presentation(&app, visual), DiagramPresentation::Grid);
    assert!(visual.debug_bounds("diagram-view").is_none());

    // Exclusive presentations: Diagram replaces the grid, Properties replaces
    // the diagram, Grid brings the grid back.
    diagram_open(visual);
    assert_eq!(presentation(&app, visual), DiagramPresentation::Diagram);
    assert!(visual.debug_bounds("diagram-view").is_some());
    assert!(visual.debug_bounds("grid-scroll-area").is_none(), "the diagram replaces the grid");
    diagram_click(visual, "properties-rail-button");
    assert_eq!(presentation(&app, visual), DiagramPresentation::Columns);
    assert!(visual.debug_bounds("diagram-view").is_none());
    diagram_open(visual);
    diagram_click(visual, "columns-rail-grid-button");
    assert_eq!(presentation(&app, visual), DiagramPresentation::Grid);
    assert!(visual.debug_bounds("diagram-view").is_none());
    assert!(visual.debug_bounds("grid-scroll-area").is_some(), "the grid is back");
}

#[gpui::test]
fn diagram_rail_button_is_hidden_for_mongo_and_non_table_tabs(cx: &mut TestAppContext) {
    let (_, visual) = context_test_app(cx, "mongo-grid", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1100.0), gpui::px(850.0)));
    settle(visual);
    assert!(visual.debug_bounds("properties-rail-button").is_some(), "the rail itself renders");
    assert!(visual.debug_bounds("diagram-rail-toggle").is_none(), "no diagram for Mongo");
}

#[gpui::test]
fn diagram_rail_button_is_hidden_for_a_sql_query_tab(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    assert!(visual.debug_bounds("diagram-rail-toggle").is_some(), "offered for the table tab");
    // The same tab as a free-form SQL query: no table identity, no diagram.
    app.update(visual, |this, cx| {
        this.workspace.tab_mut(tab_id).unwrap().query_mode = None;
        cx.notify();
    });
    settle(visual);
    assert!(visual.debug_bounds("properties-rail-button").is_some(), "the rail itself renders");
    assert!(visual.debug_bounds("diagram-rail-toggle").is_none());
}

#[gpui::test]
fn diagram_shows_related_entities_and_supports_drag_pan_zoom_fit_and_open(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    visual.update(|window, _| window.refresh());
    diagram_open(visual);
    let view = diagram_view(&app, visual);

    // Focus `orders` + customers/regions (parents) + order_items (child).
    let labels = view.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().expect("snapshot").graph;
        assert!(graph.entities.iter().all(|entity| entity.state == DiagramEntityState::Ready));
        assert_eq!(graph.relations.len(), 3);
        graph.entities.iter().map(|entity| entity.label.clone()).collect::<Vec<_>>()
    });
    assert_eq!(labels, ["orders", "customers", "order_items", "regions"]);
    for index in 0..4 {
        entity_bounds(visual, index);
    }
    for selector in ["diagram-zoom-out", "diagram-zoom-in", "diagram-fit", "diagram-relayout", "diagram-mode"] {
        assert!(visual.debug_bounds(selector).is_some(), "{selector} in the toolbar");
    }
    let area = visual.debug_bounds("diagram-area").expect("area");
    for index in 0..4 {
        let bounds = entity_bounds(visual, index);
        assert!(area.contains(&bounds.origin) && area.contains(&bounds.bottom_right()), "fit shows entity {index}");
    }
    // Parents are right of the focus, the child is left.
    let (focus, customers, order_items) =
        (entity_bounds(visual, 0), entity_bounds(visual, 1), entity_bounds(visual, 2));
    assert!(customers.left() > focus.right() && order_items.right() < focus.left());

    // Hover highlights, click selects.
    visual.simulate_mouse_move(customers.center(), None, Default::default());
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.hovered.map(|id| id.0)), Some(1));
    visual.simulate_click(customers.center(), Default::default());
    assert_eq!(view.read_with(visual, |view, _| view.selected.map(|id| id.0)), Some(1));
    let empty = area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0));
    visual.simulate_click(empty, Default::default());
    assert_eq!(view.read_with(visual, |view, _| view.selected), None, "background click deselects");

    // Dragging an entity moves it and its line endpoints follow.
    let before = entity_bounds(visual, 1);
    let end_before = view.read_with(visual, |view, _| *view.routes[1].points.last().unwrap());
    drag(visual, before.center(), gpui::point(gpui::px(40.0), gpui::px(30.0)));
    let after = entity_bounds(visual, 1);
    assert_eq!(after.origin - before.origin, gpui::point(gpui::px(40.0), gpui::px(30.0)));
    view.read_with(visual, |view, _| {
        let end = *view.routes[1].points.last().unwrap();
        assert!(end.x > end_before.x && end.y > end_before.y, "the line endpoint moved with the entity");
        assert_eq!(end.x, view.layout.rects[1].x, "and still touches the entity's left edge");
    });
    assert_eq!(entity_bounds(visual, 0), focus, "other entities stay put");

    // Dragging the background pans every entity.
    let focus_before = entity_bounds(visual, 0);
    drag(visual, empty, gpui::point(gpui::px(25.0), gpui::px(15.0)));
    assert_eq!(entity_bounds(visual, 0).origin - focus_before.origin, gpui::point(gpui::px(25.0), gpui::px(15.0)));

    // Zoom buttons change the scale; clamped to 50%..200%.
    let zoom = zoom_of(&view, visual);
    diagram_click(visual, "diagram-zoom-in");
    assert!((zoom_of(&view, visual) - zoom * 1.25).abs() < 1e-4);
    assert!(visual.debug_bounds("diagram-zoom-label").is_some());
    for _ in 0..12 {
        diagram_click(visual, "diagram-zoom-in");
    }
    assert_eq!(zoom_of(&view, visual), 2.0);
    for _ in 0..12 {
        diagram_click(visual, "diagram-zoom-out");
    }
    assert_eq!(zoom_of(&view, visual), 0.5);
    // Cmd+scroll zooms around the cursor, plain scroll pans.
    let zoom = zoom_of(&view, visual);
    let scroll = |delta: f32, modifiers: gpui::Modifiers| gpui::ScrollEvent {
        position: area.center(),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.0), gpui::px(delta))),
        modifiers,
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    };
    visual.simulate_event(scroll(-40.0, gpui::Modifiers { platform: true, ..Default::default() }));
    settle(visual);
    assert!(zoom_of(&view, visual) > zoom, "Cmd+scroll up zooms in");
    let origin = entity_bounds(visual, 0).origin;
    visual.simulate_event(scroll(30.0, Default::default()));
    settle(visual);
    assert_eq!(entity_bounds(visual, 0).origin.y - origin.y, gpui::px(30.0), "plain scroll pans");

    // Fit brings everything back into view at a sensible scale.
    diagram_click(visual, "diagram-fit");
    for index in 0..4 {
        let bounds = entity_bounds(visual, index);
        assert!(area.contains(&bounds.origin) && area.contains(&bounds.bottom_right()), "Fit shows entity {index}");
    }
    // Re-layout restores the automatic positions (undoing the drag).
    diagram_click(visual, "diagram-relayout");
    assert_eq!(entity_bounds(visual, 1).size, before.size);
    assert_eq!(entity_bounds(visual, 0).left() - entity_bounds(visual, 1).left(), focus.left() - before.left());

    // Double-click opens the related table in a new tab.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let customers = entity_bounds(visual, 1);
    diagram_double_click(visual, customers.center());
    app.read_with(visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs + 1);
        let tab = this.workspace.active_tab().unwrap();
        assert_eq!((tab.collection.as_str(), tab.schema.as_deref()), ("customers", Some("public")));
        assert!(tab.query_mode.is_table_data());
    });
    // The orders tab keeps its diagram (and its manual layout state).
    assert!(app.read_with(visual, |this, _| this.diagram_views.contains_key(&tab_id)));
}

const CHINOOK_SEED: &str = "CREATE TABLE artists(artist_id INTEGER PRIMARY KEY, name TEXT NOT NULL); \
     CREATE TABLE albums(album_id INTEGER PRIMARY KEY, title TEXT NOT NULL, \
       artist_id INTEGER NOT NULL REFERENCES artists(artist_id)); \
     CREATE TABLE genres(genre_id INTEGER PRIMARY KEY, name TEXT); \
     CREATE TABLE tracks(track_id INTEGER PRIMARY KEY, name TEXT NOT NULL, \
       album_id INTEGER REFERENCES albums(album_id), genre_id INTEGER REFERENCES genres(genre_id), \
       unit_price NUMERIC NOT NULL, added DATE); \
     CREATE TABLE invoice_lines(line_id INTEGER PRIMARY KEY, \
       track_id INTEGER NOT NULL REFERENCES tracks(track_id), quantity INTEGER NOT NULL); \
     INSERT INTO artists VALUES (1, 'AC/DC'); INSERT INTO albums VALUES (1, 'Back in Black', 1); \
     INSERT INTO genres VALUES (1, 'Rock'); INSERT INTO tracks VALUES (1, 'Hells Bells', 1, 1, 0.99, '2020-01-01'); \
     INSERT INTO invoice_lines VALUES (1, 1, 2);";

#[gpui::test]
fn diagram_of_a_real_sqlite_table_shows_keys_rows_and_lines_at_the_fk_rows(cx: &mut TestAppContext) {
    let db = SqliteDb::new(CHINOOK_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table("tracks");
    settle(rig.visual);
    let (app, tab) = (rig.app.clone(), rig.tab);

    diagram_open(rig.visual);
    wait_until(rig.visual, "the diagram to load", |visual| {
        let Some(view) = app.read_with(visual, |this, _| this.diagram_views.get(&tab).cloned()) else {
            return false;
        };
        view.read_with(visual, |view, _| {
            view.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.graph.entities.len() == 4
                    && snapshot.graph.entities.iter().all(|entity| entity.state == DiagramEntityState::Ready)
            })
        })
    });
    settle(rig.visual);
    let view = diagram_view(&app, rig.visual);
    let (labels, relations) = view.read_with(rig.visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        (
            graph.entities.iter().map(|entity| entity.label.clone()).collect::<Vec<_>>(),
            graph.relations.len(),
        )
    });
    // Depth 1: artists is two hops away and stays out.
    assert_eq!(labels, ["tracks", "albums", "genres", "invoice_lines"]);
    assert_eq!(relations, 3);

    // PK first with the key marker, then the rest; the PK name is bold.
    let area = rig.visual.debug_bounds("diagram-area").unwrap();
    let row = |visual: &mut gpui::VisualTestContext, entity: usize, column: usize| {
        let selector: &'static str = Box::leak(format!("diagram-row-{entity}-{column}").into_boxed_str());
        visual.debug_bounds(selector)
    };
    let tracks_columns = view.read_with(rig.visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.entities[0]
            .columns
            .iter()
            .map(|column| (column.name.clone(), column.primary_key, column.foreign_key))
            .collect::<Vec<_>>()
    });
    assert_eq!(tracks_columns[0], ("track_id".to_string(), true, false));
    assert!(tracks_columns.iter().any(|(name, _, fk)| name == "album_id" && *fk));
    for column in 0..tracks_columns.len() {
        assert!(row(rig.visual, 0, column).is_some(), "row {column} of tracks is drawn");
    }

    // Each line starts at the vertical centre of its FK row and ends at the
    // PK row of the referenced table.
    let to_window_y = |view: &DiagramView, y: f32| {
        f32::from(area.origin.y) + (y - view.viewport.origin.y) * view.viewport.zoom
    };
    let anchors = view.read_with(rig.visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        view.routes
            .iter()
            .map(|routed| {
                let relation = &graph.relations[routed.relation];
                (
                    relation.child.0,
                    relation.child_columns[0],
                    to_window_y(view, routed.points.first().unwrap().y),
                    relation.parent.0,
                    relation.parent_columns[0],
                    to_window_y(view, routed.points.last().unwrap().y),
                )
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(anchors.len(), 3);
    for (child, child_column, start_y, parent, parent_column, end_y) in anchors {
        let child_row = row(rig.visual, child, child_column).expect("FK row drawn");
        let parent_row = row(rig.visual, parent, parent_column).expect("referenced row drawn");
        assert!((f32::from(child_row.center().y) - start_y).abs() < 1.0, "line starts at the FK row");
        assert!((f32::from(parent_row.center().y) - end_y).abs() < 1.0, "line ends at the referenced row");
    }

    // Keys only hides the plain columns and keeps the line anchors.
    let name_column = tracks_columns.iter().position(|(name, _, _)| name == "name").unwrap();
    let height_all = entity_bounds(rig.visual, 0).size.height;
    choose_mode(rig.visual, DiagramMode::KeysOnly);
    assert_eq!(view.read_with(rig.visual, |view, _| view.mode), DiagramMode::KeysOnly);
    assert!(row(rig.visual, 0, name_column).is_none(), "plain columns are hidden");
    assert!(entity_bounds(rig.visual, 0).size.height < height_all);
    let (fk_row, start_y) = view.read_with(rig.visual, |view, _| {
        let relation = &view.snapshot.as_ref().unwrap().graph.relations[view.routes[0].relation];
        (relation.child_columns[0], to_window_y(view, view.routes[0].points[0].y))
    });
    let child = view.read_with(rig.visual, |view, _| {
        view.snapshot.as_ref().unwrap().graph.relations[view.routes[0].relation].child.0
    });
    let fk_bounds = row(rig.visual, child, fk_row).expect("FK row stays visible");
    assert!((f32::from(fk_bounds.center().y) - start_y).abs() < 1.0);
    choose_mode(rig.visual, DiagramMode::All);
    assert_eq!(view.read_with(rig.visual, |view, _| view.mode), DiagramMode::All);

    // Double-clicking a related table opens its table tab.
    let tabs = app.read_with(rig.visual, |this, _| this.workspace.tabs().len());
    let albums = entity_bounds(rig.visual, 1);
    diagram_double_click(rig.visual, albums.center());
    app.read_with(rig.visual, |this, _| {
        assert_eq!(this.workspace.tabs().len(), tabs + 1);
        assert_eq!(this.workspace.active_tab().unwrap().collection, "albums");
    });
}


// ---- Whole-schema diagram (no focus entity, culling, progress) ----

/// `tables` synthetic tables `t000..`: `id` PK, a `ref_id` FK to an earlier
/// table and eight plain columns each.
fn diagram_schema_snapshot(
    tables: usize,
    generation: u64,
    progress: Option<(usize, usize)>,
) -> crate::app::results::diagram::data::DiagramSnapshot {
    diagram_schema_snapshot_loaded(tables, tables, generation, progress)
}

/// Like [`diagram_schema_snapshot`] with only the first `ready` tables
/// described (the rest are still loading placeholders).
fn diagram_schema_snapshot_loaded(
    tables: usize,
    ready: usize,
    generation: u64,
    progress: Option<(usize, usize)>,
) -> crate::app::results::diagram::data::DiagramSnapshot {
    use crate::app::results::diagram::model::{DiagramGraph, RelatedProperties, TableRef};
    use crate::catalog::{CatalogColumn, CatalogForeignKey, TableProperties};
    let table = |index: usize| TableRef {
        database: "db".into(),
        schema: Some("public".into()),
        table: format!("t{index:03}"),
    };
    let column = |name: &str, ty: &str, pk: bool| CatalogColumn {
        name: name.into(),
        data_type: ty.into(),
        nullable: !pk,
        primary_key: pk,
        ..Default::default()
    };
    let entries: Vec<_> = (0..tables)
        .map(|index| {
            let mut columns = vec![column("id", "integer", true), column("ref_id", "integer", false)];
            columns.extend((0..8).map(|n| column(&format!("c{n}"), "varchar(40)", false)));
            let foreign_keys = (index > 0).then(|| {
                vec![CatalogForeignKey {
                    name: format!("fk_{index}"),
                    table: format!("public.t{index:03}"),
                    columns: vec!["ref_id".into()],
                    referenced_table: format!("public.t{:03}", (index * 7) % index),
                    referenced_columns: vec!["id".into()],
                    on_update: None,
                    on_delete: None,
                    definition: None,
                }]
            });
            let properties = TableProperties { columns, foreign_keys, ..Default::default() };
            let state = if index < ready {
                RelatedProperties::Ready(std::sync::Arc::new(properties))
            } else {
                RelatedProperties::Loading
            };
            (table(index), state)
        })
        .collect();
    crate::app::results::diagram::data::DiagramSnapshot {
        graph: std::sync::Arc::new(DiagramGraph::build_schema(&entries)),
        generation,
        error: None,
        progress,
        truncated: None,
    }
}

/// A bare window hosting one schema `DiagramView` (production theme).
fn diagram_schema_window(
    cx: &mut TestAppContext,
    snapshot: crate::app::results::diagram::data::DiagramSnapshot,
) -> (gpui::Entity<DiagramView>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| DiagramView::new(crate::app::results::diagram::view::DiagramKind::Schema, cx));
        gpui_component::Root::new(view, window, cx)
    });
    let view = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<DiagramView>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    view.update(visual, |view, cx| view.set_snapshot(Some(snapshot), cx));
    settle(visual);
    (view, visual)
}

fn rendered_entities(visual: &mut gpui::VisualTestContext, total: usize) -> Vec<usize> {
    (0..total)
        .filter(|index| {
            let selector: &'static str = Box::leak(format!("diagram-entity-{index}").into_boxed_str());
            visual.debug_bounds(selector).is_some()
        })
        .collect()
}

#[gpui::test]
fn diagram_schema_view_culls_reports_progress_and_opens_tables(cx: &mut TestAppContext) {
    use crate::app::results::diagram::view::DiagramEvent;
    const TABLES: usize = 300;
    let (view, visual) = diagram_schema_window(cx, diagram_schema_snapshot(TABLES, 1, Some((120, TABLES))));
    let opened = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
    let sink = opened.clone();
    visual.update(|_, app| {
        app.subscribe(&view, move |_, event: &DiagramEvent, _| {
            if let DiagramEvent::OpenTable(table) = event {
                sink.borrow_mut().push(table.table.clone());
            }
        })
        .detach();
    });

    // No focus entity; progress and the table count are in the toolbar.
    assert!(visual.debug_bounds("diagram-progress").is_some(), "loading progress shown");
    assert!(visual.debug_bounds("diagram-table-count").is_some());
    view.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        assert_eq!((graph.entities.len(), graph.entities.iter().any(|entity| entity.focus)), (TABLES, false));
        assert!(!view.routes.is_empty());
        assert_eq!(view.layout.rects.len(), TABLES);
    });

    // Only the entities intersecting the area exist as elements.
    let first = rendered_entities(visual, TABLES);
    assert!(!first.is_empty() && first.len() < TABLES / 2, "culled: {} of {TABLES}", first.len());

    // Panning to the far corner of the diagram swaps the rendered set.
    view.update(visual, |view, cx| {
        let bounds = view.layout.bounds();
        view.viewport.origin = crate::app::results::diagram::layout::DiagramPoint {
            x: bounds.x + bounds.width - 600.0,
            y: bounds.y + bounds.height - 400.0,
        };
        cx.notify();
    });
    settle(visual);
    let far = rendered_entities(visual, TABLES);
    assert!(!far.is_empty() && far.iter().all(|index| !first.contains(index)), "a different set after panning");

    // Fit works for a large graph: the schema minimum (10%), top-left first.
    diagram_click(visual, "diagram-fit");
    assert_eq!(zoom_of(&view, visual), 0.1);
    let again = rendered_entities(visual, TABLES);
    assert_eq!(again, first, "Fit returns to the first screen of a large diagram");

    // Double-click opens the table; loading finishing removes the progress.
    let target = again[0];
    let selector: &'static str = Box::leak(format!("diagram-entity-{target}").into_boxed_str());
    let bounds = visual.debug_bounds(selector).unwrap();
    diagram_double_click(visual, bounds.center());
    assert_eq!(opened.borrow().as_slice(), [format!("t{target:03}")]);
    view.update(visual, |view, cx| view.set_snapshot(Some(diagram_schema_snapshot(TABLES, 2, None)), cx));
    settle(visual);
    assert!(visual.debug_bounds("diagram-progress").is_none());
    assert_eq!(rendered_entities(visual, TABLES), first, "re-layout on progress keeps the layout deterministic");
}

/// On-demand frame-time / allocation report for pan and zoom of a 300-table
/// schema diagram (CPU side of a frame: event, render, layout, prepaint,
/// paint into the scene; no GPU submission):
///   cargo test --release -p tablex-gpui diagram_schema_frame_times -- --ignored --nocapture
#[gpui::test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture"]
fn diagram_schema_frame_times(cx: &mut TestAppContext) {
    // `DIAGRAM_BENCH_TABLES=200 cargo test --release …` for another size.
    let tables: usize = std::env::var("DIAGRAM_BENCH_TABLES").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    const FRAMES: usize = 200;
    let (view, visual) = diagram_schema_window(cx, diagram_schema_snapshot(tables, 1, None));
    // Debug selectors are compiled out of release builds: fixed cursor position.
    let scroll = |delta: (f32, f32), modifiers: gpui::Modifiers| gpui::ScrollEvent {
        position: gpui::point(gpui::px(700.0), gpui::px(450.0)),
        delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(delta.0), gpui::px(delta.1))),
        modifiers,
        touch_phase: gpui::TouchPhase::Moved,
        source: gpui::ScrollSource::Pointer,
    };
    let report = |name: &str, mut times: Vec<f64>, allocs: u64, bytes: u64, frames: usize| {
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let at = |p: f64| times[((times.len() - 1) as f64 * p) as usize];
        eprintln!(
            "{name:>8}: p50 {:6.2} ms  p95 {:6.2} ms  max {:6.2} ms  allocs/frame {:6}  bytes/frame {:8}",
            at(0.5),
            at(0.95),
            times[times.len() - 1],
            allocs / frames as u64,
            bytes / frames as u64
        );
    };
    let frame = |visual: &mut gpui::VisualTestContext, event: Option<gpui::ScrollEvent>| {
        let (a0, b0) = (crate::grid_bench::allocation_count(), crate::grid_bench::allocation_bytes());
        let start = std::time::Instant::now();
        if let Some(event) = event {
            visual.simulate_event(event);
        }
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
        (
            start.elapsed().as_secs_f64() * 1000.0,
            crate::grid_bench::allocation_count() - a0,
            crate::grid_bench::allocation_bytes() - b0,
        )
    };
    for _ in 0..10 {
        frame(visual, None);
    }
    let run = |name: &str, visual: &mut gpui::VisualTestContext, make: &dyn Fn(usize) -> gpui::ScrollEvent| {
        let (mut times, mut allocs, mut bytes) = (Vec::new(), 0, 0);
        for index in 0..FRAMES {
            let (ms, a, b) = frame(visual, Some(make(index)));
            times.push(ms);
            allocs += a;
            bytes += b;
        }
        report(name, times, allocs, bytes, FRAMES);
    };
    // Park the viewport on the densest 50% screen of the diagram so the
    // frames below render as many boxes as any pan position can show.
    let densest = view.update(visual, |view, cx| {
        let probe = |origin: crate::app::results::diagram::layout::DiagramPoint| {
            let viewport = crate::app::results::diagram::layout::Viewport { origin, zoom: 0.5, min_zoom: 0.5 };
            view.layout.visible(&viewport, 1400.0, 810.0).count()
        };
        let best = view
            .layout
            .rects
            .iter()
            .map(|rect| crate::app::results::diagram::layout::DiagramPoint { x: rect.x - 20.0, y: rect.y - 20.0 })
            .max_by_key(|origin| probe(*origin))
            .unwrap();
        let count = probe(best);
        view.viewport = crate::app::results::diagram::layout::Viewport { origin: best, zoom: 1.0, min_zoom: 0.1 };
        cx.notify();
        count
    });
    eprintln!("densest 50% screen: {densest} of {tables} entities");
    settle(visual);
    let wave = |index: usize| ((index % 100) as f32 - 50.0) * 6.0 * if index % 200 < 100 { 1.0 } else { -1.0 };
    run("pan 100%", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    // What the D2 toolbar group (undo, redo, three panel toggles) costs.
    crate::app::results::diagram::view::ANNOTATE_TOOLBAR_ENABLED.with(|enabled| enabled.set(false));
    view.update(visual, |_, cx| cx.notify());
    settle(visual);
    run("no D2 bar", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    crate::app::results::diagram::view::ANNOTATE_TOOLBAR_ENABLED.with(|enabled| enabled.set(true));
    view.update(visual, |_, cx| cx.notify());
    settle(visual);
    // D3: an editable diagram has the Changes button in its toolbar; the worst
    // case is a pending change on every table the densest screen shows (badges,
    // marked columns, a few created tables), all drawn while panning.
    {
        use crate::app::structure_edit::EditScope;
        use crate::app::structure_edit::EditContext;
        use crate::sql_ddl::draft::Draft;
        use crate::sql_ddl::{Change, ColumnSpec, TableId};
        let scope = EditScope { kind: ConnectionKind::Postgres, database: "db".into(), schema: Some("public".into()) };
        view.update(visual, |view, cx| {
            let context = EditContext { scope: scope.clone(), subject: "bench".into(), confirm_execute: false, disabled: None, sql_mode: None, catalog: None, catalog_error: None };
            view.set_edit_context(Some(context), cx)
        });
        settle(visual);
        run("D3 bar", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
        let in_view: Vec<usize> = view.read_with(visual, |view, _| {
            view.layout.visible(&view.viewport, 1400.0, 810.0).map(|id| id.0).collect()
        });
        view.update(visual, |view, cx| {
            let mut draft = Draft::default();
            for (n, &index) in in_view.iter().enumerate() {
                let table = TableId::new(Some("public"), &format!("t{index:03}"));
                draft.push(Change::AddColumn {
                    table: table.clone(),
                    column: ColumnSpec { name: "added".into(), data_type: "integer".into(), nullable: true, ..ColumnSpec::default() },
                });
                if n % 3 == 0 {
                    draft.push(Change::DropColumn { table: table.clone(), column: "c1".into() });
                }
                if n % 4 == 0 {
                    draft.push(Change::RenameColumn { table, from: "c2".into(), to: "renamed".into() });
                }
            }
            for n in 0..5 {
                draft.push(Change::CreateTable {
                    table: TableId::new(Some("public"), &format!("created{n}")),
                    columns: vec![ColumnSpec { name: "id".into(), data_type: "integer".into(), nullable: false, ..ColumnSpec::default() }],
                    primary_key: vec!["id".into()],
                    comment: None,
                });
            }
            view.set_draft(std::sync::Arc::new(draft), cx);
        });
        settle(visual);
        let (pending, entities) = view.read_with(visual, |view, _| (view.pending_count(), view.snapshot.as_ref().unwrap().graph.entities.len()));
        eprintln!("D3 draft: {pending} pending changes, {entities} entities");
        run("D3 draft", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
        view.update(visual, |view, cx| {
            view.set_draft(Default::default(), cx);
            view.set_edit_context(None, cx);
            view.session.history.clear();
        });
        settle(visual);
    }
    // D2 worst case: everything the densest screen can show is annotated.
    // 50 notes, 20 logical relationships and 5 groups, all in view.
    let in_view: Vec<usize> = view.read_with(visual, |view, _| {
        view.layout.visible(&view.viewport, 1400.0, 810.0).map(|id| id.0).collect()
    });
    let annotate_densest = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        use crate::app::results::diagram::annotations::{RelationEnd, TableKey, SWATCHES};
        let in_view = in_view.clone();
        view.update(visual, |view, cx| {
            let origin = view.viewport.origin;
            let name = |index: usize| format!("t{index:03}");
            let end = |index: usize, column: &str| RelationEnd {
                schema: Some("public".into()),
                table: name(index),
                columns: vec![column.into()],
            };
            view.edit_annotations(cx, |a| {
                for n in 0..20 {
                    let (child, parent) = (in_view[n % in_view.len()], in_view[(n * 7 + 3) % in_view.len()]);
                    let column = format!("c{}", n % 8);
                    let _ = a.add_relation(end(child, &column), end(parent, "id"), n % 2 == 0, false);
                }
                for n in 0..50 {
                    let id = a.add_note(origin.x + 20.0 + (n % 10) as f32 * 130.0, origin.y + 20.0 + (n / 10) as f32 * 170.0);
                    if let Some(note) = a.note_mut(id) {
                        note.text = format!("Note {n}: remember to check this table before the release.");
                        note.width = 120.0;
                        note.height = 70.0;
                    }
                }
                // Five groups over what is in view.
                let size = (in_view.len() / 5).max(1);
                for (g, swatch) in SWATCHES.iter().take(5).enumerate() {
                    let members: Vec<TableKey> = in_view
                        .iter()
                        .skip(g * size)
                        .take(size)
                        .map(|&i| TableKey { schema: Some("public".into()), table: name(i) })
                        .collect();
                    if !members.is_empty() {
                        a.add_group(format!("Group {g}"), swatch.1, members);
                    }
                }
            });
        });
    };
    annotate_densest(&view, visual);
    settle(visual);
    let (notes, relations, groups) =
        view.read_with(visual, |view, _| (view.annotations.notes.len(), view.annotations.relations.len(), view.annotations.groups.len()));
    eprintln!("annotated: {notes} notes, {relations} logical relationships, {groups} groups");
    run("annot 100%", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    view.update(visual, |view, cx| {
        view.edit_annotations(cx, |a| *a = Default::default());
        view.session.history.clear();
    });
    settle(visual);
    view.update(visual, |view, cx| {
        view.viewport = crate::app::results::diagram::layout::Viewport { origin: view.viewport.origin, zoom: 1.0, min_zoom: 0.1 };
        cx.notify();
    });
    settle(visual);
    // The same pan with the plain notation (no crow's foot ends): the cost of
    // the notation is the difference.
    view.update(visual, |view, cx| {
        view.notation = crate::app::results::diagram::notation::RelationNotation::Plain;
        cx.notify();
    });
    settle(visual);
    run("plain 100%", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    view.update(visual, |view, cx| {
        view.notation = crate::app::results::diagram::notation::RelationNotation::CrowsFoot;
        cx.notify();
    });
    settle(visual);
    // What the minimap costs: the same pan without it.
    crate::app::results::diagram::view::MINIMAP_ENABLED.with(|enabled| enabled.set(false));
    view.update(visual, |_, cx| cx.notify());
    settle(visual);
    run("no minimap", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    crate::app::results::diagram::view::MINIMAP_ENABLED.with(|enabled| enabled.set(true));
    view.update(visual, |_, cx| cx.notify());
    settle(visual);
    run("zoom", visual, &|index| {
        let delta = if (index / 20) % 2 == 0 { -30.0 } else { 30.0 };
        scroll((0.0, delta), gpui::Modifiers { platform: true, ..Default::default() })
    });
    view.update(visual, |view, cx| {
        view.viewport.zoom = 0.5;
        cx.notify();
    });
    settle(visual);
    let shown = view.read_with(visual, |view, _| view.layout.visible(&view.viewport, 1400.0, 810.0).count());
    eprintln!("entities in the 50% screen at the start of the pan: {shown}");
    run("pan 50%", visual, &|index| scroll((wave(index), wave(index) / 2.0), Default::default()));
    // Idle redraws: nothing changed, nothing is re-laid out.
    let (mut times, mut allocs, mut bytes) = (Vec::new(), 0, 0);
    for _ in 0..FRAMES {
        let (ms, a, b) = frame(visual, None);
        times.push(ms);
        allocs += a;
        bytes += b;
    }
    report("redraw", times, allocs, bytes, FRAMES);
    let visible = view.read_with(visual, |view, _| view.layout.visible(&view.viewport, 1400.0, 810.0).count());
    eprintln!("entities in view at 50%: {visible} of {tables}");
}

// ---- Whole-schema diagram tab: sidebar menu, palette, scenario, session ----

fn diagram_active_tab(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> crate::workspace::Tab {
    app.read_with(visual, |this, _| this.workspace.active_tab().cloned().expect("an active tab"))
}

/// Waits until the active diagram tab's view has described every table.
fn diagram_wait_loaded(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> usize {
    wait_until(visual, "the schema diagram to load", |visual| {
        let Some(view) = app.read_with(visual, |this, _| {
            let tab = this.workspace.active_tab()?.id;
            this.diagram_views.get(&tab).cloned()
        }) else {
            return false;
        };
        view.read_with(visual, |view, _| {
            view.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.progress.is_none()
                    && !snapshot.graph.entities.is_empty()
                    && snapshot.graph.entities.iter().all(|entity| entity.state == DiagramEntityState::Ready)
            })
        })
    });
    settle(visual);
    let view = diagram_view(app, visual);
    view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities.len())
}

#[gpui::test]
fn sidebar_view_diagram_opens_one_schema_tab_that_loads_and_opens_tables(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    // Events hit the last painted frame: paint the expanded tree first.
    settle(visual);
    let index = app.update(visual, |this, _| {
        this.navigator
            .visible_rows()
            .iter()
            .position(|row| {
                matches!(&row.id, crate::navigator::NavigatorNodeId::Schema { database, schema, .. }
                    if database == "warehouse" && schema == "public")
            })
            .expect("warehouse.public row")
    });
    let selector: &'static str = Box::leak(format!("sidebar-row-{index}").into_boxed_str());
    let row = visual.debug_bounds(selector).expect("schema row bounds");
    visual.simulate_mouse_down(row.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(row.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    // Schema menu: New Query, View Diagram.
    for _ in 0..2 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
    let tab = diagram_active_tab(&app, visual);
    assert_eq!(tab.kind, crate::workspace::TabKind::Diagram);
    assert_eq!(tab.title, "public diagram");
    assert_eq!(
        (tab.connection_id.as_str(), tab.database.as_str(), tab.schema.as_deref()),
        (crate::ui_fixture::POSTGRES_ID, "warehouse", Some("public"))
    );
    assert!(visual.debug_bounds("diagram-view").is_some());
    let tables = diagram_wait_loaded(&app, visual);
    assert_eq!(tables, 20, "every table of the schema is an entity");
    assert!(visual.debug_bounds("diagram-table-count").is_some());
    assert!(visual.debug_bounds("diagram-progress").is_none(), "loading finished");

    // Asking again activates the same tab instead of opening another.
    let tabs = app.read_with(visual, |this, _| this.workspace.tabs().len());
    app.update(visual, |this, cx| {
        let id = crate::navigator::NavigatorNodeId::Schema {
            connection: crate::ui_fixture::POSTGRES_ID.into(),
            database: "warehouse".into(),
            schema: "public".into(),
        };
        this.open_diagram_for_node(&id, cx);
    });
    settle(visual);
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), tabs);

    // Double-click on an entity opens that table's data tab.
    let first = rendered_entities(visual, tables);
    assert!(!first.is_empty());
    let name = app
        .read_with(visual, |this, _| this.diagram_views[&tab.id].clone())
        .read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities[first[0]].table.table.clone());
    let selector: &'static str = Box::leak(format!("diagram-entity-{}", first[0]).into_boxed_str());
    let bounds = visual.debug_bounds(selector).unwrap();
    diagram_double_click(visual, bounds.center());
    let opened = diagram_active_tab(&app, visual);
    assert_eq!((opened.collection.clone(), opened.schema.as_deref()), (name, Some("public")));
    assert!(opened.query_mode.as_ref().is_some_and(|mode| mode.is_table_data()));
}

#[gpui::test]
fn palette_opens_the_diagram_by_action_and_by_cmd_enter_on_a_database(cx: &mut TestAppContext) {
    let (app, visual) = palette_app(cx);
    // Action: "View Diagram" uses the selected sidebar node (a schema here).
    let schema = crate::navigator::NavigatorNodeId::Schema {
        connection: crate::ui_fixture::POSTGRES_ID.into(),
        database: "warehouse".into(),
        schema: "public".into(),
    };
    app.update(visual, |this, _| this.navigator.select(schema.clone()));
    palette_open(visual);
    palette_type(visual, "view diagram");
    assert!(palette_titles(&palette_snapshot(&app, visual), "Actions").contains(&"View Diagram".to_string()));
    visual.simulate_keystrokes("enter");
    palette_settle(visual);
    let tab = diagram_active_tab(&app, visual);
    assert_eq!((tab.kind, tab.title.as_str()), (crate::workspace::TabKind::Diagram, "public diagram"));

    // Cmd+Enter on a database entry opens that database's diagram (its
    // loaded schemas include `public`).
    palette_open(visual);
    palette_type(visual, "warehouse");
    let ranked = palette_snapshot(&app, visual);
    assert!(palette_titles(&ranked, "Databases").contains(&"warehouse".to_string()), "{ranked:?}");
    visual.simulate_keystrokes("secondary-enter");
    palette_settle(visual);
    let before = app.read_with(visual, |this, _| this.workspace.tabs().len());
    let tab = diagram_active_tab(&app, visual);
    assert_eq!(tab.kind, crate::workspace::TabKind::Diagram);
    assert_eq!((tab.database.as_str(), tab.schema.as_deref()), ("warehouse", Some("public")));
    assert_eq!(app.read_with(visual, |this, _| this.workspace.tabs().len()), before);
}

#[gpui::test]
fn database_diagram_scenario_opens_the_chinook_schema_tab(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "database-diagram", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    let tab = diagram_active_tab(&app, visual);
    assert_eq!((tab.kind, tab.title.as_str()), (crate::workspace::TabKind::Diagram, "main diagram"));
    let tables = diagram_wait_loaded(&app, visual);
    assert_eq!(tables, crate::ui_fixture_diagram::table_names().len());
    let view = diagram_view(&app, visual);
    view.read_with(visual, |view, _| {
        let graph = &view.snapshot.as_ref().unwrap().graph;
        assert!(graph.relations.len() >= tables - 2, "the Chinook catalog is densely linked");
        assert!(graph.entities.iter().all(|entity| !entity.focus));
    });
    assert!(visual.debug_bounds("diagram-table-count").is_some());
    assert!(!rendered_entities(visual, tables).is_empty());
}

#[gpui::test]
fn table_diagram_scenario_opens_with_the_diagram_presentation(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "table-diagram", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    wait_until(visual, "the table diagram", |visual| visual.debug_bounds("diagram-entity-0").is_some());
    assert_eq!(presentation(&app, visual), DiagramPresentation::Diagram);
    assert!(visual.debug_bounds("diagram-view").is_some());
}

/// Every entity of the graph lies inside the measured area.
fn diagram_graph_in_view(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> bool {
    view.read_with(visual, |view, _| {
        let area = view.area.get().size;
        let bounds = view.layout.bounds();
        let top_left = view.viewport.to_screen(crate::app::results::diagram::layout::DiagramPoint {
            x: bounds.x,
            y: bounds.y,
        });
        let bottom_right = view.viewport.to_screen(crate::app::results::diagram::layout::DiagramPoint {
            x: bounds.x + bounds.width,
            y: bounds.y + bounds.height,
        });
        top_left.x >= -0.5
            && top_left.y >= -0.5
            && bottom_right.x <= f32::from(area.width) + 0.5
            && bottom_right.y <= f32::from(area.height) + 0.5
    })
}

#[gpui::test]
fn diagram_schema_view_refits_after_every_load_batch_until_the_user_takes_over(cx: &mut TestAppContext) {
    const TABLES: usize = 11;
    // Placeholders first: header-only boxes, then batches that grow them.
    let (view, visual) = diagram_schema_window(cx, diagram_schema_snapshot_loaded(TABLES, 0, 1, Some((0, TABLES))));
    assert!(diagram_graph_in_view(&view, visual), "fitted while everything is loading");
    for (generation, ready) in [(2, 4), (3, 8), (4, TABLES)] {
        let progress = (ready < TABLES).then_some((ready, TABLES));
        view.update(visual, |view, cx| {
            view.set_snapshot(Some(diagram_schema_snapshot_loaded(TABLES, ready, generation, progress)), cx)
        });
        settle(visual);
        assert!(diagram_graph_in_view(&view, visual), "the whole graph is in view after batch {generation}");
    }
    assert!(zoom_of(&view, visual) < 0.5, "11 tables of 10 rows need more than the table diagram's 50% floor");

    // After the user pans, later batches no longer move the viewport.
    let area = visual.debug_bounds("diagram-area").unwrap();
    drag(visual, area.origin + gpui::point(gpui::px(4.0), gpui::px(4.0)), gpui::point(gpui::px(30.0), gpui::px(20.0)));
    let origin = view.read_with(visual, |view, _| view.viewport.origin);
    view.update(visual, |view, cx| {
        view.set_snapshot(Some(diagram_schema_snapshot_loaded(TABLES, TABLES, 5, None)), cx)
    });
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.viewport.origin), origin);
    // Fit takes the view back to the whole graph and re-arms following.
    diagram_click(visual, "diagram-fit");
    assert!(diagram_graph_in_view(&view, visual));
}

#[gpui::test]
fn diagram_schema_view_zooms_out_to_ten_percent_but_the_table_view_stops_at_fifty(cx: &mut TestAppContext) {
    let (view, visual) = diagram_schema_window(cx, diagram_schema_snapshot(40, 1, None));
    for _ in 0..30 {
        diagram_click(visual, "diagram-zoom-out");
    }
    assert_eq!(zoom_of(&view, visual), 0.1);
    assert!(diagram_graph_in_view(&view, visual) || zoom_of(&view, visual) == 0.1);
    view.read_with(visual, |view, _| assert_eq!(view.viewport.min_zoom, 0.1));
    // Rows are drawn name-only below 75% and boxes lose their rows below 35%.
    assert!(!rendered_entities(visual, 40).is_empty());
}

fn zoom_text_of(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> String {
    view.read_with(visual, |view, cx| view.zoom_text(cx))
}

/// Focuses the zoom field with a real click and types `text` over its content.
fn type_zoom(visual: &mut gpui::VisualTestContext, text: &str) {
    let field = visual.debug_bounds("diagram-zoom-label").expect("zoom field");
    visual.simulate_click(field.center(), Default::default());
    settle(visual);
    visual.simulate_input(text);
    settle(visual);
}

fn zoom_diagram_window(
    cx: &mut TestAppContext,
) -> (gpui::Entity<DiagramView>, &mut gpui::VisualTestContext) {
    let (app, visual, _) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    // Focus / blur events only reach an active window.
    visual.update(|window, _| window.activate_window());
    diagram_open(visual);
    let view = diagram_view(&app, visual);
    (view, visual)
}

#[test]
fn zoom_text_accepts_percent_and_factor_spellings_only() {
    use crate::app::results::diagram::view::parse_zoom;
    for (text, expected) in [("120", 1.2), ("120%", 1.2), (" 75 % ", 0.75), ("1.2x", 1.2), ("1.5X", 1.5), ("62.5", 0.625)] {
        assert!((parse_zoom(text).unwrap() - expected).abs() < 1e-6, "{text}");
    }
    for text in ["", "%", "x", "abc", "-50", "0", "0%", "inf", "NaN", "1,2x", "12 0"] {
        assert_eq!(parse_zoom(text), None, "{text:?}");
    }
}

#[gpui::test]
fn zoom_field_applies_typed_values_on_enter(cx: &mut TestAppContext) {
    let (view, visual) = zoom_diagram_window(cx);
    assert_eq!(zoom_text_of(&view, visual), "100%", "the field shows the current zoom");
    for (typed, expected) in [("120", 1.2), ("80%", 0.8), ("1.5x", 1.5)] {
        type_zoom(visual, typed);
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert!((zoom_of(&view, visual) - expected).abs() < 1e-4, "{typed}");
        assert_eq!(zoom_text_of(&view, visual), format!("{}%", (expected * 100.0_f32).round()));
    }
}

#[gpui::test]
fn zoom_field_clamps_to_the_range_of_the_kind(cx: &mut TestAppContext) {
    let (view, visual) = zoom_diagram_window(cx);
    type_zoom(visual, "900");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(zoom_of(&view, visual), 2.0);
    assert_eq!(zoom_text_of(&view, visual), "200%");
    type_zoom(visual, "5");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(zoom_of(&view, visual), 0.5, "tables stop at 50%");
    assert_eq!(zoom_text_of(&view, visual), "50%");

    let (schema, visual) = diagram_schema_window(cx, diagram_schema_snapshot(40, 1, None));
    visual.update(|window, _| window.activate_window());
    type_zoom(visual, "5");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(zoom_of(&schema, visual), 0.1, "schemas go down to 10%");
    assert_eq!(zoom_text_of(&schema, visual), "10%");
}

#[gpui::test]
fn zoom_field_reverts_on_escape_and_on_invalid_text_without_a_popup(cx: &mut TestAppContext) {
    let (view, visual) = zoom_diagram_window(cx);
    type_zoom(visual, "150");
    assert_eq!(zoom_text_of(&view, visual), "150", "the typed text replaced the selection");
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(zoom_of(&view, visual), 1.0, "Escape discards the typed value");
    assert_eq!(zoom_text_of(&view, visual), "100%");

    type_zoom(visual, "banana");
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(zoom_of(&view, visual), 1.0);
    assert_eq!(zoom_text_of(&view, visual), "100%", "invalid text reverts");
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)), "no error popup");
}

#[gpui::test]
fn zoom_field_applies_on_blur(cx: &mut TestAppContext) {
    let (view, visual) = zoom_diagram_window(cx);
    type_zoom(visual, "75");
    // A click on the drawing area moves focus out of the field.
    let area = visual.debug_bounds("diagram-area").expect("area");
    visual.simulate_click(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), Default::default());
    settle(visual);
    assert!((zoom_of(&view, visual) - 0.75).abs() < 1e-4, "blur applies the typed zoom");
}

#[gpui::test]
fn zoom_menu_offers_presets_and_fit(cx: &mut TestAppContext) {
    let (view, visual) = zoom_diagram_window(cx);
    // Down N times then Enter picks the Nth entry of the open menu.
    for (downs, expected) in [(1, 0.5), (2, 0.75), (3, 1.0), (4, 1.5), (5, 2.0)] {
        diagram_click(visual, "diagram-zoom-presets");
        for _ in 0..downs {
            visual.simulate_keystrokes("down");
        }
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert!((zoom_of(&view, visual) - expected).abs() < 1e-4, "preset {expected}");
        assert_eq!(zoom_text_of(&view, visual), format!("{}%", (expected * 100.0_f32).round()));
    }
    // Fit returns the small diagram to the whole graph (100%, never magnified).
    diagram_click(visual, "diagram-zoom-presets");
    for _ in 0..6 {
        visual.simulate_keystrokes("down");
    }
    visual.simulate_keystrokes("enter");
    settle(visual);
    assert_eq!(zoom_of(&view, visual), 1.0);
    assert_eq!(zoom_text_of(&view, visual), "100%");
    assert!(view.read_with(visual, |view, _| view.auto_fit), "Fit re-arms following");
}

// ---- D1: saved positions, search, minimap, notation, modes, JOIN SQL, export ----------

fn d1_temp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("tablex-diagram-{label}-{}", uuid::Uuid::new_v4()))
}

/// Points the app's diagram positions at `dir` (tests never touch the real
/// data directory).
fn d1_use_store(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, dir: &std::path::Path) {
    let store = std::sync::Arc::new(crate::app::results::diagram::store::LayoutStore::new(dir.to_path_buf()));
    app.update(visual, |this, _| this.diagram_layouts = Some(store));
}

/// Window position of a diagram-space point.
fn d1_window_point(
    visual: &mut gpui::VisualTestContext,
    view: &gpui::Entity<DiagramView>,
    point: crate::app::results::diagram::layout::DiagramPoint,
) -> gpui::Point<gpui::Pixels> {
    let area = visual.debug_bounds("diagram-area").expect("area");
    view.read_with(visual, |view, _| {
        area.origin
            + gpui::point(
                gpui::px((point.x - view.viewport.origin.x) * view.viewport.zoom),
                gpui::px((point.y - view.viewport.origin.y) * view.viewport.zoom),
            )
    })
}

fn d1_rects(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext) -> Vec<crate::app::results::diagram::layout::DiagramRect> {
    view.read_with(visual, |view, _| view.layout.rects.clone())
}

fn d1_reopen_orders(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    let row = app.update(visual, |this, _| {
        this.navigator.visible_rows().into_iter().find(|row| row.label == "orders").expect("orders row")
    });
    app.update(visual, |this, cx| this.handle_navigator_open(&row, cx));
    settle(visual);
}

/// The Chinook `table-diagram` scenario (focus `Track`, with columns and
/// both parents and children) once every entity is loaded.
fn d1_scenario(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, gpui::Entity<DiagramView>) {
    let (app, visual) = context_test_app(cx, "table-diagram", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    wait_until(visual, "the table diagram", |visual| visual.debug_bounds("diagram-entity-0").is_some());
    let view = diagram_view(&app, visual);
    wait_until(visual, "every entity to load", |visual| {
        view.read_with(visual, |view, _| {
            view.snapshot
                .as_ref()
                .is_some_and(|s| s.graph.entities.iter().all(|e| e.state == DiagramEntityState::Ready))
        })
    });
    settle(visual);
    (app, visual, view)
}

fn d1_typed_search(visual: &mut gpui::VisualTestContext, text: &str) {
    diagram_click(visual, "diagram-search");
    visual.simulate_input(text);
    settle(visual);
}

#[gpui::test]
fn dragged_positions_survive_reopening_and_relayout_forgets_them(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let dir = d1_temp_dir("positions");
    let (app, visual, tab_id) = sql_table_app(cx);
    d1_use_store(&app, visual, &dir);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    diagram_open(visual);
    let view = diagram_view(&app, visual);
    // What a view with nothing saved shows after loading, and the automatic
    // layout of the loaded graph (Re-layout; the first one was made while
    // the related tables were still placeholders).
    let first_open = d1_rects(&view, visual);
    diagram_click(visual, "diagram-relayout");
    let auto = d1_rects(&view, visual);
    assert_eq!(auto.len(), 4);

    // Drag `customers` by (40, 30) screen px (zoom 1 for this small graph).
    let customers = entity_bounds(visual, 1);
    drag(visual, customers.center(), gpui::point(gpui::px(40.0), gpui::px(30.0)));
    let dragged = d1_rects(&view, visual);
    assert_ne!(dragged[1], auto[1]);
    assert_eq!(dragged[0], auto[0], "only the dragged entity moved");

    // Nothing is written until the debounce is over; then one write lands.
    let files = |dir: &std::path::Path| std::fs::read_dir(dir).map(|entries| entries.count()).unwrap_or(0);
    assert_eq!(files(&dir), 0, "debounced");
    visual.cx.executor().advance_clock(crate::app::results::diagram::view::SAVE_DEBOUNCE + std::time::Duration::from_millis(10));
    wait_until(visual, "the saved positions", |_| files(&dir) == 1);

    // Close the tab and open the table again: same position (a fresh view
    // reads the file; this is also what an app restart does).
    app.update(visual, |this, cx| this.close_tab(tab_id, cx));
    settle(visual);
    d1_reopen_orders(&app, visual);
    diagram_open(visual);
    let reopened = diagram_view(&app, visual);
    wait_until(visual, "the saved positions to apply", |visual| {
        reopened.read_with(visual, |view, _| view.moved)
    });
    settle(visual);
    let after = d1_rects(&reopened, visual);
    assert_eq!(after[1].x, dragged[1].x);
    assert_eq!(after[1].y, dragged[1].y);
    assert_eq!(after[0], auto[0], "untouched entities keep their automatic place");

    // Re-layout resets the layout and deletes the saved file.
    diagram_click(visual, "diagram-relayout");
    assert_eq!(d1_rects(&reopened, visual), auto);
    wait_until(visual, "the saved file to go", |_| files(&dir) == 0);
    app.update(visual, |this, cx| {
        let tab = this.workspace.active_tab().unwrap().id;
        this.close_tab(tab, cx);
    });
    settle(visual);
    d1_reopen_orders(&app, visual);
    diagram_open(visual);
    let again = diagram_view(&app, visual);
    settle(visual);
    assert_eq!(d1_rects(&again, visual), first_open, "a reset layout stays reset");
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn a_saved_entry_of_a_table_that_is_gone_is_ignored(cx: &mut TestAppContext) {
    use crate::app::results::diagram::store::{DiagramKey, LayoutStore, SavedDiagram, SavedEntity};
    cx.dispatcher.allow_parking();
    let dir = d1_temp_dir("stale");
    let (app, visual, tab_id) = sql_table_app(cx);
    d1_use_store(&app, visual, &dir);
    let key = {
        let tab = app.read_with(visual, |this, _| this.workspace.tab(tab_id).unwrap().clone());
        DiagramKey {
            connection: tab.connection_id.clone(),
            database: tab.database.clone(),
            schema: tab.schema.clone().filter(|schema| !schema.is_empty()),
            table: Some(tab.collection.clone()),
        }
    };
    let saved = SavedDiagram {
        key: key.clone(),
        entities: vec![
            SavedEntity { schema: Some("public".into()), table: "orders".into(), x: 1000.0, y: 2000.0, ..Default::default() },
            SavedEntity { schema: Some("public".into()), table: "dropped_table".into(), x: -50.0, y: -60.0, ..Default::default() },
        ],
        ..Default::default()
    };
    LayoutStore::new(dir.clone()).save(&key, &saved).unwrap();
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    diagram_open(visual);
    let view = diagram_view(&app, visual);
    wait_until(visual, "the saved positions", |visual| view.read_with(visual, |view, _| view.moved));
    let rects = d1_rects(&view, visual);
    assert_eq!((rects[0].x, rects[0].y), (1000.0, 2000.0), "the saved entity moved");
    assert!(rects.iter().skip(1).all(|r| r.x > -50.0 || r.y > -60.0), "the entry of the missing table moved nothing");
    std::fs::remove_dir_all(dir).ok();
}

#[gpui::test]
fn search_highlights_matches_centres_the_view_and_enter_cycles(cx: &mut TestAppContext) {
    let (_app, visual, view) = d1_scenario(cx);
    let area = visual.debug_bounds("diagram-area").unwrap();
    let entity_of = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, table: &str| {
        view.read_with(visual, |view, _| {
            view.snapshot.as_ref().unwrap().graph.entities.iter().position(|e| e.table.table == table).unwrap()
        })
    };
    let centered = |visual: &mut gpui::VisualTestContext, index: usize| {
        let bounds = entity_bounds(visual, index);
        (bounds.center().x - area.center().x).abs() < gpui::px(1.5) && (bounds.center().y - area.center().y).abs() < gpui::px(1.5)
    };

    // A table name (any case): one hit, highlighted and centred.
    d1_typed_search(visual, "playlisttrack");
    let playlist_track = entity_of(&view, visual, "PlaylistTrack");
    let (matches, current) = view.read_with(visual, |view, _| (view.search.matches.to_vec(), view.search.current));
    assert_eq!((matches.iter().map(|id| id.0).collect::<Vec<_>>(), current), (vec![playlist_track], Some(0)));
    assert!(centered(visual, playlist_track), "the match is centred");
    assert!(visual.debug_bounds("diagram-search-count").is_some());

    // A column name matches the tables having it (here Track and MediaType);
    // Enter cycles through them, wrapping.
    visual.simulate_keystrokes("secondary-a");
    visual.simulate_input("mediatype");
    settle(visual);
    let track = entity_of(&view, visual, "Track");
    let media_type = entity_of(&view, visual, "MediaType");
    let mut hits = view.read_with(visual, |view, _| view.search.matches.iter().map(|id| id.0).collect::<Vec<_>>());
    hits.sort_unstable();
    let mut expected = vec![track, media_type];
    expected.sort_unstable();
    assert_eq!(hits, expected, "table MediaType + column MediaTypeId of Track");
    let order = view.read_with(visual, |view, _| view.search.matches.iter().map(|id| id.0).collect::<Vec<_>>());
    assert!(centered(visual, order[0]), "the first hit is centred");
    for step in 1..=order.len() {
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert_eq!(view.read_with(visual, |view, _| view.search.current), Some(step % order.len()));
        assert!(centered(visual, order[step % order.len()]), "Enter {step}");
    }

    // Escape inside the field clears the text and the hits.
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.search.matches.len()), 0);
    assert!(visual.debug_bounds("diagram-search-count").is_none());
}

#[gpui::test]
fn secondary_f_focuses_the_search_and_escape_clears_selection_and_search(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    diagram_open(visual);
    let view = diagram_view(&app, visual);
    let focused = |visual: &mut gpui::VisualTestContext| {
        let view = view.clone();
        visual.update(|window, cx| view.read(cx).search_focused(window, cx))
    };
    // Click an entity (focuses the diagram and selects it), then Cmd+F.
    let customers = entity_bounds(visual, 1);
    visual.simulate_click(customers.center(), Default::default());
    assert_eq!(view.read_with(visual, |view, _| view.selected.map(|id| id.0)), Some(1));
    assert!(!focused(visual));
    visual.simulate_keystrokes("secondary-f");
    settle(visual);
    assert!(focused(visual), "secondary-f focuses the diagram search");
    visual.simulate_input("order");
    settle(visual);
    assert!(view.read_with(visual, |view, _| !view.search.matches.is_empty()));
    // Back on the diagram: Escape drops the selection and the search.
    let customers = entity_bounds(visual, 1);
    visual.simulate_click(customers.center(), Default::default());
    assert_eq!(view.read_with(visual, |view, _| view.selected.map(|id| id.0)), Some(1));
    visual.simulate_keystrokes("escape");
    settle(visual);
    view.read_with(visual, |view, cx| {
        assert_eq!(view.selected, None);
        assert!(view.search.matches.is_empty() && view.search.query.is_empty());
        assert_eq!(view.search_text(cx), "");
    });
}

#[gpui::test]
fn minimap_click_and_drag_pan_the_viewport(cx: &mut TestAppContext) {
    let (app, visual, _) = sql_table_app(cx);
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    diagram_open(visual);
    let view = diagram_view(&app, visual);
    let area = visual.debug_bounds("diagram-area").unwrap();
    // The small diagram fits the window, so it has no minimap until part of
    // it is out of view.
    assert_eq!(view.read_with(visual, |view, _| view.minimap_rect()), None);
    for _ in 0..4 {
        diagram_click(visual, "diagram-zoom-in");
    }
    let rect = view.read_with(visual, |view, _| view.minimap_rect()).expect("the minimap is shown");
    assert_eq!((rect.width, rect.height), (180.0, 120.0));
    let origin = area.origin + gpui::point(gpui::px(rect.x), gpui::px(rect.y));
    // Bottom-right corner of the area.
    assert!((area.right() - (origin.x + gpui::px(rect.width)) - gpui::px(12.0)).abs() < gpui::px(0.5));
    assert!((area.bottom() - (origin.y + gpui::px(rect.height)) - gpui::px(12.0)).abs() < gpui::px(0.5));

    let layout_before = d1_rects(&view, visual);
    let size = area.size;
    let centre = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext| {
        view.read_with(visual, |view, _| view.viewport.visible_rect(f32::from(size.width), f32::from(size.height)).center())
    };
    let content = view.read_with(visual, |view, _| view.layout.bounds());
    // A click near the top-left corner centres the view on the content's
    // top-left (the target is clamped to the content).
    visual.simulate_click(origin + gpui::point(gpui::px(2.0), gpui::px(2.0)), Default::default());
    settle(visual);
    let c = centre(&view, visual);
    assert!((c.x - content.x).abs() < 0.5 && (c.y - content.y).abs() < 0.5, "{c:?} vs {content:?}");
    assert!(!view.read_with(visual, |view, _| view.auto_fit), "the user took over");
    assert_eq!(d1_rects(&view, visual), layout_before, "panning never moves entities");

    // Dragging inside the minimap follows the pointer.
    let start = origin + gpui::point(gpui::px(60.0), gpui::px(60.0));
    visual.simulate_mouse_down(start, gpui::MouseButton::Left, Default::default());
    let before = centre(&view, visual);
    visual.simulate_mouse_move(start + gpui::point(gpui::px(30.0), gpui::px(0.0)), gpui::MouseButton::Left, Default::default());
    let after = centre(&view, visual);
    visual.simulate_mouse_up(start + gpui::point(gpui::px(30.0), gpui::px(0.0)), gpui::MouseButton::Left, Default::default());
    settle(visual);
    assert!(after.x > before.x + 1.0, "dragging right moves the view right: {before:?} -> {after:?}");
    assert_eq!(view.read_with(visual, |view, _| view.selected), None);
    assert_eq!(d1_rects(&view, visual), layout_before);
}

#[gpui::test]
fn notation_toggle_and_the_four_attribute_modes_relayout_the_boxes(cx: &mut TestAppContext) {
    use crate::app::results::diagram::layout::DiagramLayout;
    use crate::app::results::diagram::notation::RelationNotation;
    let (_app, visual, view) = d1_scenario(cx);
    assert_eq!(view.read_with(visual, |view, _| view.notation), RelationNotation::CrowsFoot, "crow's foot is the default");
    diagram_click(visual, "diagram-notation");
    assert_eq!(view.read_with(visual, |view, _| view.notation), RelationNotation::Plain);
    diagram_click(visual, "diagram-notation");
    assert_eq!(view.read_with(visual, |view, _| view.notation), RelationNotation::CrowsFoot);

    let metrics = crate::app::results::diagram::view::METRICS;
    let mut heights = Vec::new();
    for mode in DiagramMode::ALL {
        choose_mode(visual, mode);
        view.read_with(visual, |view, _| {
            assert_eq!(view.mode, mode);
            let graph = &view.snapshot.as_ref().unwrap().graph;
            for (index, rect) in view.layout.rects.iter().enumerate() {
                let expected = DiagramLayout::entity_height(graph, crate::app::results::diagram::model::EntityId(index), mode, &metrics);
                assert_eq!(rect.height, expected, "{mode:?}: entity {index} is re-measured");
            }
            // Routes were recomputed for the visible rows.
            assert_eq!(view.routes.len(), graph.relations.len());
        });
        // The rendered box follows the model height.
        let zoom = zoom_of(&view, visual);
        let rendered = entity_bounds(visual, 0).size.height;
        let model = view.read_with(visual, |view, _| view.layout.rects[0].height);
        assert!((f32::from(rendered) - model * zoom).abs() < 1.0, "{mode:?}: rendered {rendered:?} vs {}", model * zoom);
        heights.push(model);
    }
    assert!(heights[0] > heights[1] && heights[1] > heights[2] && heights[2] > heights[3], "{heights:?}");
    assert_eq!(heights[3], metrics.header_height, "None leaves the header only");
    // The dropdown reopens with the current mode checked and offers all four.
    diagram_click(visual, "diagram-mode");
    visual.simulate_keystrokes("escape");
    settle(visual);
}

#[gpui::test]
fn copy_join_sql_from_a_relation_and_from_an_entity_writes_the_clipboard(cx: &mut TestAppContext) {
    let (_app, visual, view) = d1_scenario(cx);
    let relation_sql = |view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, index: usize| {
        view.read_with(visual, |view, _| {
            use crate::app::results::diagram::view::JoinTarget;
            view.join_sql(JoinTarget::Relation(index))
        })
    };

    // Right-click on the vertical stretch of the first routed relation.
    let (relation, on_line) = view.read_with(visual, |view, _| {
        let route = &view.routes[0];
        let (a, b) = (route.points[1], route.points[2]);
        (route.relation, crate::app::results::diagram::layout::DiagramPoint { x: a.x, y: (a.y + b.y) / 2.0 })
    });
    let at = d1_window_point(visual, &view, on_line);
    visual.simulate_mouse_move(at, None, Default::default());
    visual.simulate_mouse_down(at, gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(at, gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let copied = visual.cx.read_from_clipboard().and_then(|item| item.text()).expect("the menu copied SQL").to_string();
    assert_eq!(Some(copied.clone()), relation_sql(&view, visual, relation));
    assert_eq!(
        copied,
        "SELECT *\nFROM \"main\".\"InvoiceLine\" AS t1\nINNER JOIN \"main\".\"Track\" AS t2\n  ON t1.\"TrackId\" = t2.\"TrackId\"",
        "the relation, quoted for the connection's dialect"
    );

    // Right-click on an entity selects it and joins all of its relations.
    visual.cx.write_to_clipboard(gpui::ClipboardItem::new_string(String::new()));
    let focus = entity_bounds(visual, 0);
    visual.simulate_mouse_move(focus.center(), None, Default::default());
    visual.simulate_mouse_down(focus.center(), gpui::MouseButton::Right, Default::default());
    visual.simulate_mouse_up(focus.center(), gpui::MouseButton::Right, Default::default());
    visual.run_until_parked();
    assert_eq!(view.read_with(visual, |view, _| view.selected.map(|id| id.0)), Some(0));
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let all = visual.cx.read_from_clipboard().and_then(|item| item.text()).expect("copied").to_string();
    assert_eq!(
        all,
        "SELECT *\nFROM \"main\".\"Track\" AS t1\n\
         LEFT JOIN \"main\".\"InvoiceLine\" AS t2\n  ON t1.\"TrackId\" = t2.\"TrackId\"\n\
         LEFT JOIN \"main\".\"PlaylistTrack\" AS t3\n  ON t1.\"TrackId\" = t3.\"TrackId\"\n\
         LEFT JOIN \"main\".\"Album\" AS t4\n  ON t1.\"AlbumId\" = t4.\"AlbumId\"\n\
         LEFT JOIN \"main\".\"Genre\" AS t5\n  ON t1.\"GenreId\" = t5.\"GenreId\"\n\
         INNER JOIN \"main\".\"MediaType\" AS t6\n  ON t1.\"MediaTypeId\" = t6.\"MediaTypeId\"",
        "every relation of Track: children LEFT, nullable FKs LEFT, the mandatory FK INNER"
    );
}

#[gpui::test]
fn the_export_menu_writes_every_format_through_a_save_dialog(cx: &mut TestAppContext) {
    use crate::app::results::diagram::export::ExportFormat;
    cx.dispatcher.allow_parking();
    let (_app, visual, view) = d1_scenario(cx);
    let dir = d1_temp_dir("export");
    std::fs::create_dir_all(&dir).unwrap();
    let focus_table = "Track";
    let request_size = view.read_with(visual, |view, _| view.snapshot.as_ref().unwrap().graph.entities.len());
    assert_eq!(request_size, 6);

    for (index, format) in ExportFormat::ALL.into_iter().enumerate() {
        diagram_click(visual, "diagram-export");
        for _ in 0..=index {
            visual.simulate_keystrokes("down");
        }
        visual.simulate_keystrokes("enter");
        settle(visual);
        assert!(visual.did_prompt_for_new_path(), "{format:?} asks for a destination");
        let target = dir.join(format.file_name("chosen"));
        if index == 0 {
            // Cancelling the dialog writes nothing.
            visual.simulate_new_path_selection(|_| None);
            settle(visual);
            assert!(!target.exists());
            diagram_click(visual, "diagram-export");
            visual.simulate_keystrokes("down");
            visual.simulate_keystrokes("enter");
            settle(visual);
            assert!(visual.did_prompt_for_new_path());
        }
        let chosen = target.clone();
        visual.simulate_new_path_selection(move |_| Some(chosen.clone()));
        wait_until(visual, "the exported file", |_| target.exists());
        // The write finishes after the file appears.
        wait_until(visual, "the export notice", |visual| visual.debug_bounds("diagram-note").is_some());
        let bytes = std::fs::read(&target).unwrap();
        match format {
            ExportFormat::Svg => {
                let svg = String::from_utf8(bytes).unwrap();
                let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()).expect("a valid SVG");
                assert!(tree.size().width() > 400.0);
                assert_eq!(svg.matches("class=\"entity\"").count(), 6);
                assert!(svg.contains(&format!(">{focus_table}</text>")) && svg.contains(">InvoiceLine</text>"));
                assert_eq!(svg.matches("class=\"relation\"").count(), 5);
            }
            ExportFormat::Png => {
                let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).expect("a decodable PNG");
                assert!(image.width() > 800 && image.height() > 300, "{}x{}", image.width(), image.height());
            }
            ExportFormat::Mermaid => {
                let text = String::from_utf8(bytes).unwrap();
                assert!(text.starts_with("erDiagram\n") && text.contains("Track {") && text.contains("Album |o--o{ Track : \"AlbumId -> AlbumId\"") && text.contains("MediaType ||--o{ Track"), "{text}");
            }
            ExportFormat::Dbml => {
                let text = String::from_utf8(bytes).unwrap();
                assert!(text.contains("Table main.Track {") && text.contains("Ref: main.Track.AlbumId > main.Album.AlbumId"), "{text}");
            }
            ExportFormat::Json => {
                let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(value["kind"], "table");
                assert_eq!(value["tables"].as_array().unwrap().len(), 6);
                assert_eq!(value["relations"].as_array().unwrap().len(), 5);
                assert!(value["tables"][0]["position"]["x"].is_number());
            }
        }
    }

    // A destination that cannot be written is reported, nothing is left behind.
    diagram_click(visual, "diagram-export");
    visual.simulate_keystrokes("down");
    visual.simulate_keystrokes("enter");
    settle(visual);
    let missing = dir.join("no-such-dir").join("x.svg");
    visual.simulate_new_path_selection(move |_| Some(missing.clone()));
    wait_until(visual, "the failure notice", |visual| {
        view.read_with(visual, |view, _| view.notice.as_ref().is_some_and(|(text, error)| *error && text.contains("Could not export")))
    });
    assert!(!dir.join("no-such-dir").exists());
    std::fs::remove_dir_all(dir).ok();
}

// ---- D2 Part A: no overlapping boxes after load / mode change; minimap only when needed ----

/// Every pair of entity boxes is disjoint.
fn a_assert_no_overlap(view: &gpui::Entity<DiagramView>, visual: &mut gpui::VisualTestContext, what: &str) {
    let rects = d1_rects(view, visual);
    for (i, a) in rects.iter().enumerate() {
        for (j, b) in rects.iter().enumerate().skip(i + 1) {
            let apart = a.x + a.width <= b.x || b.x + b.width <= a.x || a.y + a.height <= b.y || b.y + b.height <= a.y;
            assert!(apart, "{what}: entity {i} {a:?} overlaps entity {j} {b:?}");
        }
    }
}

#[gpui::test]
fn the_table_diagram_never_overlaps_boxes_after_loading_or_changing_the_attribute_mode(cx: &mut TestAppContext) {
    let (_app, visual, view) = d1_scenario(cx);
    a_assert_no_overlap(&view, visual, "table diagram after load");
    for mode in DiagramMode::ALL {
        choose_mode(visual, mode);
        a_assert_no_overlap(&view, visual, &format!("table diagram, {mode:?}"));
    }
    diagram_click(visual, "diagram-refresh");
    wait_until(visual, "every entity to reload", |visual| {
        view.read_with(visual, |view, _| {
            view.snapshot.as_ref().is_some_and(|s| s.graph.entities.iter().all(|e| e.state == DiagramEntityState::Ready))
        })
    });
    settle(visual);
    a_assert_no_overlap(&view, visual, "table diagram after refresh");
}

#[gpui::test]
fn the_database_diagram_never_overlaps_boxes_after_loading_or_changing_the_attribute_mode(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "database-diagram", "populated");
    visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    settle(visual);
    diagram_wait_loaded(&app, visual);
    let view = diagram_view(&app, visual);
    a_assert_no_overlap(&view, visual, "database diagram after load");
    for mode in DiagramMode::ALL {
        choose_mode(visual, mode);
        a_assert_no_overlap(&view, visual, &format!("database diagram, {mode:?}"));
    }
}

#[gpui::test]
fn the_minimap_shows_only_while_part_of_the_diagram_is_out_of_view(cx: &mut TestAppContext) {
    let (_app, visual, view) = d1_scenario(cx);
    // Everything fits: nothing to overview, so nothing covers the tables.
    assert!(view.read_with(visual, |view, _| view.content_fits()), "the scenario fits the window");
    assert_eq!(view.read_with(visual, |view, _| view.minimap_rect()), None);
    assert!(visual.debug_bounds("diagram-minimap").is_none(), "no minimap is drawn");
    let area = visual.debug_bounds("diagram-area").unwrap();
    for index in 0..6 {
        let bounds = entity_bounds(visual, index);
        assert!(area.contains(&bounds.origin) && area.contains(&bounds.bottom_right()), "entity {index} is in view");
    }

    // Zoomed in until part of it is cut off: the overview appears, translucent.
    for _ in 0..3 {
        diagram_click(visual, "diagram-zoom-in");
    }
    assert!(!view.read_with(visual, |view, _| view.content_fits()));
    let rect = view.read_with(visual, |view, _| view.minimap_rect()).expect("the minimap is offered");
    assert_eq!((rect.width, rect.height), (180.0, 120.0));
    assert!(visual.debug_bounds("diagram-minimap").is_some(), "the minimap is drawn");
    assert!(view.read_with(visual, |view, _| view.minimap_opacity()) < 1.0, "translucent while idle");

    // Hovering it makes it opaque; leaving makes it translucent again.
    let minimap = visual.debug_bounds("diagram-minimap").unwrap();
    visual.simulate_mouse_move(minimap.center(), None, Default::default());
    settle(visual);
    assert_eq!(view.read_with(visual, |view, _| view.minimap_opacity()), 1.0);
    visual.simulate_mouse_move(area.origin + gpui::point(gpui::px(6.0), gpui::px(6.0)), None, Default::default());
    settle(visual);
    assert!(view.read_with(visual, |view, _| view.minimap_opacity()) < 1.0);

    // Fit brings everything into view again and the overview goes away.
    diagram_click(visual, "diagram-fit");
    assert!(view.read_with(visual, |view, _| view.content_fits()));
    assert!(visual.debug_bounds("diagram-minimap").is_none());
}
