// A searchable Select must filter what the search box is typed into. gpui-component's plain `Vec<T>`
// delegate shows the box but never filters (only `SearchableVec<T>` does), so each Select below is driven
// the way a user does: open, type, pick from the filtered list, clear the query, reopen.

/// One searchable Select under test.
struct SearchableSelect<'a> {
    /// Debug selector of the trigger.
    trigger: &'static str,
    /// Width of the menu (what the `Select` was given, else the trigger's).
    menu_width: f32,
    /// Height of one row (the Select's size decides it).
    pitch: f32,
    /// How many rows its menu shows with no query: every item, or fewer when the menu scrolls.
    shown: usize,
    /// Typed into the search box (matching is case-insensitive substring).
    query: String,
    /// How many items `query` leaves.
    matching: usize,
    /// Row of the filtered list to click.
    pick: usize,
    /// Row `pick` has in the full list: the cursor on reopening.
    pick_in_full: usize,
    /// The model value after `pick`, as the caller reads it.
    value: &'a dyn Fn(&gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) -> String,
    expected: String,
}

fn menu_rows(visual: &mut gpui::VisualTestContext, probe: &SearchableSelect) -> OpenMenu {
    open_menu_pitch(visual, probe.trigger, probe.menu_width, true, probe.pitch)
        .unwrap_or_else(|| panic!("{} is open", probe.trigger))
}

/// Opens the Select, types, picks from the filtered list, clears the query, picks again and reopens.
fn drive_searchable_select(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    probe: &SearchableSelect,
) {
    let trigger = probe.trigger;
    // 1. Open: every item.
    click_debug_button(visual, trigger);
    assert_eq!(menu_rows(visual, probe).rows, probe.shown, "{trigger}: the menu is full before a query");
    // 2. Type: only the matches stay.
    visual.simulate_input(&probe.query);
    settle(visual);
    assert_eq!(menu_rows(visual, probe).rows, probe.matching, "{trigger}: {:?} leaves only its matches", probe.query);
    // 3. Clear the query: every item is back.
    visual.simulate_keystrokes("secondary-a backspace");
    settle(visual);
    assert_eq!(menu_rows(visual, probe).rows, probe.shown, "{trigger}: clearing the query brings the items back");
    // 4. Type again and pick from the filtered list: the model gets the clicked item, not the item at that
    //    row of the full list.
    visual.simulate_input(&probe.query);
    settle(visual);
    assert_eq!(menu_rows(visual, probe).rows, probe.matching, "{trigger}: filtered again");
    click_select_option(visual, trigger, 48.7, probe.pick);
    assert_eq!((probe.value)(app, visual), probe.expected, "{trigger}: the model follows the filtered pick");
    assert_eq!(open_menu_pitch(visual, trigger, probe.menu_width, true, probe.pitch), None, "{trigger}: picking closes the menu");
    // 5. Reopen: the query is gone and the cursor is on the picked item's row of the full list.
    click_debug_button(visual, trigger);
    assert_eq!(
        menu_rows(visual, probe),
        OpenMenu { rows: probe.shown, cursor: Some(probe.pick_in_full) },
        "{trigger}: reopening lists everything with the picked item selected"
    );
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert_eq!((probe.value)(app, visual), probe.expected, "{trigger}: Escape keeps the pick");
}

#[gpui::test]
fn the_toolbar_database_select_filters_what_is_typed_and_picks_the_matching_database(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    // Databases fixture_db_0 .. fixture_db_7; fixture_db_0 is the tab's.
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "script-database",
            menu_width: 240.0,
            pitch: SELECT_ITEM_PITCH,
            // The menu is four rows tall and scrolls.
            shown: 4,
            query: "DB_3".into(),
            matching: 1,
            pick: 0,
            pick_in_full: 3,
            value: &|app, visual| app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().database.clone()),
            expected: "fixture_db_3".into(),
        },
    );
}

#[gpui::test]
fn the_toolbar_connection_select_filters_what_is_typed_and_picks_the_matching_connection(cx: &mut TestAppContext) {
    let (app, visual) = script_app(cx);
    add_second_connection(&app, visual);
    // Items: [Mongo fixture, Mongo second]; the tab is on the first.
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "script-connection",
            menu_width: 240.0,
            pitch: SELECT_ITEM_PITCH,
            shown: 2,
            query: "SECON".into(),
            matching: 1,
            pick: 0,
            pick_in_full: 1,
            value: &|app, visual| app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().connection_id.clone()),
            expected: SECOND_MONGO.into(),
        },
    );
}

#[gpui::test]
fn the_sql_filter_column_select_filters_what_is_typed_and_picks_the_matching_column(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab) = sql_table_app(cx);
    open_filter_popover(visual);
    let columns: Vec<String> = app.read_with(visual, |this, _| {
        let grid = this.grid(tab);
        (0..grid.column_count()).filter_map(|column| grid.column_name(column).map(str::to_string)).collect()
    });
    // The query is the tail of the last column's name: it is no prefix of anything.
    let last = columns.last().unwrap().clone();
    let query = last[1..].to_string();
    let matching = columns.iter().filter(|name| name.to_lowercase().contains(&query)).count();
    assert_eq!(matching, 1, "{columns:?}: the test needs a query that leaves one column");
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "sql-filter-column-1",
            menu_width: 240.0,
            pitch: SELECT_ITEM_PITCH,
            shown: columns.len(),
            query,
            matching,
            pick: 0,
            pick_in_full: columns.len() - 1,
            value: &|app, visual| app.read_with(visual, |this, _| this.workspace.active_tab().unwrap().table_filter.rules[0].column.clone()),
            expected: last,
        },
    );
}

#[gpui::test]
fn the_ssh_config_select_filters_what_is_typed_and_fills_from_the_matching_host(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = dialog_app(cx);
    let inputs = open_fixture_dialog(&app, visual, crate::ui_fixture::POSTGRES_ID);
    let config = engine_net::SshConfig::parse(
        "Host bastion\n  HostName bastion.example.com\n  User deploy\n\
         Host jumpy\n  HostName jumpy.example.com\n  User ops\n\
         Host staging-db\n  HostName staging.example.com\n  User dba\n",
    )
    .unwrap();
    visual.update(|window, cx| inputs.ssh.set_config_result(Ok(config), window, cx));
    click_debug_button(visual, "connection-config-tab-tunnel");
    click_debug_button(visual, "ssh-enable-switch");
    settle(visual);
    let width = visual.debug_bounds("connection-config-ssh-config-select").expect("the picker").size.width.as_f32();
    let ssh = inputs.ssh.clone();
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "connection-config-ssh-config-select",
            menu_width: width,
            // The connection dialog's Selects are the medium size.
            pitch: 31.4,
            shown: 3,
            query: "STAG".into(),
            matching: 1,
            pick: 0,
            pick_in_full: 2,
            value: &|_, visual| visual.update(|_, cx| ssh.read(cx).host.clone()),
            expected: "staging.example.com".into(),
        },
    );
    let filled = visual.update(|_, cx| inputs.ssh.read(cx));
    assert_eq!((filled.config_host.as_str(), filled.user.as_str()), ("staging-db", "dba"));
}

#[gpui::test]
fn the_aggregate_operator_select_filters_what_is_typed_and_picks_the_matching_operator(cx: &mut TestAppContext) {
    let (app, visual) = agg_stages_app(cx);
    let first = agg_stage_ids(&app, visual)[0];
    let trigger: &'static str = Box::leak(format!("agg-stage-operator-{first}").into_boxed_str());
    let operators: Vec<&str> = crate::aggregate::COMMON_STAGES.iter().map(|(name, _)| *name).collect();
    let group = operators.iter().position(|name| *name == "$group").unwrap();
    let matching = operators.iter().filter(|name| name.contains("grou")).count();
    assert_eq!(matching, 1);
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger,
            menu_width: 220.0,
            pitch: SELECT_ITEM_PITCH,
            // The common stages and "Custom"; the menu shows twelve rows and scrolls.
            shown: 12,
            query: "GROU".into(),
            matching,
            pick: 0,
            pick_in_full: group,
            value: &|app, visual| agg_draft(app, visual).stages[0].operator.clone(),
            expected: "$group".into(),
        },
    );
}

#[gpui::test]
fn the_chart_x_select_filters_what_is_typed_and_maps_the_pick_back_to_its_column(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    // Columns: region, month, amount, qty, team. "am" is in `amount` (row 2) and `team` (row 4): the
    // second filtered row is the fifth column.
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "chart-x",
            menu_width: 240.0,
            pitch: SELECT_ITEM_PITCH,
            shown: 5,
            query: "AM".into(),
            matching: 2,
            pick: 1,
            pick_in_full: 4,
            value: &|app, visual| spec_of(app, visual).x.to_string(),
            expected: "4".into(),
        },
    );
    assert_eq!(scene_of(&app, visual).model.x_title, "team");
}

#[gpui::test]
fn the_settings_model_select_filters_what_is_typed_and_picks_the_matching_model(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    settings_nav(visual, "ai");
    let models = app.read_with(visual, |this, _| this.assistant.models_for(AiProvider::Anthropic));
    let sonnet = models.iter().position(|model| model.contains("sonnet")).expect("a sonnet model");
    assert_eq!(models.iter().filter(|model| model.contains("sonnet")).count(), 1, "{models:?}");
    assert_ne!(
        app.read_with(visual, |this, _| this.settings_ui.page.as_ref().unwrap().draft.ai.selected().model.clone()),
        models[sonnet],
        "the pick must change the model"
    );
    drive_searchable_select(
        &app,
        visual,
        &SearchableSelect {
            trigger: "setting-control-ai.model",
            menu_width: 220.0,
            pitch: SELECT_ITEM_PITCH,
            shown: models.len(),
            query: "SONN".into(),
            matching: 1,
            pick: 0,
            pick_in_full: sonnet,
            value: &|app, visual| draft_of(app, visual).ai.selected().model.clone(),
            expected: models[sonnet].clone(),
        },
    );
}

#[gpui::test]
fn the_chart_values_combobox_filters_what_is_typed_and_toggles_the_matching_column(cx: &mut TestAppContext) {
    let (app, visual) = chart_app(cx, sales_batch(600));
    show_chart(&app, visual);
    assert_eq!(spec_of(&app, visual).values, vec![2]);
    click_selector(visual, "chart-values");
    assert_eq!(open_menu(visual, "chart-values", 240.0, true).map(|menu| menu.rows), Some(5));
    // Columns: region, month, amount, qty, team. "am" leaves `amount` and `team`.
    visual.simulate_input("AM");
    settle(visual);
    assert_eq!(open_menu(visual, "chart-values", 240.0, true).map(|menu| menu.rows), Some(2), "only the matches stay");
    visual.simulate_keystrokes("secondary-a backspace");
    settle(visual);
    assert_eq!(open_menu(visual, "chart-values", 240.0, true).map(|menu| menu.rows), Some(5), "clearing brings every column back");
    visual.simulate_input("AM");
    settle(visual);
    assert_eq!(open_menu(visual, "chart-values", 240.0, true).map(|menu| menu.rows), Some(2));
    // The second filtered row is the fifth column.
    click_select_option(visual, "chart-values", 48.7, 1);
    assert_eq!(spec_of(&app, visual).values, vec![2, 4], "team, not the second column of the full list");
    visual.simulate_keystrokes("escape");
    settle(visual);
    click_selector(visual, "chart-values");
    assert_eq!(app.read_with(visual, |this, cx| this.av().chart.values_selection_for_test(cx)), vec![2, 4], "reopened with both selected");
    visual.simulate_keystrokes("escape");
    settle(visual);
}

/// gpui-component's `Vec<T>` delegate has no search: a `.searchable(true)` Select needs `SearchableVec<T>`.
/// A source file that makes a Select searchable must not build any Select or Combobox on a plain `Vec`
/// of `String` or chart choices (fixed lists of `&'static str` or `SharedString`, as the FK action Select of
/// the properties editor, are not searchable and stay; this only looks per file, so it is a net, not a proof).
#[test]
fn no_searchable_select_is_built_on_a_plain_vec() {
    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name != "tests") {
                    sources(&path, out);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    const PLAIN: [&str; 6] = [
        "SelectState<Vec<String>>",
        "SelectState<Vec<Choice>>",
        "ComboboxState<Vec<",
        "SelectEvent<Vec<String>>",
        "SelectState::new(Vec",
        "ComboboxState::new(Vec",
    ];
    let mut files = Vec::new();
    sources(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        if !text.contains(".searchable(true)") && !text.contains(".searchable(searchable)") {
            continue;
        }
        for plain in PLAIN {
            assert!(!text.contains(plain), "{} makes a Select searchable and uses `{plain}`: use `SearchableVec`", file.display());
        }
    }
}
