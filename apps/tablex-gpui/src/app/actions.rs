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
        RunActiveQuery,
        RunScript,
        RunFromCursor,
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
        ToggleResults,
        ToggleVimMode,
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
        // The window is leased while its action is dispatched (it cannot be read then); find
        // and update it once the dispatch has finished. A results window handles the action
        // itself (`results::placement`); here, the app's window.
        cx.defer(move |cx| {
            let Some((window, app)) = super::menus::app_window(cx) else {
                return;
            };
            let _ = window.update(cx, |_, window, cx| {
                app.update(cx, |app, cx| app.open_active_grid_find(window, cx));
            });
        });
    });
    super::command_palette::register_open_action(cx);
    // Like the palette: nothing may be focused, so the app-level fallback finds the app's window and
    // its `AppRoot` (the active window may be a results window, which holds none).
    // The menu's "AI Assistant" and the shortcut with nothing focused: same fallback.
    super::menus::on_app_root::<ToggleAssistant>(cx, |app, window, cx| app.toggle_assistant(window, cx));
    super::menus::on_app_root::<OpenSettings>(cx, |app, window, cx| app.open_settings(window, cx));
    // The SQL editor's own commands act on the active tab, so they work with nothing focused too.
    super::menus::on_app_root::<SaveActiveSql>(cx, |app, window, cx| app.save_active_sql(window, cx));
    super::menus::on_app_root::<SaveSqlFileAs>(cx, |app, window, cx| app.save_sql_file_as(window, cx));
    super::menus::on_app_root::<OpenSqlFile>(cx, |app, window, cx| app.open_sql_file(window, cx));
    super::menus::on_app_root::<FormatSql>(cx, |app, window, cx| app.format_sql(window, cx));
    // Hide / show the result area of the active tab; a results window handles it itself, so
    // this is the app's window.
    cx.on_action(|_: &ToggleResults, cx: &mut gpui::App| {
        cx.defer(move |cx| {
            if let Some((_, app)) = super::menus::app_window(cx) {
                app.update(cx, |app, cx| app.toggle_active_results(cx));
            }
        });
    });
    super::shortcuts::bind_defaults(cx);
}
