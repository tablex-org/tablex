// The `showcase` scenario: a workspace as it looks in daily use. Its state is built, shows the
// shop's tree and three tabs, and nothing on screen calls itself a fixture.

fn showcase_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::init(cx);
    });
    let scenario = UiScenario::from_args(["tablex-gpui", "--ui-scenario", "showcase", "--ui-fixture", "sample"])
        .unwrap()
        .expect("the showcase scenario parses");
    // Whatever fixture was named, the showcase runs on its own data.
    assert_eq!(scenario.fixture, FixtureKind::Showcase);
    assert_eq!(scenario.window_size(), (1360, 860));
    let (root, visual) = cx.add_window_view(|window, cx| {
        let app = cx.new(|cx| AppRoot::new_for_scenario(scenario, cx));
        gpui_component::Root::new(app, window, cx)
    });
    let app = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1360.0), gpui::px(860.0)));
    settle(visual);
    (app, visual)
}

/// Every text the showcase can put on screen: connections, tree rows, tab titles, SQL, grid headers
/// and cells of every tab.
fn showcase_texts(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) -> Vec<String> {
    app.update(visual, |this, _| {
        let mut texts: Vec<String> = Vec::new();
        for saved in this.saved_connections.iter() {
            texts.push(saved.name.clone());
            texts.push(saved.uri.clone());
        }
        texts.extend(this.navigator.visible_rows().into_iter().map(|row| row.label));
        for tab in this.workspace.tabs().to_vec() {
            texts.push(tab.title.clone());
            texts.push(tab.sql.clone());
            let grid = this.grid(tab.id);
            for column in 0..grid.column_count() {
                texts.extend(grid.column_name(column).map(str::to_owned));
                for row in 0..grid.row_count() {
                    texts.extend(grid.full_cell_text(crate::grid::CellPosition { row, column }));
                }
            }
        }
        texts
    })
}

#[gpui::test]
fn the_showcase_builds_its_workspace(cx: &mut TestAppContext) {
    let (app, visual) = showcase_app(cx);
    app.read_with(visual, |this, _| {
        let names: Vec<&str> = this.saved_connections.iter().map(|saved| saved.name.as_str()).collect();
        assert_eq!(names, ["Production", "Analytics", "Orders", "Cache", "Local"]);
        let production = &this.saved_connections[0];
        assert_eq!(production.kind, ConnectionKind::Postgres);
        assert!(production.uri.contains("prod-db.example.com"), "{}", production.uri);
        assert!(production.ssh_tunnel);
        assert!(this.connection_type_of(crate::ui_fixture::POSTGRES_ID).is_some_and(|ty| ty.is_production() && ty.read_only));

        // The tree: the group, Production open down to the columns of `orders`.
        let labels: Vec<String> = this.navigator.visible_rows().into_iter().map(|row| row.label).collect();
        let at = |label: &str| labels.iter().position(|candidate| candidate == label).unwrap_or_else(|| panic!("{label} in {labels:?}"));
        assert!(at("Work") < at("Production") && at("Production") < at("shop") && at("shop") < at("public"));
        for table in ["coupons", "customers", "inventory", "invoices", "order_items", "orders", "payments", "products", "reviews", "shipments"] {
            assert!(labels.iter().any(|label| label == table), "{table}");
        }
        assert!(at("orders") < at("Columns") && at("Columns") < at("customer_id") && at("Triggers") < at("payments"), "{labels:?}");
        assert!(labels.iter().any(|label| label == "Local") && labels.iter().any(|label| label == "Cache"));

        // Three tabs, the SQL one in front with its 40 rows.
        let titles: Vec<&str> = this.workspace.tabs().iter().map(|tab| tab.title.as_str()).collect();
        assert_eq!(titles, ["Top customers", "orders", "customers"]);
        let active = this.workspace.active_tab().unwrap();
        assert_eq!(active.title, "Top customers");
        assert!(active.sql.contains("JOIN customers") || active.sql.contains("JOIN orders o"), "{}", active.sql);
        assert!(active.sql.contains("GROUP BY") && active.sql.contains("ORDER BY") && active.sql.contains("WHERE"));
        let grid = this.grid(active.id);
        assert_eq!((grid.row_count(), grid.column_count()), (40, 7));
        let tab = |title: &str| this.workspace.tabs().iter().find(|tab| tab.title == title).unwrap().id;
        assert_eq!(this.grid(tab("orders")).row_count(), 40, "the Mongo orders");
        assert_eq!(this.grid(tab("customers")).row_count(), 50, "the first page of customers");
        assert!(this.workspace.tabs().iter().find(|tab| tab.title == "orders").unwrap().query_mode.is_mongo_find());
    });
}

#[gpui::test]
fn nothing_the_showcase_shows_calls_itself_a_fixture(cx: &mut TestAppContext) {
    let (app, visual) = showcase_app(cx);
    let texts = showcase_texts(&app, visual);
    assert!(texts.len() > 400, "the tree, tabs and three grids were read: {}", texts.len());
    for text in &texts {
        assert!(!text.to_lowercase().contains("fixture"), "{text:?}");
    }
    // Open the other folders of `orders` and the other connections: still nothing.
    app.update(visual, |this, _| {
        for connection in [crate::ui_fixture::CLICKHOUSE_ID, crate::ui_fixture::SQLITE_ID] {
            this.apply_scenario_expansion(&NavigatorNodeId::Connection { connection: connection.into() });
        }
    });
    for text in showcase_texts(&app, visual) {
        assert!(!text.to_lowercase().contains("fixture"), "{text:?}");
    }
}

#[gpui::test]
fn the_showcase_tables_open_their_folders_with_content(cx: &mut TestAppContext) {
    use crate::navigator::TableFolder;
    let (app, visual) = showcase_app(cx);
    let key = crate::ui_fixture::POSTGRES_ID;
    let orders = NavigatorNodeId::Object {
        connection: key.into(),
        database: "shop".into(),
        schema: "public".into(),
        object: "orders".into(),
    };
    let shown = app.update(visual, |this, _| {
        [TableFolder::Indexes, TableFolder::Constraints, TableFolder::ForeignKeys, TableFolder::Triggers].map(|folder| {
            let id = orders.table_folder(folder).unwrap();
            this.apply_scenario_expansion(&id);
            this.navigator.loaded_children(&id).map_or(0, <[_]>::len)
        })
    });
    // Indexes (the primary key's and three more), constraints (primary key and two checks), foreign keys, triggers.
    assert_eq!(shown, [4, 3, 2, 2]);
}
