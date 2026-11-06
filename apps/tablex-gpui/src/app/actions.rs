gpui::actions!(
    tablex,
    [
        ConnectConnection,
        DisconnectConnection,
        CancelConnectionConnect,
        RefreshConnection,
        TogglePinConnection,
        CopyConnectionString,
        CopyConnectionName,
        CopyConnectionUrl,
        CopyConnectionUrlWithPassword,
        CopyConnectionHostPort,
        NewConnectionQuery,
        CreateDatabase,
        OpenVisibleFilter,
        DuplicateConnection,
        MoveConnectionToNewGroup,
        SaveActiveSql,
        SaveSqlFileAs,
        OpenSqlFile,
        FormatSql,
        ExplainPlan,
        ExplainPlanAnalyze,
        CommitTransaction,
        RollbackTransaction,
        ToggleAutoCommit,
        EditConnection,
        DeleteConnection,
        OpenNavigatorNode,
        CopyNavigatorSql,
        EditGridCell,
        DeleteGridRow,
        CopyGridSelection,
        CancelCellEdit,
        SortGridPageAscending,
        SortGridPageDescending,
        SortGridDatabaseAscending,
        SortGridDatabaseDescending,
        ClearGridSort,
        RefreshGridResults,
        OpenGridCellDetails,
        OpenGridColumnDetails,
        OpenGridRowDetails,
        PinGridColumn,
        UnpinGridColumn,
        ToggleRecordView,
        TransposeGridRow,
        ClearGridSelection,
        SetGridSelectionNull,
        BulkEditGridSelection,
        CloneGridRow,
        OpenGridFind,
        CloseGridFind,
        FocusDiagramSearch,
        ClearDiagramSelection,
        DiagramUndo,
        DiagramRedo,
        PropertiesUndo,
        PropertiesRedo,
        MongoFindEnter,
        MongoSortEnter,
        CloseActiveTab,
        ReopenClosedTab,
        NextTabGroup,
        PreviousTabGroup,
        ToggleErrorLog,
        ToggleAssistant,
        AssistantEscape,
        AssistantExplain,
        AssistantFix,
        AssistantOptimise,
        OpenCommandPalette,
        OpenSettings,
        CloseSettings,
        PaletteNewQuery,
        TabPickerPrev,
        TabPickerNext,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
    ]
);

pub(super) fn bind_keys(cx: &mut gpui::App) {
    // Primary+F with nothing focused has no focus path to carry the action to a
    // view, so the app-level fallback (it only runs when no focused ancestor
    // handled it) finds the window's `AppRoot` and opens the results find bar.
    // `open_grid_find` ignores tabs that show no grid.
    cx.on_action(|_: &OpenGridFind, cx: &mut gpui::App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        // The window is leased while its action is dispatched; update it once
        // the dispatch has finished.
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| {
                        root.read(cx)
                            .view()
                            .clone()
                            .downcast::<super::AppRoot>()
                            .ok()
                    });
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.open_active_grid_find(window, cx));
                }
            });
        });
    });
    super::command_palette::register_open_action(cx);
    // Like the palette: nothing may be focused, so the app-level fallback
    // finds the window's `AppRoot`.
    // The menu's "AI Assistant" and the shortcut with nothing focused: same fallback.
    cx.on_action(|_: &ToggleAssistant, cx: &mut gpui::App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| {
                        root.read(cx)
                            .view()
                            .clone()
                            .downcast::<super::AppRoot>()
                            .ok()
                    });
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.toggle_assistant(window, cx));
                }
            });
        });
    });
    cx.on_action(|_: &OpenSettings, cx: &mut gpui::App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| {
                        root.read(cx)
                            .view()
                            .clone()
                            .downcast::<super::AppRoot>()
                            .ok()
                    });
                if let Some(app) = app {
                    app.update(cx, |app, cx| app.open_settings(window, cx));
                }
            });
        });
    });
    // The SQL editor's own commands act on the active tab, so they work with nothing focused too.
    app_action::<SaveActiveSql>(cx, |app, window, cx| app.save_active_sql(window, cx));
    app_action::<SaveSqlFileAs>(cx, |app, window, cx| app.save_sql_file_as(window, cx));
    app_action::<OpenSqlFile>(cx, |app, window, cx| app.open_sql_file(window, cx));
    app_action::<FormatSql>(cx, |app, window, cx| app.format_sql(window, cx));
    super::shortcuts::bind_defaults(cx);
}

/// Runs `run` on the window's `AppRoot` when action `A` reaches the app with no focused view
/// above it that handled it.
fn app_action<A: gpui::Action>(
    cx: &mut gpui::App,
    run: impl Fn(&mut super::AppRoot, &mut gpui::Window, &mut gpui::Context<super::AppRoot>) + Clone + 'static,
) {
    cx.on_action(move |_: &A, cx: &mut gpui::App| {
        let Some(window) = cx.active_window() else {
            return;
        };
        let run = run.clone();
        // The window is leased while its action is dispatched; update it once the dispatch ends.
        cx.defer(move |cx| {
            let _ = window.update(cx, |root, window, cx| {
                let app = root
                    .downcast::<gpui_component::Root>()
                    .ok()
                    .and_then(|root| root.read(cx).view().clone().downcast::<super::AppRoot>().ok());
                if let Some(app) = app {
                    app.update(cx, |app, cx| run(app, window, cx));
                }
            });
        });
    });
}
