// Shortcuts editor proofs: the default keymap, recording through the real
// page, conflicts, Apply re-binding the live keymap, and persistence.

/// The key text of a platform primary chord, as a test types it.
fn primary(rest: &str) -> String {
    format!(
        "{}-{rest}",
        if cfg!(target_os = "macos") {
            "cmd"
        } else {
            "ctrl"
        }
    )
}

fn open_shortcuts_page(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext) {
    open_settings_page(app, visual);
    settings_nav(visual, "shortcuts");
    assert!(rendered(visual, "shortcuts-reset-all".into()));
}

/// Click a chip (or Record) and type the chord.
fn record_chord(visual: &mut gpui::VisualTestContext, button: String, keys: &str) {
    click(visual, button);
    visual.simulate_keystrokes(keys);
    settle(visual);
}

fn draft_shortcuts(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
) -> crate::app::shortcuts::Overrides {
    draft_of(app, visual).shortcuts
}

fn page_error(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    id: &str,
) -> Option<String> {
    app.read_with(visual, |this, _| {
        this.settings_ui
            .page
            .as_ref()
            .and_then(|page| page.shortcut_error(id))
    })
}

/// What the old, hand-written `bind_keys` tables bound (before the registry).
fn old_binding_table(cx: &gpui::App) -> Vec<gpui::KeyBinding> {
    use super::actions::*;
    use super::menus::{Hide, HideOtherApps, Minimize, Quit};
    let pane = if cfg!(target_os = "macos") {
        "secondary-alt"
    } else {
        "alt"
    };
    let mut table = vec![
        shortcuts::key_binding(cx, "secondary-k", OpenCommandPalette, None),
        shortcuts::key_binding(cx, "secondary-p", OpenCommandPalette, None),
        shortcuts::key_binding(cx, "secondary-,", OpenSettings, None),
        shortcuts::key_binding(cx, "escape", CloseSettings, Some("settings")),
        shortcuts::key_binding(cx, "shift-enter", PaletteNewQuery, Some("List")),
        shortcuts::key_binding(cx, "secondary-c", CopyGridSelection, Some("grid")),
        shortcuts::key_binding(cx, "secondary-r", RefreshGridResults, Some("grid")),
        shortcuts::key_binding(cx, "secondary-c", CopyConnectionName, Some("sidebar")),
        shortcuts::key_binding(cx, "secondary-e", EditConnection, Some("sidebar")),
        shortcuts::key_binding(cx, "f5", RefreshConnection, Some("sidebar")),
        shortcuts::key_binding(cx, "secondary-s", SaveActiveSql, None),
        shortcuts::key_binding(cx, "secondary-shift-e", ExplainPlan, None),
        shortcuts::key_binding(cx, "secondary-w", CloseActiveTab, None),
        shortcuts::key_binding(cx, "secondary-shift-pagedown", NextTabGroup, None),
        shortcuts::key_binding(cx, "secondary-shift-pageup", PreviousTabGroup, None),
        shortcuts::key_binding(cx, "secondary-shift-]", NextTabGroup, None),
        shortcuts::key_binding(cx, "secondary-shift-[", PreviousTabGroup, None),
        shortcuts::key_binding(cx, "secondary-shift-l", ToggleErrorLog, None),
        shortcuts::key_binding(cx, "secondary-shift-a", ToggleAssistant, None),
        shortcuts::key_binding(cx, "escape", AssistantEscape, Some("assistant")),
        shortcuts::key_binding(cx, &format!("{pane}-left"), FocusPaneLeft, None),
        shortcuts::key_binding(cx, &format!("{pane}-right"), FocusPaneRight, None),
        shortcuts::key_binding(cx, &format!("{pane}-up"), FocusPaneUp, None),
        shortcuts::key_binding(cx, &format!("{pane}-down"), FocusPaneDown, None),
        shortcuts::key_binding(cx, "up", TabPickerPrev, Some("tab-picker > Input")),
        shortcuts::key_binding(cx, "down", TabPickerNext, Some("tab-picker > Input")),
        shortcuts::key_binding(cx, "enter", EditGridCell, Some("grid")),
        shortcuts::key_binding(cx, "escape", CancelCellEdit, Some("grid")),
        shortcuts::key_binding(cx, "secondary-f", OpenGridFind, None),
        shortcuts::key_binding(cx, "secondary-f", OpenGridFind, Some("grid-find > Input")),
        shortcuts::key_binding(cx, "escape", CloseGridFind, Some("grid-find")),
        shortcuts::key_binding(cx, "secondary-f", FocusDiagramSearch, Some("diagram")),
        shortcuts::key_binding(cx, "secondary-f", FocusDiagramSearch, Some("diagram > Input")),
        shortcuts::key_binding(cx, "escape", ClearDiagramSelection, Some("diagram")),
        shortcuts::key_binding(cx, "secondary-z", DiagramUndo, Some("diagram")),
        shortcuts::key_binding(cx, "secondary-shift-z", DiagramRedo, Some("diagram")),
        shortcuts::key_binding(cx, "secondary-z", PropertiesUndo, Some("properties-edit")),
        shortcuts::key_binding(cx, "secondary-shift-z", PropertiesRedo, Some("properties-edit")),
        shortcuts::key_binding(cx, "enter", MongoFindEnter, Some("mongo-find > Input")),
        shortcuts::key_binding(cx, 
            "secondary-enter",
            MongoFindEnter,
            Some("mongo-find > Input"),
        ),
        shortcuts::key_binding(cx, "enter", MongoSortEnter, Some("mongo-sort > Input")),
        shortcuts::key_binding(cx, 
            "secondary-enter",
            MongoSortEnter,
            Some("mongo-sort > Input"),
        ),
        // Record view from the focused grid.
        shortcuts::key_binding(cx, "tab", ToggleRecordView, Some("grid")),
        shortcuts::key_binding(cx, "secondary-q", Quit, None),
        // The SQL editor's files and formatting.
        shortcuts::key_binding(cx, "secondary-o", OpenSqlFile, None),
        shortcuts::key_binding(cx, "secondary-shift-s", SaveSqlFileAs, None),
        shortcuts::key_binding(cx, 
            if cfg!(target_os = "macos") { "alt-shift-f" } else { "secondary-shift-f" },
            FormatSql,
            None,
        ),
    ];
    if !cfg!(target_os = "macos") {
        table.push(shortcuts::key_binding(cx, 
            "secondary-y",
            DiagramRedo,
            Some("diagram"),
        ));
        table.push(shortcuts::key_binding(cx, 
            "secondary-y",
            PropertiesRedo,
            Some("properties-edit"),
        ));
    }
    if cfg!(target_os = "macos") {
        table.extend([
            shortcuts::key_binding(cx, "secondary-h", Hide, None),
            shortcuts::key_binding(cx, "secondary-alt-h", HideOtherApps, None),
            shortcuts::key_binding(cx, "secondary-m", Minimize, None),
        ]);
    }
    table
}

fn app_bindings(cx: &gpui::App) -> Vec<String> {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .bindings()
        .filter(|binding| {
            let name = binding.action().name();
            name.starts_with("tablex::") || name.starts_with("tablex_menu::")
        })
        .map(|binding| format!("{binding:?}"))
        .collect()
}

#[gpui::test]
fn the_registry_binds_exactly_the_old_default_keymap(cx: &mut TestAppContext) {
    let actual = cx.update(|cx| {
        actions::bind_keys(cx);
        // Binding twice (palette setup, then menu setup) must not duplicate.
        shortcuts::bind_defaults(cx);
        app_bindings(cx)
    });
    let mut expected: Vec<String> = cx.update(|cx| old_binding_table(cx))
        .iter()
        .map(|binding| format!("{binding:?}"))
        .collect();
    let mut actual = actual;
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "every old binding resolves to the same chord, context and action"
    );
}

#[gpui::test]
fn recording_through_the_page_edits_the_draft_and_apply_rebinds_the_live_keymap(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_shortcuts_page(&app, visual);
    assert!(rendered(visual, "shortcut-workspace.open_settings".into()));
    assert!(draft_shortcuts(&app, visual).is_empty());

    // Click the chord of "Open Settings" and press a new one.
    record_chord(
        visual,
        "shortcut-chip-workspace.open_settings-0".into(),
        &primary("shift-o"),
    );
    let wanted = vec!["secondary-shift-o".to_owned()];
    assert_eq!(
        draft_shortcuts(&app, visual).get("workspace.open_settings"),
        Some(&wanted)
    );
    assert!(
        applied(&app, visual).shortcuts.is_empty(),
        "a draft until Apply"
    );
    assert!(
        rendered(visual, "shortcut-reset-workspace.open_settings".into()),
        "a changed shortcut offers its reset"
    );
    assert!(app.read_with(visual, |this, _| this.settings_dirty()));
    assert!(page_error(&app, visual, "workspace.open_settings").is_none());

    click(visual, "settings-apply".into());
    assert_eq!(
        applied(&app, visual)
            .shortcuts
            .get("workspace.open_settings"),
        Some(&wanted)
    );
    click(visual, "settings-close".into());
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));

    visual.simulate_keystrokes(&primary(","));
    settle(visual);
    assert_ne!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the old chord is gone"
    );
    visual.simulate_keystrokes(&primary("shift-o"));
    settle(visual);
    assert_eq!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the new chord runs the action"
    );

    // The title-bar tooltip data shows the chord in force.
    let label = app.read_with(visual, |this, _| {
        shortcuts::label(&this.settings.shortcuts, "workspace.open_settings")
    });
    assert_eq!(
        label,
        Some(shortcuts::display(
            "secondary-shift-o",
            cfg!(target_os = "macos")
        ))
    );

    // Reset to default re-binds the original chord and frees the new one.
    settings_nav(visual, "shortcuts");
    click(visual, "shortcut-reset-workspace.open_settings".into());
    assert!(draft_shortcuts(&app, visual).is_empty());
    click(visual, "settings-apply-close".into());
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));
    visual.simulate_keystrokes(&primary("shift-o"));
    settle(visual);
    assert_ne!(active_kind(&app, visual), Some(TabKind::Settings));
    visual.simulate_keystrokes(&primary(","));
    settle(visual);
    assert_eq!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the default is back"
    );
}

#[gpui::test]
fn a_chord_used_in_an_overlapping_context_is_blocked_with_the_other_actions_name(
    cx: &mut TestAppContext,
) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_shortcuts_page(&app, visual);

    // Save Script is global; Close Tab already owns secondary-w.
    record_chord(
        visual,
        "shortcut-chip-editor.save_script-0".into(),
        &primary("w"),
    );
    assert!(
        draft_shortcuts(&app, visual).is_empty(),
        "the change was blocked"
    );
    let message = page_error(&app, visual, "editor.save_script").expect("an inline message");
    assert!(message.contains("Close Tab"), "{message}");
    assert!(
        rendered(visual, "shortcut-error-editor.save_script".into()),
        "shown in the row"
    );
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));

    // A global action cannot take a context-scoped chord either (grid copy).
    record_chord(
        visual,
        "shortcut-chip-editor.save_script-0".into(),
        &primary("c"),
    );
    assert!(page_error(&app, visual, "editor.save_script")
        .unwrap()
        .contains("Copy"));

    // The same chord in disjoint contexts is fine: sidebar "Edit Connection"
    // takes the grid's secondary-r.
    record_chord(
        visual,
        "shortcut-chip-sidebar.edit_connection-0".into(),
        &primary("r"),
    );
    assert_eq!(
        draft_shortcuts(&app, visual).get("sidebar.edit_connection"),
        Some(&vec!["secondary-r".to_owned()])
    );
    assert!(page_error(&app, visual, "sidebar.edit_connection").is_none());

    // A bare character is typed text, not a shortcut.
    record_chord(
        visual,
        "shortcut-chip-workspace.open_settings-0".into(),
        "j",
    );
    assert!(page_error(&app, visual, "workspace.open_settings").is_some());
    assert!(!draft_shortcuts(&app, visual).contains_key("workspace.open_settings"));
}

#[gpui::test]
fn escape_cancels_recording_and_reset_and_remove_edit_the_draft(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_shortcuts_page(&app, visual);

    // Escape while recording: nothing changes, the page is not closed and no
    // discard question appears.
    click(visual, "shortcut-chip-workspace.close_tab-0".into());
    assert!(app.read_with(visual, |this, _| this
        .settings_ui
        .page
        .as_ref()
        .unwrap()
        .is_recording()));
    visual.simulate_keystrokes("escape");
    settle(visual);
    assert!(!app.read_with(visual, |this, _| this
        .settings_ui
        .page
        .as_ref()
        .unwrap()
        .is_recording()));
    assert!(draft_shortcuts(&app, visual).is_empty());
    assert_eq!(active_kind(&app, visual), Some(TabKind::Settings));
    assert!(!rendered(visual, "settings-discard-body".into()));
    assert!(page_error(&app, visual, "workspace.close_tab").is_none());

    // Recording swallows a bound chord: it must not run its action.
    click(visual, "shortcut-record-workspace.close_tab".into());
    visual.simulate_keystrokes(&primary("w"));
    settle(visual);
    assert_eq!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the tab was not closed by the recorded chord"
    );

    // Remove one of the palette's two chords, then the other.
    click(
        visual,
        "shortcut-remove-workspace.open_command_palette-0".into(),
    );
    assert_eq!(
        draft_shortcuts(&app, visual).get("workspace.open_command_palette"),
        Some(&vec!["secondary-p".to_owned()])
    );
    click(
        visual,
        "shortcut-remove-workspace.open_command_palette-0".into(),
    );
    assert_eq!(
        draft_shortcuts(&app, visual).get("workspace.open_command_palette"),
        Some(&Vec::new())
    );
    // Record adds a chord to the now empty action.
    record_chord(
        visual,
        "shortcut-record-workspace.open_command_palette".into(),
        &primary("shift-p"),
    );
    assert_eq!(
        draft_shortcuts(&app, visual).get("workspace.open_command_palette"),
        Some(&vec!["secondary-shift-p".to_owned()])
    );

    // Reset per item.
    click(
        visual,
        "shortcut-reset-workspace.open_command_palette".into(),
    );
    assert!(draft_shortcuts(&app, visual).is_empty());

    // Reset all.
    record_chord(
        visual,
        "shortcut-chip-workspace.close_tab-0".into(),
        &primary("shift-w"),
    );
    record_chord(
        visual,
        "shortcut-chip-sidebar.edit_connection-0".into(),
        &primary("shift-j"),
    );
    assert_eq!(draft_shortcuts(&app, visual).len(), 2);
    click(visual, "shortcuts-reset-all".into());
    assert!(draft_shortcuts(&app, visual).is_empty());
    assert!(!app.read_with(visual, |this, _| this.settings_dirty()));

    // Reset Defaults in the footer covers the shortcuts too.
    record_chord(
        visual,
        "shortcut-chip-workspace.close_tab-0".into(),
        &primary("shift-w"),
    );
    click(visual, "settings-reset-defaults".into());
    assert!(draft_shortcuts(&app, visual).is_empty());
}

#[gpui::test]
fn removing_a_binding_unbinds_it_after_apply(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_shortcuts_page(&app, visual);
    click(visual, "shortcut-remove-workspace.open_settings-0".into());
    assert_eq!(
        draft_shortcuts(&app, visual).get("workspace.open_settings"),
        Some(&Vec::new())
    );
    assert!(rendered(
        visual,
        "shortcut-record-workspace.open_settings".into()
    ));
    click(visual, "settings-apply-close".into());
    visual.simulate_keystrokes(&primary(","));
    settle(visual);
    assert_ne!(
        active_kind(&app, visual),
        Some(TabKind::Settings),
        "the unbound chord does nothing"
    );
}

#[gpui::test]
fn the_search_box_finds_shortcuts_by_title_category_and_chord(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    open_settings_page(&app, visual);
    let find = |visual: &mut gpui::VisualTestContext, text: &str| {
        let search = visual.debug_bounds("settings-search").expect("search box");
        visual.simulate_click(search.center(), Default::default());
        visual.simulate_keystrokes(&primary("a"));
        visual.simulate_input(text);
        settle(visual);
    };
    find(visual, "close tab");
    assert!(
        rendered(visual, "shortcut-workspace.close_tab".into()),
        "by title"
    );
    assert!(!rendered(visual, "shortcut-workspace.open_settings".into()));
    find(visual, "sidebar");
    assert!(
        rendered(visual, "shortcut-sidebar.edit_connection".into()),
        "by category"
    );
    find(visual, "secondary-w");
    assert!(
        rendered(visual, "shortcut-workspace.close_tab".into()),
        "by chord text"
    );
    find(
        visual,
        &shortcuts::display("secondary-w", cfg!(target_os = "macos")),
    );
    assert!(
        rendered(visual, "shortcut-workspace.close_tab".into()),
        "by the displayed chord"
    );
    find(visual, "zzzz-nothing");
    assert!(!rendered(visual, "shortcut-workspace.close_tab".into()));
}

#[gpui::test]
fn overrides_are_saved_loaded_at_start_and_unknown_ids_are_ignored(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    let path = settings_temp_path();
    app.update(visual, |this, _| this.settings_ui.path = Some(path.clone()));
    open_shortcuts_page(&app, visual);
    record_chord(
        visual,
        "shortcut-chip-workspace.open_settings-0".into(),
        &primary("shift-o"),
    );
    click(visual, "settings-apply".into());
    visual
        .executor()
        .advance_clock(std::time::Duration::from_millis(400));
    wait_until(visual, "the settings file", |_| path.exists());
    let saved = crate::settings::store::load(&path, &crate::settings::store::Legacy::default());
    assert_eq!(
        saved.settings.shortcuts.get("workspace.open_settings"),
        Some(&vec!["secondary-shift-o".to_owned()])
    );

    // Hand-add an id this build does not know, then start a fresh app on the file.
    let mut file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    file["shortcuts"]["no.such.action"] = serde_json::json!(["secondary-x"]);
    std::fs::write(&path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();
    click(visual, "settings-close".into());

    let (fresh, visual) = {
        let path = path.clone();
        let (fresh, visual) = settings_app(cx);
        fresh.update(visual, |this, cx| {
            this.settings = std::sync::Arc::new(Settings::default());
            this.settings_ui.path = Some(path);
            this.init_settings(cx);
        });
        (fresh, visual)
    };
    assert_eq!(
        fresh.read_with(visual, |this, _| this
            .settings
            .shortcuts
            .get("workspace.open_settings")
            .cloned()),
        Some(vec!["secondary-shift-o".to_owned()]),
        "loaded from disk"
    );
    assert_eq!(
        fresh.read_with(visual, |this, _| this.settings.shortcuts.len()),
        2,
        "the unknown id is kept in the file but does nothing"
    );
    visual.simulate_keystrokes(&primary("shift-o"));
    settle(visual);
    assert_eq!(
        active_kind(&fresh, visual),
        Some(TabKind::Settings),
        "the saved chord is bound at start"
    );
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[gpui::test]
fn the_menus_and_palette_show_the_chord_in_force(cx: &mut TestAppContext) {
    cx.dispatcher.allow_parking();
    let (app, visual) = settings_app(cx);
    visual.update(|_, cx| menus::finish_launch(cx, |_| {}));
    let chord_of_quit = |visual: &mut gpui::VisualTestContext| {
        visual.update(|_, cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            keymap
                .bindings_for_action(&menus::Quit)
                .last()
                .map(|binding| binding.keystrokes()[0].unparse(cx.platform_info().os))
        })
    };
    assert!(chord_of_quit(visual).is_some());
    app.update(visual, |this, cx| {
        let mut next = (**this.settings()).clone();
        next.shortcuts
            .insert("app.quit".into(), vec!["secondary-shift-q".into()]);
        this.apply_settings(next, None, cx);
    });
    let after = chord_of_quit(visual).expect("quit stays bound");
    assert!(after.contains("shift") && after.ends_with('q'), "{after}");
    // The command palette's description of Quit names the new chord.
    let detail = app.read_with(visual, |this, _| {
        command_palette::StaticAction::Quit
            .detail(&this.settings.shortcuts)
            .to_string()
    });
    assert!(
        detail.ends_with(&shortcuts::display(
            "secondary-shift-q",
            cfg!(target_os = "macos")
        )),
        "{detail}"
    );
}
