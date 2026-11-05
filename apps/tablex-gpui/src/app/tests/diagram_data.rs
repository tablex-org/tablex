// Data layer of the table ER diagram: the focus table and its foreign-key
// neighbours fetched in the background (bounded concurrency), cached per tab,
// memoized into one `Arc<DiagramGraph>` per generation. The injected tests
// replace the engine / driver I/O with the Chinook-like scenario catalog; the
// native-engine and driver tests run against a real SQLite file.

use super::results::diagram::data::{
    DiagramSnapshot, InjectedFetch, SchemaDiagramTarget, MAX_CONCURRENT, MAX_SCHEMA_TABLES,
};
use super::results::diagram::model::{EntityState, TableRef};

const DIAGRAM_FETCH_DELAY: std::time::Duration = std::time::Duration::from_millis(40);

/// Counts the injected fetches and the most that ran at once.
#[derive(Default)]
struct FetchStats {
    calls: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
    /// Tables outside the Chinook catalog describe as relation-free.
    lenient: std::sync::atomic::AtomicBool,
    /// Per-fetch delay in milliseconds (0: `DIAGRAM_FETCH_DELAY`).
    delay_ms: std::sync::atomic::AtomicU64,
    failing: std::sync::Mutex<Vec<String>>,
}

impl FetchStats {
    fn calls(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.max_active.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn fail(&self, table: &str) {
        self.failing.lock().unwrap().push(table.to_string());
    }

    fn fetcher(self: &std::sync::Arc<Self>) -> std::sync::Arc<InjectedFetch> {
        use std::sync::atomic::Ordering::SeqCst;
        let stats = self.clone();
        std::sync::Arc::new(move |table: &TableRef| {
            stats.calls.fetch_add(1, SeqCst);
            let now = stats.active.fetch_add(1, SeqCst) + 1;
            stats.max_active.fetch_max(now, SeqCst);
            std::thread::sleep(match stats.delay_ms.load(SeqCst) {
                0 => DIAGRAM_FETCH_DELAY,
                ms => std::time::Duration::from_millis(ms),
            });
            stats.active.fetch_sub(1, SeqCst);
            if stats.failing.lock().unwrap().contains(&table.table) {
                return Err(format!("boom {}", table.table));
            }
            crate::ui_fixture_diagram::table_properties(&table.table).or_else(|error| {
                if stats.lenient.load(SeqCst) {
                    Ok(Default::default())
                } else {
                    Err(error)
                }
            })
        })
    }
}

struct DiagramRig<'a> {
    app: gpui::Entity<AppRoot>,
    visual: &'a mut gpui::VisualTestContext,
    tab: u64,
    stats: std::sync::Arc<FetchStats>,
}

impl<'a> DiagramRig<'a> {
    /// A fixture app with a `Track` table tab whose diagram I/O is injected.
    fn new(cx: &'a mut TestAppContext) -> Self {
        cx.dispatcher.allow_parking();
        let (app, visual) = sidebar_test_app(cx);
        let stats = std::sync::Arc::new(FetchStats::default());
        let fetcher = stats.fetcher();
        let tab = app.update(visual, |this, _| {
            this.diagram.injected = Some(fetcher);
            this.workspace.open_query_tab_with_metadata(
                "Track",
                TabMetadata {
                    connection_id: crate::ui_fixture::SQLITE_ID.into(),
                    database: "main".into(),
                    collection: "Track".into(),
                    query_mode: Some(QueryMode::SqlTable),
                    ..Default::default()
                },
            )
        });
        Self { app, visual, tab, stats }
    }

    fn request(&mut self) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| this.request_diagram(tab, cx));
    }

    fn refresh(&mut self) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| this.refresh_diagram(tab, cx));
    }

    fn snapshot(&mut self) -> Option<DiagramSnapshot> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| this.diagram_graph(tab))
    }

    /// Waits until the diagram shows no entity that is still loading.
    fn wait_loaded(&mut self) -> DiagramSnapshot {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, "the diagram to load", |visual| {
            app.read_with(visual, |this, _| {
                this.diagram_graph(tab).is_some_and(|snapshot| {
                    snapshot
                        .graph
                        .entities
                        .iter()
                        .all(|entity| entity.state != EntityState::Loading)
                })
            })
        });
        self.snapshot().expect("loaded")
    }

    fn diagram_log(&mut self) -> Vec<String> {
        self.app.read_with(self.visual, |this, _| {
            (0..this.error_log.log.len())
                .filter_map(|index| this.error_log.log.get(index))
                .filter(|entry| entry.area == crate::error_log::Area::Diagram)
                .map(|entry| entry.message.to_string())
                .collect()
        })
    }
}

fn diagram_tables(snapshot: &DiagramSnapshot) -> Vec<(&str, &EntityState)> {
    snapshot
        .graph
        .entities
        .iter()
        .map(|entity| (entity.table.table.as_str(), &entity.state))
        .collect()
}

#[gpui::test]
fn diagram_fetches_focus_parents_and_children_with_bounded_concurrency(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    assert!(rig.snapshot().is_none(), "nothing requested yet");
    rig.request();
    assert!(rig.snapshot().is_none(), "the focus is still loading");

    let snapshot = rig.wait_loaded();
    let mut tables = diagram_tables(&snapshot);
    assert_eq!(tables[0].0, "Track", "the focus comes first");
    tables.sort_by_key(|(table, _)| *table);
    assert!(tables.iter().all(|(_, state)| **state == EntityState::Ready), "{tables:?}");
    assert_eq!(
        tables.iter().map(|(table, _)| *table).collect::<Vec<_>>(),
        ["Album", "Genre", "InvoiceLine", "MediaType", "PlaylistTrack", "Track"]
    );
    assert_eq!(snapshot.graph.relations.len(), 5);
    assert!(snapshot.error.is_none());
    assert_eq!(rig.stats.calls(), 6);
    assert_eq!(
        rig.stats.max_active(),
        MAX_CONCURRENT,
        "five related tables are fetched {MAX_CONCURRENT} at a time"
    );
}

#[gpui::test]
fn diagram_generation_bumps_only_on_completed_fetches_and_the_graph_is_memoized(
    cx: &mut TestAppContext,
) {
    let mut rig = DiagramRig::new(cx);
    rig.request();
    let done = rig.wait_loaded();

    // Nothing completed in between: the very same graph and generation.
    let again = rig.snapshot().unwrap();
    assert_eq!(again.generation, done.generation);
    assert!(std::sync::Arc::ptr_eq(&again.graph, &done.graph), "memoized");
    rig.request();
    let after_request = rig.snapshot().unwrap();
    assert!(std::sync::Arc::ptr_eq(&after_request.graph, &done.graph));
    assert_eq!(after_request.generation, done.generation);

    // One generation per completed fetch (focus plus five related tables).
    rig.refresh();
    let done_again = rig.wait_loaded();
    assert_eq!(done_again.generation, done.generation + 1 + 6);
    assert!(!std::sync::Arc::ptr_eq(&done_again.graph, &done.graph));
}

#[gpui::test]
fn diagram_cache_avoids_refetching_and_refresh_refetches(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.request();
    rig.wait_loaded();
    assert_eq!(rig.stats.calls(), 6);

    // Requesting a loaded diagram again is a cache hit.
    rig.request();
    rig.visual.run_until_parked();
    assert_eq!(rig.stats.calls(), 6);

    // The tab shows another table of the same session: Album and Track are
    // cached, only Album's parent Artist is new.
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| {
        this.workspace.tab_mut(tab).unwrap().collection = "Album".into();
        this.request_diagram(tab, cx);
    });
    let album = rig.wait_loaded();
    assert_eq!(rig.stats.calls(), 7, "only Artist was fetched");
    let mut tables: Vec<_> = diagram_tables(&album).iter().map(|(table, _)| *table).collect();
    tables.sort();
    assert_eq!(tables, ["Album", "Artist", "Track"]);

    // Refresh drops the cache.
    rig.refresh();
    rig.wait_loaded();
    assert_eq!(rig.stats.calls(), 10);
}

#[gpui::test]
fn diagram_focus_failure_is_reported_and_loads_nothing_else(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.stats.fail("Track");
    rig.request();
    let (app, tab) = (rig.app.clone(), rig.tab);
    wait_until(rig.visual, "the focus failure", |visual| {
        app.read_with(visual, |this, _| this.diagram_graph(tab).is_some())
    });
    let snapshot = rig.snapshot().unwrap();
    assert_eq!(snapshot.error.as_deref(), Some("boom Track"));
    assert!(snapshot.graph.entities.is_empty());
    assert_eq!(rig.stats.calls(), 1, "no related table is fetched");
    let log = rig.diagram_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("boom Track"), "{log:?}");
}

#[gpui::test]
fn diagram_related_failure_marks_only_that_entity_failed(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.stats.fail("Genre");
    rig.request();
    let snapshot = rig.wait_loaded();
    assert!(snapshot.error.is_none());
    for (table, state) in diagram_tables(&snapshot) {
        match table {
            "Genre" => assert_eq!(state, &EntityState::Failed("boom Genre".into())),
            _ => assert_eq!(state, &EntityState::Ready, "{table}"),
        }
    }
    let log = rig.diagram_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("Genre"), "{log:?}");
}

#[gpui::test]
fn closing_the_tab_drops_the_diagram_cache(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.request();
    rig.wait_loaded();
    let tab = rig.tab;
    assert!(rig.app.read_with(rig.visual, |this, _| this.diagram.tracks(tab)));
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab, cx));
    assert!(!rig.app.read_with(rig.visual, |this, _| this.diagram.tracks(tab)));
    assert!(rig.app.read_with(rig.visual, |this, _| this.diagram_graph(tab).is_none()));
}

#[gpui::test]
fn closing_the_tab_while_fetching_ignores_late_completions(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.request();
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab, cx));
    let stats = rig.stats.clone();
    wait_until(rig.visual, "the in-flight fetch", |_| stats.calls() == 1);
    std::thread::sleep(DIAGRAM_FETCH_DELAY * 2);
    rig.visual.run_until_parked();
    assert!(!rig.app.read_with(rig.visual, |this, _| this.diagram.tracks(tab)));
    assert_eq!(rig.stats.calls(), 1, "no related table was queued");
}

const DIAGRAM_SEED: &str = "PRAGMA foreign_keys = ON; \
     CREATE TABLE artist(id INTEGER PRIMARY KEY, name TEXT); \
     CREATE TABLE album(id INTEGER PRIMARY KEY, title TEXT, artist_id INTEGER REFERENCES artist(id)); \
     CREATE TABLE track(id INTEGER PRIMARY KEY, album_id INTEGER REFERENCES album(id));";

/// Points the rig's SQL tab at `album` of the scratch SQLite file.
fn open_album_diagram_tab(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    tab: u64,
) {
    app.update(visual, |this, _| {
        let tab = this.workspace.tab_mut(tab).unwrap();
        tab.database = "main".into();
        tab.collection = "album".into();
        tab.query_mode = Some(QueryMode::SqlTable);
    });
}

fn assert_album_diagram(snapshot: &DiagramSnapshot) {
    let mut tables = diagram_tables(snapshot);
    assert_eq!(tables[0].0, "album");
    tables.sort_by_key(|(table, _)| *table);
    assert_eq!(
        tables.iter().map(|(table, _)| *table).collect::<Vec<_>>(),
        ["album", "artist", "track"]
    );
    assert!(tables.iter().all(|(_, state)| **state == EntityState::Ready), "{tables:?}");
    assert_eq!(snapshot.graph.relations.len(), 2);
    assert!(snapshot.error.is_none());
}

#[gpui::test]
fn diagram_loads_foreign_keys_from_the_sqlite_driver(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DIAGRAM_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, tab) = (rig.app.clone(), rig.tab);
    let visual = rig.visual;
    open_album_diagram_tab(&app, visual, tab);
    app.update(visual, |this, cx| this.request_diagram(tab, cx));
    wait_until(visual, "the driver diagram", |visual| {
        app.read_with(visual, |this, _| {
            this.diagram_graph(tab).is_some_and(|snapshot| {
                snapshot.graph.entities.len() == 3
                    && snapshot.graph.entities.iter().all(|entity| entity.state == EntityState::Ready)
            })
        })
    });
    let snapshot = app.read_with(visual, |this, _| this.diagram_graph(tab)).unwrap();
    assert_album_diagram(&snapshot);
}

#[gpui::test]
fn a_dropped_driver_session_flips_the_connection_to_its_reconnect_state(cx: &mut TestAppContext) {
    let db = SqliteDb::new(DIAGRAM_SEED);
    let rig = SqlRig::sqlite(cx, &db);
    let (app, tab, key) = (rig.app.clone(), rig.tab, rig.key.clone());
    let visual = rig.visual;
    rig.session.lock().unwrap().disconnect().expect("disconnect");
    open_album_diagram_tab(&app, visual, tab);
    app.update(visual, |this, cx| this.request_diagram(tab, cx));
    wait_until(visual, "the lost connection", |visual| {
        app.read_with(visual, |this, _| {
            matches!(this.sessions.status(&key), ConnectionStatus::Failed(_))
        })
    });
    app.read_with(visual, |this, _| {
        assert_eq!(
            this.sessions.status(&key),
            ConnectionStatus::Failed(crate::sessions::CONNECTION_LOST_REASON.into())
        );
        let snapshot = this.diagram_graph(tab).expect("the focus failed");
        assert!(snapshot.error.is_some());
        assert!(snapshot.graph.entities.is_empty());
        let logged = (0..this.error_log.log.len())
            .filter_map(|index| this.error_log.log.get(index))
            .any(|entry| {
                entry.area == crate::error_log::Area::Diagram
                    && entry.code.as_deref() == Some("session.not_connected")
            });
        assert!(logged, "the failure is in the Error Log under Diagram");
    });
}

// --- whole-schema diagrams -------------------------------------------------

const SCHEMA_CONNECTION: &str = "schema-conn";

fn schema_target() -> SchemaDiagramTarget {
    SchemaDiagramTarget {
        connection: SCHEMA_CONNECTION.into(),
        database: "main".into(),
        schema: Some("main".into()),
    }
}

fn schema_tables_group() -> NavigatorNodeId {
    crate::navigator::object_group_id(SCHEMA_CONNECTION, "main", "main", crate::navigator::ObjectGroup::Tables)
}

fn schema_table_children(names: &[String]) -> Vec<crate::navigator::LoadedChild> {
    names
        .iter()
        .map(|name| {
            crate::navigator::object_child(
                SCHEMA_CONNECTION,
                "main",
                "main",
                name,
                crate::navigator::KindOfObject::Table,
            )
        })
        .collect()
}

impl DiagramRig<'_> {
    /// The navigator already knows the schema's tables.
    fn cache_schema_tables(&mut self, names: &[String]) {
        let children = schema_table_children(names);
        self.app.update(self.visual, |this, _| {
            let group = schema_tables_group();
            let request = this.navigator.begin_background_load(&group).expect("group loads");
            let generation = this.navigator.load_generation(&request).unwrap();
            this.navigator.apply_load(&request, generation, Ok(children));
        });
    }

    fn request_schema(&mut self) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            this.load_schema_diagram_for(tab, schema_target(), false, cx)
        });
    }

    fn refresh_schema(&mut self) {
        let tab = self.tab;
        self.app.update(self.visual, |this, cx| {
            this.load_schema_diagram_for(tab, schema_target(), true, cx)
        });
    }

    fn schema_snapshot(&mut self) -> Option<DiagramSnapshot> {
        let tab = self.tab;
        self.app.read_with(self.visual, |this, _| this.schema_diagram_graph(tab))
    }

    /// Waits until the schema diagram is complete (no progress, nothing loading).
    fn wait_schema_loaded(&mut self) -> DiagramSnapshot {
        let (app, tab) = (self.app.clone(), self.tab);
        wait_until(self.visual, "the schema diagram to load", |visual| {
            app.read_with(visual, |this, _| {
                this.schema_diagram_graph(tab).is_some_and(|snapshot| {
                    snapshot.progress.is_none()
                        && snapshot.graph.entities.iter().all(|entity| entity.state != EntityState::Loading)
                })
            })
        });
        self.schema_snapshot().expect("loaded")
    }
}

#[gpui::test]
fn schema_diagram_uses_the_navigator_cache_and_describes_every_table(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.cache_schema_tables(&crate::ui_fixture_diagram::table_names());
    rig.request_schema();
    let snapshot = rig.wait_schema_loaded();

    assert_eq!(snapshot.graph.entities.len(), 11);
    assert!(snapshot.graph.entities.iter().all(|entity| !entity.focus));
    assert_eq!(snapshot.graph.relations.len(), 11, "every FK, the Employee self-reference included");
    assert!(snapshot.error.is_none() && snapshot.truncated.is_none() && snapshot.progress.is_none());
    assert_eq!(rig.stats.calls(), 11);
    assert_eq!(rig.stats.max_active(), MAX_CONCURRENT);
    // The navigator's cached list was used: no load was started for it.
    let loading = rig.app.read_with(rig.visual, |this, _| {
        this.navigator.load_state(&schema_tables_group()).cloned()
    });
    assert_eq!(loading, Some(crate::navigator::NodeLoadState::Loaded));

    // Idempotent request, memoized graph.
    rig.request_schema();
    let again = rig.schema_snapshot().unwrap();
    assert!(std::sync::Arc::ptr_eq(&again.graph, &snapshot.graph));
    assert_eq!(rig.stats.calls(), 11);
}

#[gpui::test]
fn schema_diagram_loads_the_table_list_through_the_navigator(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.request_schema();
    assert!(rig.schema_snapshot().is_none(), "waiting for the table list");
    let state = rig.app.read_with(rig.visual, |this, _| {
        this.navigator.load_state(&schema_tables_group()).cloned()
    });
    assert_eq!(state, Some(crate::navigator::NodeLoadState::Loading), "the navigator load path was used");
    assert_eq!(rig.stats.calls(), 0);

    // The navigator's load lands (what the engine / driver load completion does).
    let children = schema_table_children(&["Album".to_string(), "Artist".to_string()]);
    rig.app.update(rig.visual, |this, cx| {
        let request = crate::navigator::LoadRequest::Objects {
            connection: SCHEMA_CONNECTION.into(),
            database: "main".into(),
            schema: "main".into(),
            group: crate::navigator::ObjectGroup::Tables,
        };
        let generation = this.navigator.load_generation(&request).unwrap();
        this.apply_navigator_load(&request, generation, Ok(children), cx);
    });
    let snapshot = rig.wait_schema_loaded();
    let tables: Vec<_> = diagram_tables(&snapshot).iter().map(|(table, _)| *table).collect();
    assert_eq!(tables, ["Album", "Artist"]);
    assert_eq!(snapshot.graph.relations.len(), 1);
}

#[gpui::test]
fn schema_diagram_reports_a_failed_table_list(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.request_schema();
    rig.app.update(rig.visual, |this, cx| {
        let request = crate::navigator::LoadRequest::Objects {
            connection: SCHEMA_CONNECTION.into(),
            database: "main".into(),
            schema: "main".into(),
            group: crate::navigator::ObjectGroup::Tables,
        };
        let generation = this.navigator.load_generation(&request).unwrap();
        this.apply_navigator_load(&request, generation, Err("no catalog".into()), cx);
    });
    let snapshot = rig.schema_snapshot().expect("the list failed");
    assert_eq!(snapshot.error.as_deref(), Some("no catalog"));
    assert!(snapshot.graph.entities.is_empty());
}

#[gpui::test]
fn schema_diagram_is_capped_at_the_table_limit(cx: &mut TestAppContext) {
    use std::sync::atomic::Ordering::SeqCst;
    let mut rig = DiagramRig::new(cx);
    rig.stats.lenient.store(true, SeqCst);
    rig.stats.delay_ms.store(1, SeqCst);
    let names: Vec<String> = (0..MAX_SCHEMA_TABLES + 50).map(|index| format!("t{index:03}")).collect();
    rig.cache_schema_tables(&names);
    rig.request_schema();
    let snapshot = rig.wait_schema_loaded();
    assert_eq!(snapshot.graph.entities.len(), MAX_SCHEMA_TABLES);
    assert_eq!(snapshot.truncated, Some(MAX_SCHEMA_TABLES + 50));
    assert_eq!(rig.stats.calls(), MAX_SCHEMA_TABLES, "the extra tables are never described");
    let tables = diagram_tables(&snapshot);
    assert!(tables.iter().any(|(table, _)| *table == "t299") && tables.iter().all(|(table, _)| *table != "t300"));
}

#[gpui::test]
fn schema_diagram_reports_progress_and_publishes_in_batches(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.cache_schema_tables(&crate::ui_fixture_diagram::table_names());
    rig.request_schema();
    let mut seen = Vec::new();
    for _ in 0..400 {
        // The publish timer runs on the test clock.
        rig.visual.cx.executor().advance_clock(std::time::Duration::from_millis(120));
        rig.visual.run_until_parked();
        let snapshot = rig.schema_snapshot();
        if let Some(snapshot) = &snapshot {
            seen.push((snapshot.generation, snapshot.progress));
            if snapshot.progress.is_none() {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let (_, last) = *seen.last().expect("a snapshot was published");
    assert_eq!(last, None, "complete: no progress");
    assert!(
        seen.iter().any(|(_, progress)| matches!(progress, Some((loaded, 11)) if *loaded < 11)),
        "partial progress was published: {seen:?}"
    );
    // Eleven completions, far fewer publishes than rebuilds-per-fetch.
    let generations: std::collections::BTreeSet<_> = seen.iter().map(|(generation, _)| *generation).collect();
    assert!(generations.len() < 11, "{generations:?}");
}

#[gpui::test]
fn schema_diagram_marks_a_failed_table_and_refresh_describes_everything_again(
    cx: &mut TestAppContext,
) {
    let mut rig = DiagramRig::new(cx);
    rig.stats.fail("Genre");
    rig.cache_schema_tables(&crate::ui_fixture_diagram::table_names());
    rig.request_schema();
    let first = rig.wait_schema_loaded();
    for (table, state) in diagram_tables(&first) {
        match table {
            "Genre" => assert_eq!(state, &EntityState::Failed("boom Genre".into())),
            _ => assert_eq!(state, &EntityState::Ready, "{table}"),
        }
    }
    let log = rig.diagram_log();
    assert_eq!(log.len(), 1, "{log:?}");

    rig.refresh_schema();
    let second = rig.wait_schema_loaded();
    assert_eq!(rig.stats.calls(), 22);
    assert!(second.generation > first.generation);
    assert!(!std::sync::Arc::ptr_eq(&second.graph, &first.graph));
}

#[gpui::test]
fn closing_the_tab_drops_the_schema_diagram(cx: &mut TestAppContext) {
    let mut rig = DiagramRig::new(cx);
    rig.cache_schema_tables(&crate::ui_fixture_diagram::table_names());
    rig.request_schema();
    rig.wait_schema_loaded();
    let tab = rig.tab;
    rig.app.update(rig.visual, |this, cx| this.close_tab(tab, cx));
    assert!(rig.app.read_with(rig.visual, |this, _| this.schema_diagram_graph(tab).is_none()));
}
