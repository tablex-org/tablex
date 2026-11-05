//! The applied settings of the running app: the `Arc<Settings>` snapshot,
//! loading, the debounced off-thread save, and propagation of a change to the
//! views that depend on it. The Settings tab (`settings_view`) edits a draft
//! and calls [`AppRoot::apply_settings`]; nothing else writes `self.settings`.

use super::*;
use crate::gpui_session::SessionWriter;
use crate::settings::registry::{self, Section};
use crate::settings::{store, Settings, ThemeChoice};
use crate::sidebar_preferences::SidebarPreferences;
use gpui::SharedString;
use std::path::PathBuf;

/// Writes are coalesced for this long after the last change.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Grid display values derived from the settings once per change, so the row
/// renderer reads plain fields (one clone of a `SharedString` per row, none
/// per cell).
#[derive(Clone)]
pub(super) struct GridDisplay {
    pub(super) null_text: SharedString,
    pub(super) right_align_numbers: bool,
    pub(super) show_row_numbers: bool,
    /// `None`: inherit the interface font.
    pub(super) font_family: Option<SharedString>,
    pub(super) font_size: f32,
    pub(super) column_width_default: f32,
    pub(super) column_width_min: f32,
    pub(super) column_width_max: f32,
    pub(super) auto_size_columns: bool,
}

impl GridDisplay {
    /// The text a cell shows, and whether it is muted: a NULL (drawn with the
    /// configured NULL text, read without touching the preview cache) or a
    /// missing document field (the source's own marker).
    pub(super) fn cell_text(
        &self,
        grid: &crate::grid::GridState<crate::grid_source::GridContentSource>,
        position: crate::grid::CellPosition,
    ) -> (SharedString, bool) {
        match grid.cell_state(position) {
            crate::grid::CellState::Null => (self.null_text.clone(), true),
            crate::grid::CellState::Missing => (grid.cell_text(position).unwrap_or_default(), true),
            crate::grid::CellState::Value => (grid.cell_text(position).unwrap_or_default(), false),
        }
    }

    pub(super) fn from_settings(settings: &Settings) -> Self {
        let results = &settings.results;
        Self {
            null_text: SharedString::from(results.null_text.clone()),
            right_align_numbers: results.right_align_numbers,
            show_row_numbers: results.show_row_numbers,
            font_family: Some(settings.appearance.grid_font.clone())
                .filter(|font| !font.is_empty())
                .map(SharedString::from),
            font_size: settings.appearance.grid_font_size,
            column_width_default: results.column_width_default as f32,
            column_width_min: results.column_width_min as f32,
            column_width_max: results.column_width_max as f32,
            auto_size_columns: results.auto_size_columns,
        }
    }
}

/// Fonts the theme had before any setting touched it.
struct ThemeFonts {
    interface: SharedString,
    mono: SharedString,
    mono_size: gpui::Pixels,
}

pub(super) struct SettingsUi {
    /// Where `settings.json` lives; `None` disables loading and saving.
    pub(super) path: Option<PathBuf>,
    writer: Arc<SessionWriter>,
    pub(super) pending: Option<Task<()>>,
    /// Number of file writes dispatched (tests assert debouncing with it).
    pub(super) writes: usize,
    /// "Let MCP clients run changes" was turned off and the file may still say on (the write
    /// failed): a later failed save keeps saying so, and the first save that lands takes the
    /// notice back.
    mcp_off_unsaved: bool,
    /// The Settings tab's draft and controls, alive while the tab is open.
    pub(super) page: Option<super::settings_view::SettingsPage>,
    /// The text each script tab last saved to disk (unsaved-close check).
    pub(super) saved_scripts: HashMap<u64, String>,
    /// Editors reconfigured in place by settings changes (test evidence).
    #[cfg(test)]
    pub(super) editors_reconfigured: usize,
    /// Capture scenario: the draft and page the Settings tab opens with.
    pub(super) scenario_seed: Option<super::settings_view::ScenarioSeed>,
    theme_fonts: Option<ThemeFonts>,
    _quit: Option<gpui::Subscription>,
}

impl SettingsUi {
    pub(super) fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            writer: Arc::new(SessionWriter::default()),
            pending: None,
            writes: 0,
            mcp_off_unsaved: false,
            page: None,
            saved_scripts: HashMap::new(),
            #[cfg(test)]
            editors_reconfigured: 0,
            scenario_seed: None,
            theme_fonts: None,
            _quit: None,
        }
    }
}

/// Real runs use the config dir; scenario runs the folder of the isolated
/// sidebar-preferences file (nothing, when the launcher gave none); plain
/// unit-test apps never touch the user's files.
pub(super) fn resolve_settings_path(
    scenario_enabled: bool,
    sidebar_prefs_path: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if scenario_enabled {
        sidebar_prefs_path
            .and_then(std::path::Path::parent)
            .map(|dir| dir.join(store::FILE_NAME))
    } else if cfg!(test) {
        None
    } else {
        store::default_path()
    }
}

impl AppRoot {
    #[cfg(test)]
    /// The applied settings (a cheap shared snapshot; never read from disk).
    pub(super) fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    /// Loads `settings.json` (migrating the legacy sidebar choices on first
    /// run), publishes the result and arms the quit-time flush. Called once
    /// from the constructor, after the sidebar preferences loaded.
    pub(super) fn init_settings(&mut self, cx: &mut Context<Self>) {
        let theme = gpui_component::theme::Theme::global(cx);
        self.settings_ui.theme_fonts = Some(ThemeFonts {
            interface: theme.font_family.clone(),
            mono: theme.mono_font_family.clone(),
            mono_size: theme.mono_font_size,
        });
        let mut notice = None;
        if let Some(path) = self.settings_ui.path.clone() {
            let legacy = store::Legacy::read(self.sidebar.path.as_deref());
            let loaded = store::load(&path, &legacy);
            notice = loaded.notice;
            self.settings = Arc::new(loaded.settings);
            if loaded.needs_save {
                self.save_settings_now(cx);
            }
            self.settings_ui._quit = Some(cx.on_app_quit(|this, _| {
                let _ = this.flush_settings_blocking();
                async {}
            }));
        }
        // The copy default used to live in its own file: a choice made there
        // (and not yet in Settings) carries over once.
        if self.settings.results.default_copy_format == crate::settings::CopyDefault::Smart {
            if let Some(choice) = self.grid_prefs.copy_default.to_setting() {
                let mut next = (*self.settings).clone();
                next.results.default_copy_format = choice;
                self.settings = Arc::new(next);
                self.save_settings_now(cx);
            }
        }
        if let Some(message) = notice {
            self.report_warning(Area::App, None, &message);
            self.sidebar.notice = Some(message);
        }
        self.after_settings_changed(&Settings::default(), None, cx);
    }

    /// Replaces the applied settings with `new`, saves them (debounced, off
    /// the UI thread) and updates only the views the changed settings feed.
    pub(super) fn apply_settings(
        &mut self,
        new: Settings,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        if new == *self.settings {
            return;
        }
        let old = std::mem::replace(&mut self.settings, Arc::new(new));
        if let Some(page) = self.settings_ui.page.as_mut() {
            page.rebase(&old, &self.settings);
        }
        self.after_settings_changed(&old, window, cx);
        self.schedule_settings_save(cx);
        // Turning "Let MCP clients run changes" off is a safety switch that `tablex-mcp` reads from
        // the file at the time of each write: it must not wait out the debounce, or a client's yes
        // that arrives in that window would still run. Written now, on this thread (a few bytes,
        // only on this change), and the pending timer is dropped since there is nothing left to save.
        // If that write fails the file still says ON while the page says off: that is said, in the
        // way a failed debounced save is said, and the debounce stays armed to try again.
        if old.ai.mcp_writes && !self.settings.ai.mcp_writes && self.settings_ui.path.is_some() {
            match self.flush_settings_blocking() {
                Ok(()) => {
                    self.settings_ui.pending = None;
                    self.settings_reached_the_file(cx);
                }
                Err(error) => {
                    self.settings_ui.mcp_off_unsaved = true;
                    self.settings_save_failed(&error, "TableX will try again", cx);
                }
            }
        }
    }

    /// A sidebar / tab-bar control changed one of the settings it mirrors
    /// (connection sort, global search, tab group-by, compact titles, second
    /// row): record it in the settings without pushing it back.
    pub(super) fn mirror_sidebar_into_settings(&mut self, cx: &mut Context<Self>) {
        let prefs = self.navigator.preferences();
        let mut next = (*self.settings).clone();
        next.navigation.connection_sort = prefs.sort;
        next.navigation.global_search = prefs.global_search;
        next.appearance.tab_group_by = prefs.tab_group_by;
        next.appearance.compact_tab_titles = prefs.compact_tab_titles;
        next.appearance.tab_rows = prefs.tab_rows;
        if next == *self.settings {
            return;
        }
        let old = std::mem::replace(&mut self.settings, Arc::new(next));
        if let Some(page) = self.settings_ui.page.as_mut() {
            page.rebase(&old, &self.settings);
        }
        self.schedule_settings_save(cx);
    }

    /// Pushes the global choices that the sidebar stores itself into its
    /// preferences (and so into `sidebar-preferences.json`).
    fn push_settings_to_sidebar(&mut self, cx: &mut Context<Self>) {
        let settings = self.settings.clone();
        let current = self.navigator.preferences();
        if current.sort == settings.navigation.connection_sort
            && current.global_search == settings.navigation.global_search
            && current.tab_group_by == settings.appearance.tab_group_by
            && current.compact_tab_titles == settings.appearance.compact_tab_titles
            && current.tab_rows == settings.appearance.tab_rows
        {
            return;
        }
        let change = |prefs: &mut SidebarPreferences| {
            prefs.sort = settings.navigation.connection_sort;
            prefs.global_search = settings.navigation.global_search;
            prefs.tab_group_by = settings.appearance.tab_group_by;
            prefs.compact_tab_titles = settings.appearance.compact_tab_titles;
            prefs.tab_rows = settings.appearance.tab_rows;
            Ok(())
        };
        if self.sidebar.path.is_some() && self.change_sidebar_preferences(change, cx).is_ok() {
            return;
        }
        // No (writable) preferences file: the choice still applies in memory.
        let mut candidate = self.navigator.preferences().clone();
        let _ = change(&mut candidate);
        let _ = self.navigator.set_preferences(candidate);
    }

    /// Propagation: runs after `self.settings` changed from `old`.
    pub(super) fn after_settings_changed(
        &mut self,
        old: &Settings,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let changed = registry::changed_ids(old, &self.settings);
        let touches = |section: Section| {
            changed
                .iter()
                .filter_map(|id| registry::find(id))
                .any(|descriptor| descriptor.section == section)
        };
        let settings = self.settings.clone();
        cx.set_global(completion::CompletionPrefs {
            auto_trigger: settings.editor.autocomplete_auto_trigger,
        });
        self.grid_display = GridDisplay::from_settings(&settings);
        if old.shortcuts != settings.shortcuts {
            // Only an Apply that changed a shortcut touches the keymap.
            shortcuts::rebind(cx, &old.shortcuts, &settings.shortcuts);
            menus::refresh(cx);
        }
        if changed.contains(&"results.default_copy_format") {
            self.grid_prefs.copy_default =
                results::copy::CopyFormat::from_setting(settings.results.default_copy_format);
        }
        self.workspace
            .set_default_output_limit(i64::from(settings.results.page_size));
        self.sync_connection_output_limits();
        self.release_idle_own_sessions(cx);
        self.workspace
            .set_open_next_to_active(settings.navigation.open_data_tabs_next_to_active);
        self.navigator.set_settings_defaults(
            !settings.navigation.show_system_objects,
            u64::from(settings.navigation.navigator_page_size),
        );

        if changed.contains(&"appearance.theme") {
            let system = window
                .as_deref()
                .map_or_else(|| cx.window_appearance(), Window::appearance);
            self.show_theme(system, cx);
        }
        if touches(Section::Appearance) {
            self.apply_theme_fonts(cx);
        }
        if changed.iter().any(|id| {
            matches!(
                *id,
                "navigation.connection_sort"
                    | "navigation.global_search"
                    | "appearance.tab_group_by"
                    | "appearance.compact_tab_titles"
                    | "appearance.tab_rows"
            )
        }) {
            self.push_settings_to_sidebar(cx);
            if changed.iter().any(|id| id.starts_with("appearance.")) {
                self.apply_tab_bar_preferences(cx);
            }
        }
        if touches(Section::Editor) || touches(Section::Appearance) {
            if let Some(window) = window {
                self.reconfigure_editors(window, cx);
            }
        }
        if touches(Section::Results) || touches(Section::Appearance) {
            self.relayout_grids();
        }
        // Connection types decide the agent access a chat shows, and a connection whose
        // effective access went down (a type made read-only, its preset lowered) loses its
        // agent session now: a statement already running is cancelled, not left to finish.
        if old.connection_types != self.settings.connection_types {
            self.revoke_agent_access_lowered_since(old);
        }
        self.assistant_sync(cx);
        cx.notify();
    }

    /// Shows the theme the setting asks for (`System` resolves against the OS
    /// appearance `system`). Re-deriving the component theme reloads its
    /// stock fonts, so they are applied again; nothing is touched when the
    /// resolved mode is already showing.
    fn show_theme(&mut self, system: gpui::WindowAppearance, cx: &mut Context<Self>) {
        let mode = theme::resolve(self.settings.appearance.theme, system);
        if mode != theme::mode() {
            theme::apply(mode, cx);
            self.apply_theme_fonts(cx);
            cx.notify();
        }
    }

    /// Subscribes `view` to the window's appearance for the window's lifetime.
    pub(super) fn follow_system_appearance(
        view: &gpui::Entity<Self>,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        let observed = view.clone();
        window
            .observe_window_appearance(move |window, cx| {
                let appearance = window.appearance();
                observed.update(cx, |this, cx| {
                    this.system_appearance_changed(appearance, cx)
                });
            })
            .detach();
        // The window may already differ from the app-level appearance that
        // the settings were first applied against.
        let appearance = window.appearance();
        view.update(cx, |this, cx| {
            this.system_appearance_changed(appearance, cx)
        });
    }

    /// The OS appearance changed: under `System` the theme follows it.
    pub(super) fn system_appearance_changed(
        &mut self,
        appearance: gpui::WindowAppearance,
        cx: &mut Context<Self>,
    ) {
        if self.settings.appearance.theme == ThemeChoice::System {
            self.show_theme(appearance, cx);
        }
    }

    /// Scenario runs only: `TABLEX_THEME=dark|light|system` forces the theme
    /// for the capture (not saved, scenario settings are isolated anyway).
    pub(super) fn apply_scenario_theme(&mut self, cx: &mut Context<Self>) {
        let Ok(value) = std::env::var("TABLEX_THEME") else {
            return;
        };
        let Some(choice) = <ThemeChoice as crate::settings::Choice>::from_id(&value) else {
            eprintln!("TABLEX_THEME: expected dark, light or system, got '{value}'");
            return;
        };
        let mut next = (*self.settings).clone();
        next.appearance.theme = choice;
        let old = std::mem::replace(&mut self.settings, Arc::new(next));
        self.after_settings_changed(&old, None, cx);
    }

    /// Interface and editor fonts live in the component theme.
    fn apply_theme_fonts(&mut self, cx: &mut Context<Self>) {
        let Some(defaults) = self.settings_ui.theme_fonts.as_ref() else {
            return;
        };
        let appearance = &self.settings.appearance;
        let interface = if appearance.interface_font.is_empty() {
            defaults.interface.clone()
        } else {
            SharedString::from(appearance.interface_font.clone())
        };
        let mono = if appearance.editor_font.is_empty() {
            defaults.mono.clone()
        } else {
            SharedString::from(appearance.editor_font.clone())
        };
        let mono_size = if (appearance.editor_font_size - 13.0).abs() < f32::EPSILON {
            defaults.mono_size
        } else {
            px(appearance.editor_font_size)
        };
        gpui_component::theme::Theme::update(cx, |theme| {
            theme.font_family = interface;
            theme.mono_font_family = mono;
            theme.mono_font_size = mono_size;
        });
    }

    /// Editors that exist reconfigure in place; new ones read the settings
    /// when they are built.
    fn reconfigure_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.settings.editor.clone();
        let tab_size = tab_size_of(&editor);
        for state in self.query_editors.by_tab.values() {
            #[cfg(test)]
            {
                self.settings_ui.editors_reconfigured += 1;
            }
            state.update(cx, |state, cx| {
                state.set_line_number(editor.line_numbers, window, cx);
                state.set_soft_wrap(editor.soft_wrap, window, cx);
                state.set_tab_size(tab_size, cx);
                state.set_auto_close(editor.auto_close, window, cx);
            });
        }
    }

    /// Column limits and widths of the open results follow the settings.
    fn relayout_grids(&mut self) {
        let display = self.grid_display.clone();
        for view in self.tab_views.values_mut() {
            let layout = &mut view.grid_ui.layout;
            layout.min_width = display.column_width_min;
            layout.max_width = display.column_width_max;
            layout.gutter_width = gutter_width(&display);
            for width in &mut layout.widths {
                *width = width.clamp(layout.min_width, layout.max_width);
            }
        }
    }

    // ---- unsaved scripts --------------------------------------------------------

    /// A query script tab with text that was never saved (or changed since).
    pub(super) fn is_unsaved_script(&self, tab_id: u64) -> bool {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return false;
        };
        // A file-bound tab is saved in its file (see `Tab::has_unsaved_file`).
        tab.kind == TabKind::Query
            && tab.file.is_none()
            && !tab.query_mode.is_table_data()
            && !tab.query_mode.is_mongo_find()
            && !tab.query_mode.is_mongo_aggregate()
            && !tab.sql.trim().is_empty()
            && self
                .settings_ui
                .saved_scripts
                .get(&tab_id)
                .map(String::as_str)
                != Some(tab.sql.as_str())
    }

    /// "Close this unsaved script?" (the "confirm before closing" setting).
    pub(super) fn open_unsaved_script_dialog(
        &mut self,
        tab_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            dialog
                .title("Close this script?")
                .child(
                    div()
                        .debug_selector(|| "unsaved-script-body".into())
                        .child("The script was not saved. Closing the tab discards it."),
                )
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Close"))
                .confirm()
                .on_ok(move |_, window, cx| {
                    let _ = root.update(cx, |this, cx| {
                        // Confirmed: treat the text as saved so the close
                        // below does not ask again.
                        let text = this.workspace.tab(tab_id).map(|tab| tab.sql.clone());
                        this.settings_ui
                            .saved_scripts
                            .insert(tab_id, text.unwrap_or_default());
                        this.confirm_close_tab(tab_id, window, cx);
                    });
                    true
                })
        });
    }

    // ---- saving ---------------------------------------------------------------

    /// Starts (or restarts) the debounce; the file is written 300 ms after the
    /// last change. The pending task is the only timer and exists only while a
    /// change is unsaved.
    fn schedule_settings_save(&mut self, cx: &mut Context<Self>) {
        if self.settings_ui.path.is_none() {
            return;
        }
        self.settings_ui.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DEBOUNCE).await;
            let _ = this.update(cx, |this, cx| {
                this.settings_ui.pending = None;
                this.save_settings_now(cx);
            });
        }));
    }

    /// Encodes now and writes on the background executor (skipped when the
    /// bytes are what was last submitted).
    pub(super) fn save_settings_now(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.settings_ui.path.clone() else {
            return;
        };
        let bytes = match store::encode(&self.settings) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.report_error(
                    Area::App,
                    None,
                    format!("Could not save the settings: {error}"),
                );
                return;
            }
        };
        let Some(ticket) = self.settings_ui.writer.submit(&bytes) else {
            return;
        };
        let writer = self.settings_ui.writer.clone();
        self.settings_ui.writes += 1;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { writer.write(&path, ticket, &bytes) })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(_) => this.settings_reached_the_file(cx),
                Err(error) => this.settings_save_failed(
                    &error,
                    "it is written again at the next settings change and when TableX quits",
                    cx,
                ),
            });
        })
        .detach();
    }

    /// A save failed: logged and shown. While the MCP switch was turned off and has not reached
    /// the file, the notice says what that means (`then` is what happens next) instead of the
    /// plain failure, on every failed attempt and not only the first.
    fn settings_save_failed(&mut self, error: &std::io::Error, then: &str, cx: &mut Context<Self>) {
        let message = if self.settings_ui.mcp_off_unsaved && !self.settings.ai.mcp_writes {
            format!(
                "Could not save the settings: {error}. {MCP_STILL_ON}, so MCP clients can still \
                 be asked to run changes; {then}."
            )
        } else {
            format!("Could not save the settings: {error}")
        };
        self.report_error(Area::App, None, message.clone());
        self.sidebar.notice = Some(message);
        cx.notify();
    }

    /// A save landed: the file says what the page says, so a "still ON" notice is no longer true.
    fn settings_reached_the_file(&mut self, cx: &mut Context<Self>) {
        if !std::mem::take(&mut self.settings_ui.mcp_off_unsaved) {
            return;
        }
        if self
            .sidebar
            .notice
            .as_deref()
            .is_some_and(|notice| notice.contains(MCP_STILL_ON))
        {
            self.sidebar.notice = None;
            cx.notify();
        }
    }

    /// Quit-time flush: synchronous, so a change made in the last 300 ms
    /// survives.
    ///
    /// Returns what went wrong (an encoding error as well as the write's), so a caller that must
    /// know can say so; the quit-time caller has nowhere left to say it.
    fn flush_settings_blocking(&mut self) -> std::io::Result<()> {
        let Some(path) = self.settings_ui.path.clone() else {
            return Ok(());
        };
        let bytes = store::encode(&self.settings).map_err(std::io::Error::other)?;
        self.settings_ui.writer.write_now(&path, &bytes).map(drop)
    }
}

/// The part of a failed-save notice that says the MCP switch did not reach the file.
const MCP_STILL_ON: &str = "\"Let MCP clients run changes\" is still ON in the settings file";

/// The editor's tab setting.
pub(super) fn tab_size_of(
    editor: &crate::settings::EditorSettings,
) -> gpui_component::input::TabSize {
    gpui_component::input::TabSize {
        tab_size: editor.tab_size as usize,
        hard_tabs: false,
    }
}

/// Width of the row-number gutter: the number column, or just the marker
/// column for pending deletes.
pub(super) fn gutter_width(display: &GridDisplay) -> f32 {
    if display.show_row_numbers {
        crate::grid::ColumnLayout::DEFAULT_GUTTER
    } else {
        crate::grid::ColumnLayout::HIDDEN_GUTTER
    }
}
