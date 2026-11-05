//! Workspace tab bar: drag reorder, pinned tabs, Chrome-style groups, the
//! tab context menu and the "Open tabs" overflow picker.
//!
//! Ordering, grouping and close-scope rules live in `WorkspaceState`
//! (`crate::workspace`); this module only renders that model and routes input.
//! Everything is event driven: drag state is gpui's own active drag (no
//! timers or polling), the layout is cached in the model and rebuilt only
//! when tabs change, and no per-tab entities exist besides the (single)
//! inline-rename input and the picker search input.
//!
//! Two strip modes share one row renderer (`render_strip_row`):
//! - Inline (default): one row, group chips and their tabs interleaved;
//!   a chip click collapses / expands its group.
//! - Grouped rows (the icon button at the trailing end of the tabs row, next
//!   to the "Open tabs" button, or the palette; persisted as the `tab_rows`
//!   preference, all panes): per pane, the top row holds only chips (one per
//!   group present, plus "Other" for connection-less query tabs, last), the
//!   tabs row the pane's pinned and global tabs followed by the shown group's
//!   tabs. Global tabs are those with no connection by nature (Welcome,
//!   Driver Manager); like pinned tabs they are never inside a chip, so they
//!   stay visible whichever group is shown. A pane with one group (or none)
//!   draws a single row. A chip click shows the group's last active tab;
//!   collapse does not apply. Dropping a tab on a chip moves it to that
//!   group's start when it belongs to the group (cross-pane moves) and is
//!   refused otherwise, like inline; "Other" is not a drop target (use the
//!   row's empty tail). Turning rows on while ungrouped groups by connection.
//!   Chips show the connection's state dot (sidebar colors) and a context
//!   menu: New Query, Close All Tabs in Group, Disconnect. Next / Previous
//!   Tab Group (`secondary-shift-pagedown` / `-pageup`, also
//!   `secondary-shift-]` / `[`) and the palette cycle the chips in both modes.

use super::*;
use crate::panes::{Edge, PaneId, StripSettings};
use crate::panes_view::DropHover;
use crate::sidebar_preferences::{SidebarPreferences, TabGroupBy};
use crate::workspace::{
    RowChip, TabBarItem, TabCloseScope, TabDropTarget, TabGroupKey, TabMovePlan,
};
use gpui::{Anchor, ClipboardItem, Rgba, SharedString};
use gpui_component::menu::PopupMenu;
use gpui_component::{ActiveTheme as _, IconNamed};

/// Compact pinned tab width (icon + short title + pin glyph, no close button).
pub(super) const PINNED_TAB_WIDTH: f32 = 120.0;
/// The overflow picker is shown whenever the strip overflows, or as soon as
/// this many tabs are open (so it is discoverable before the strip scrolls).
pub(super) const OPEN_TABS_ALWAYS_THRESHOLD: usize = 8;
const PICKER_ROW_HEIGHT: f32 = 28.0;
const PICKER_MAX_HEIGHT: f32 = 448.0;
const PICKER_WIDTH: f32 = 288.0;

/// gpui drag payload of a workspace tab.
#[derive(Clone)]
pub(super) struct TabDrag {
    pub(super) id: u64,
}

/// Drag ghost: a small tab preview.
struct TabDragGhost {
    title: SharedString,
    icon: TabIcon,
}

impl Render for TabDragGhost {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::accent())
            .text_size(px(12.0))
            .text_color(theme::text())
            .shadow_md()
            .child(self.icon.element(13.0, theme::text_muted()))
            .child(self.title.clone())
    }
}

/// Icons that are not part of the component `IconName` catalog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabGlyph {
    Pin,
    Minimize,
    Maximize,
    Crosshair,
    Pencil,
    SplitRight,
    SplitDown,
    Rows,
}

impl IconNamed for TabGlyph {
    fn path(self) -> SharedString {
        match self {
            Self::Pin => "icons/pin.svg",
            Self::Minimize => "icons/minimize-2.svg",
            Self::Maximize => "icons/maximize-2.svg",
            Self::Crosshair => "icons/crosshair.svg",
            Self::Pencil => "icons/pencil.svg",
            Self::SplitRight => "icons/panel-right.svg",
            Self::SplitDown => "icons/panel-bottom.svg",
            Self::Rows => "icons/rows-3.svg",
        }
        .into()
    }
}

/// Inline tab rename in progress (at most one at a time).
pub(super) struct TabRename {
    pub(super) tab_id: u64,
    input: gpui::Entity<InputState>,
    _subscription: gpui::Subscription,
}

/// "Open tabs" overflow picker state.
pub(super) struct OpenTabsPicker {
    pub(super) open: bool,
    /// The pane whose tabs the picker lists (`None` = the focused pane).
    pub(super) pane: Option<PaneId>,
    input: Option<gpui::Entity<InputState>>,
    pub(super) query: String,
    /// Keyboard highlight within the filtered list.
    pub(super) selected: usize,
    list_scroll: UniformListScrollHandle,
}

impl Default for OpenTabsPicker {
    fn default() -> Self {
        Self {
            open: false,
            pane: None,
            input: None,
            query: String::new(),
            selected: 0,
            list_scroll: UniformListScrollHandle::new(),
        }
    }
}

impl OpenTabsPicker {
    #[cfg(test)]
    pub(super) fn input_focus_handle(&self, cx: &gpui::App) -> Option<gpui::FocusHandle> {
        self.input.as_ref().map(|input| input.focus_handle(cx))
    }
}

#[derive(Default)]
pub(super) struct TabBarUi {
    /// Latest drop candidate under the pointer while a tab is dragged.
    /// Cleared when the drag ghost is released (drop, escape, or drop outside).
    pub(super) hover: Option<(TabDropTarget, TabMovePlan)>,
    /// Pane content area under a dragged tab: the split / move zone it would
    /// drop into. Cleared with `hover`.
    pub(super) pane_drop: Option<DropHover>,
    /// Bounds of that pane content area (for the overlay), from the drag event.
    pub(super) pane_drop_bounds: gpui::Bounds<gpui::Pixels>,
    pub(super) compact_titles: bool,
    /// Per-pane strip scroll state, created on first render of the pane.
    strips: std::collections::HashMap<PaneId, StripUi>,
    /// True from drag start until gpui releases the drag ghost.
    pub(super) dragging: bool,
    /// Shaped text widths (12px UI font) by text, so tab widths are measured
    /// once per label change instead of every frame.
    text_widths: std::collections::HashMap<String, f32>,
    escape_watch: Option<gpui::Subscription>,
    pub(super) picker: OpenTabsPicker,
    pub(super) rename: Option<TabRename>,
}

/// Scroll state of one pane's tab strip.
#[derive(Default)]
pub(super) struct StripUi {
    /// The tabs row (the only row in inline mode).
    pub(super) scroll: ScrollHandle,
    /// The chips row of the grouped-rows strip.
    chips_scroll: ScrollHandle,
    /// The active tab last scrolled into view, and the grouping / rows mode
    /// at that time (changing any re-reveals the active tab).
    last_active: Option<u64>,
    last_group_by: TabGroupBy,
    last_rows: bool,
}

impl TabBarUi {
    /// Makes every strip scroll its active tab into view on the next frame.
    pub(super) fn reveal_active_tabs(&mut self) {
        for strip in self.strips.values_mut() {
            strip.last_active = None;
        }
    }

    /// Ends the drag-over state (drop, escape, or drop outside).
    pub(super) fn clear_drag_hover(&mut self) {
        self.hover = None;
        self.pane_drop = None;
    }

    pub(super) fn strip(&mut self, pane: PaneId) -> &mut StripUi {
        self.strips.entry(pane).or_default()
    }
}

/// Actions offered by the tab context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TabCommand {
    ToggleCompact,
    Rename,
    Duplicate,
    CopyName,
    Locate,
    TogglePin,
    GroupBy(TabGroupBy),
    Close(TabCloseScope),
    CloseTab,
    SplitRight,
    SplitDown,
    MoveToOtherPane,
    ClosePane,
}

/// Snapshot of what the context menu of a tab offers; also the unit the
/// visibility/label tests assert on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct TabMenuState {
    pub(super) pinned: bool,
    pub(super) can_rename: bool,
    pub(super) can_duplicate: bool,
    pub(super) can_locate: bool,
    pub(super) in_group: bool,
    pub(super) compact: bool,
    pub(super) group_by: TabGroupBy,
    pub(super) close_label: &'static str,
    pub(super) others_label: &'static str,
    pub(super) all_label: &'static str,
    pub(super) others_disabled: bool,
    pub(super) left_disabled: bool,
    pub(super) right_disabled: bool,
    /// The tab's pane holds another tab, so a split would not leave it empty.
    pub(super) can_split: bool,
    /// More than one pane is open: Move to Other Pane / Close Pane apply.
    pub(super) multi_pane: bool,
}

/// Label split into the object name and its muted `@database[.schema]`
/// suffix. Full titles carry the suffix for table/collection tabs; shortened
/// titles and every other tab have none.
pub(super) fn tab_label_parts(tab: &Tab, compact: bool) -> (std::borrow::Cow<'_, str>, Option<String>) {
    use std::borrow::Cow;
    // A file's tab is named after the file; a dot after the name says the file is not saved.
    if tab.has_unsaved_file() {
        return (Cow::Owned(format!("{} \u{2022}", tab.title)), None);
    }
    let title = Cow::Borrowed(tab.title.as_str());
    // A Redis key tab is already named `key@dbN`.
    if compact
        || tab.kind != TabKind::Query
        || tab.collection.is_empty()
        || tab.database.is_empty()
        || tab.query_mode.is_redis()
    {
        return (title, None);
    }
    let suffix = match tab.schema.as_deref() {
        Some(schema) if !schema.is_empty() && schema != tab.database => {
            format!("@{}.{}", tab.database, schema)
        }
        _ => format!("@{}", tab.database),
    };
    (title, Some(suffix))
}

/// Label shown for a tab (name plus suffix); also what Copy Name copies.
pub(super) fn tab_label(tab: &Tab, compact: bool) -> String {
    match tab_label_parts(tab, compact) {
        (name, Some(suffix)) => format!("{name}{suffix}"),
        (name, None) => name.to_string(),
    }
}

/// Regular tabs fit their content within these bounds.
pub(super) const TAB_MIN_WIDTH: f32 = 100.0;
pub(super) const TAB_MAX_WIDTH: f32 = 240.0;
const PINNED_MIN_WIDTH: f32 = 72.0;
/// Smallest suffix worth drawing ("@" + a few characters + ellipsis).
const MIN_SUFFIX_WIDTH: f32 = 28.0;

/// Measured width of the compact ghost close button (32px square).
const CLOSE_BUTTON_WIDTH: f32 = 32.0;

/// Horizontal space a tab spends on everything except its text: padding,
/// icon, gaps and the close button / pin glyph slot.
fn tab_chrome_width(pinned: bool) -> f32 {
    if pinned {
        8.0 * 2.0 + 13.0 + 7.0 + 7.0 + 11.0
    } else {
        10.0 * 2.0 + 13.0 + 7.0 + 7.0 + CLOSE_BUTTON_WIDTH
    }
}

/// Result of fitting a tab's text into its width.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct TabTextFit {
    pub(super) width: f32,
    pub(super) name_width: f32,
    /// `None` when the suffix is dropped entirely.
    pub(super) suffix_width: Option<f32>,
    pub(super) truncated: bool,
}

/// Fits measured text widths into a tab: width follows content within
/// [`TAB_MIN_WIDTH`, `TAB_MAX_WIDTH`] (pinned: up to `PINNED_TAB_WIDTH`).
/// When the content is too wide the suffix gives way first; once it is gone
/// the name is middle-ellipsized.
pub(super) fn fit_tab_text(name_w: f32, suffix_w: Option<f32>, pinned: bool) -> TabTextFit {
    // Slack covers sub-pixel shaping differences so exact fits never ellipsize.
    const SLACK: f32 = 2.0;
    let chrome = tab_chrome_width(pinned);
    let (min, max) = if pinned {
        (PINNED_MIN_WIDTH, PINNED_TAB_WIDTH)
    } else {
        (TAB_MIN_WIDTH, TAB_MAX_WIDTH)
    };
    let content = name_w + suffix_w.unwrap_or(0.0) + SLACK;
    let width = (chrome + content).clamp(min, max);
    let avail = width - chrome;
    if content <= avail {
        return TabTextFit {
            width,
            name_width: name_w + SLACK,
            suffix_width: suffix_w,
            truncated: false,
        };
    }
    match suffix_w {
        Some(_) if avail - name_w - SLACK >= MIN_SUFFIX_WIDTH => TabTextFit {
            width,
            name_width: name_w + SLACK,
            suffix_width: Some(avail - name_w - SLACK),
            truncated: true,
        },
        _ => TabTextFit {
            width,
            name_width: avail,
            suffix_width: None,
            truncated: true,
        },
    }
}

/// A tab's leading icon, matching the sidebar tree: connection tabs wear the
/// connection's brand art, table / collection / pipeline tabs the same
/// colored glyph as their tree rows, the rest a plain themed glyph.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TabIcon {
    /// Full-color brand art (`img`), like the tree's connection rows.
    Brand(&'static str),
    /// Glyph tinted like its tree row.
    Colored(&'static str, Rgba),
    /// Glyph in the tab's text color.
    Plain(&'static str),
}

impl TabIcon {
    pub(super) fn element(self, size: f32, plain: Rgba) -> AnyElement {
        match self {
            Self::Brand(asset) => gpui::img(asset).w(px(size)).h(px(size)).into_any_element(),
            Self::Colored(path, color) => Icon::default()
                .path(path)
                .size(px(size))
                .text_color(color)
                .into_any_element(),
            Self::Plain(path) => Icon::default()
                .path(path)
                .size(px(size))
                .text_color(plain)
                .into_any_element(),
        }
    }
}

impl AppRoot {
    pub(super) fn tab_icon(&self, tab: &Tab) -> TabIcon {
        match tab.kind {
            TabKind::Welcome => TabIcon::Plain("icons/layout-dashboard.svg"),
            TabKind::DriverManager => TabIcon::Plain("icons/settings-2.svg"),
            TabKind::Settings => TabIcon::Plain("icons/settings.svg"),
            TabKind::Diagram => TabIcon::Plain("icons/network.svg"),
            TabKind::Import => TabIcon::Plain("icons/folder-input.svg"),
            TabKind::Transfer => TabIcon::Plain("icons/arrow-left-right.svg"),
            TabKind::SqlFile => TabIcon::Plain("icons/play.svg"),
            TabKind::Backup => TabIcon::Plain("icons/download.svg"),
            TabKind::Restore => TabIcon::Plain("icons/upload.svg"),
            TabKind::Accounts => TabIcon::Plain("icons/key-round.svg"),
            // The sidebar's glyph of the kind of object it shows.
            TabKind::Source => TabIcon::Plain(
                match tab.source.as_ref().map(|source| source.kind) {
                    Some(engine_driver_api::sql::SchemaObjectKind::Procedure) => "icons/square-terminal.svg",
                    Some(engine_driver_api::sql::SchemaObjectKind::Sequence) => "icons/hash.svg",
                    Some(engine_driver_api::sql::SchemaObjectKind::Trigger) => "icons/crosshair.svg",
                    Some(engine_driver_api::sql::SchemaObjectKind::Type) => "icons/type.svg",
                    Some(engine_driver_api::sql::SchemaObjectKind::Event) => "icons/calendar-clock.svg",
                    Some(engine_driver_api::sql::SchemaObjectKind::MaterializedView) => "icons/eye.svg",
                    _ => "icons/braces.svg",
                },
            ),
            TabKind::Connection => self
                .saved_connections
                .iter()
                .find(|saved| saved.matches_id(&tab.connection_id))
                .and_then(|saved| super::navigator_ui::connection_brand_asset(saved.kind))
                .map_or(TabIcon::Plain("icons/building-2.svg"), TabIcon::Brand),
            TabKind::Query => match tab.query_mode.as_ref() {
                // Tree colors: tables green, Mongo collections lighter green,
                // saved pipelines sky.
                Some(mode) if mode.is_sql_table() => {
                    TabIcon::Colored("icons/table.svg", theme::icon_table())
                }
                Some(QueryMode::MongoFind) => {
                    TabIcon::Colored("icons/table.svg", theme::icon_collection())
                }
                Some(QueryMode::MongoAggregate) => {
                    TabIcon::Colored("icons/list-ordered.svg", theme::icon_schema())
                }
                Some(QueryMode::RedisKey) => TabIcon::Colored(
                    "icons/key-round.svg",
                    super::redis::type_color(
                        self.workspace
                            .runtime(tab.id)
                            .and_then(|runtime| runtime.redis.info.as_ref())
                            .and_then(|info| info.key_type()),
                    ),
                ),
                _ => TabIcon::Plain("icons/square-terminal.svg"),
            },
        }
    }
}

fn tab_width(tab: &Tab, compact: bool) -> f32 {
    if tab.is_pinned {
        return PINNED_TAB_WIDTH;
    }
    let full = match tab.kind {
        TabKind::Welcome => 140.0,
        TabKind::Query => 180.0,
        TabKind::Connection => 220.0,
        TabKind::DriverManager => 180.0,
        TabKind::Settings => 140.0,
        TabKind::Diagram => 200.0,
        TabKind::Source => 220.0,
        TabKind::Import | TabKind::Transfer | TabKind::SqlFile | TabKind::Backup | TabKind::Restore | TabKind::Accounts => 220.0,
    };
    if compact {
        (full - 40.0f32).max(120.0)
    } else {
        full
    }
}

/// Query tabs that hold user-authored text can be renamed; table/collection
/// tabs are named after their object.
pub(super) fn tab_can_rename(tab: &Tab) -> bool {
    tab.kind == TabKind::Query
        && !tab.query_mode.is_table_data()
        && !tab.query_mode.is_mongo_find()
        && !tab.query_mode.is_redis_key()
        // An Aggregate tab is named by its saved pipeline (Save / Save As).
        && !tab.query_mode.is_mongo_aggregate()
}

fn indicator(left: bool) -> gpui::Div {
    let bar = div()
        .debug_selector(|| "tab-drop-indicator".into())
        .absolute()
        .top(px(4.0))
        .bottom(px(4.0))
        .w(px(2.0))
        .bg(theme::accent());
    if left {
        bar.left_0()
    } else {
        bar.right_0()
    }
}

/// Element-id / selector names of one strip row.
#[derive(Clone, Copy)]
struct RowNames {
    strip: &'static str,
    tail: &'static str,
    bar: &'static str,
}

/// The tabs (or the only) row; names are what tests and scenarios select.
const TAB_ROW: RowNames = RowNames {
    strip: "workspace-tabs",
    tail: "workspace-tab-tail",
    bar: "workspace-tab-bar",
};
/// The group-chip row of the grouped-rows strip.
const CHIP_ROW: RowNames = RowNames {
    strip: "workspace-groups",
    tail: "workspace-group-tail",
    bar: "workspace-group-bar",
};

/// Everything `render_strip_row` needs for one row.
struct StripRow {
    names: RowNames,
    height: f32,
    items: Vec<AnyElement>,
    scroll: ScrollHandle,
    /// Carries the "Open tabs" picker button.
    is_tab_row: bool,
    focus_line: bool,
}

/// How a chip behaves: toggles its group inline, selects it in the rows strip.
#[derive(Clone, Copy)]
enum ChipState {
    Inline { collapsed: bool },
    Selector { active: bool },
}

/// Selector of a strip element; the first pane keeps the plain names.
fn pane_selector(base: &str, ordinal: usize) -> SharedString {
    if ordinal == 0 {
        base.to_string().into()
    } else {
        format!("{base}-p{ordinal}").into()
    }
}

impl AppRoot {
    // ---- preferences ----------------------------------------------------

    /// Applies persisted tab-bar preferences (group-by, shortened titles).
    pub(super) fn apply_tab_bar_preferences(&mut self, cx: &mut Context<Self>) {
        let prefs = self.navigator.preferences();
        let compact = prefs.compact_tab_titles;
        // The saved choice is the default of every pane (sessions written
        // before per-pane settings, new workspaces); panes then change alone.
        let defaults = StripSettings {
            group_by: prefs.tab_group_by,
            rows: prefs.tab_rows,
        };
        self.workspace.set_strip_defaults(defaults);
        self.shell.tab_bar.compact_titles = compact;
        // Escape cancels a tab drag / inline rename regardless of focus (an
        // `on_action` handler would only run with focus inside the view).
        // Event driven: fires per keystroke, never polls.
        self.shell.tab_bar.escape_watch = Some(cx.observe_keystrokes(|this, event, window, cx| {
            if event.keystroke.key == "escape"
                && event.keystroke.modifiers.number_of_modifiers() == 0
            {
                this.cancel_tab_interaction(window, cx);
            }
        }));
    }

    fn persist_tab_preference(
        &mut self,
        change: impl FnOnce(&mut SidebarPreferences),
        cx: &mut Context<Self>,
    ) {
        if self.sidebar.path.is_none() {
            return;
        }
        let _ = self.change_sidebar_preferences(
            |prefs| {
                change(prefs);
                Ok(())
            },
            cx,
        );
    }

    /// "Group Tabs By" of the focused pane.
    #[cfg(test)]
    pub(super) fn set_tab_group_by(&mut self, mode: TabGroupBy, cx: &mut Context<Self>) {
        self.set_pane_group_by(self.workspace.focused_pane(), mode, cx);
    }

    /// "Group Tabs By" of one pane; the other panes keep their own. The
    /// choice is also saved as the default for new workspaces and old sessions.
    pub(super) fn set_pane_group_by(
        &mut self,
        pane: PaneId,
        mode: TabGroupBy,
        cx: &mut Context<Self>,
    ) {
        self.workspace.set_pane_group_by(pane, mode);
        self.persist_tab_preference(|prefs| prefs.set_tab_group_by(mode), cx);
        cx.notify();
    }

    /// Two-row strip of the focused pane (see `set_pane_tab_rows`).
    #[cfg(test)]
    pub(super) fn set_tab_rows(&mut self, rows: bool, cx: &mut Context<Self>) {
        self.set_pane_tab_rows(self.workspace.focused_pane(), rows, cx);
    }

    /// Switches one pane between the one-row strip and the grouped-rows strip
    /// (group chips on top, the shown group's tabs below). The rows need
    /// groups, so turning them on while the pane's tabs are ungrouped also
    /// groups them by connection.
    pub(super) fn set_pane_tab_rows(&mut self, pane: PaneId, rows: bool, cx: &mut Context<Self>) {
        if rows && self.workspace.group_by_of(pane) == TabGroupBy::None {
            self.workspace.set_pane_group_by(pane, TabGroupBy::Connection);
        }
        self.workspace.set_pane_tab_rows(pane, rows);
        self.shell.tab_bar.reveal_active_tabs();
        let group_by = self.workspace.group_by_of(pane);
        self.persist_tab_preference(
            |prefs| {
                prefs.set_tab_group_by(group_by);
                prefs.set_tab_rows(rows);
            },
            cx,
        );
        cx.notify();
    }

    /// Palette: toggles the focused pane's two-row strip.
    pub(super) fn toggle_tab_rows(&mut self, cx: &mut Context<Self>) {
        self.toggle_pane_tab_rows(self.workspace.focused_pane(), cx);
    }

    pub(super) fn toggle_pane_tab_rows(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        let rows = !self.workspace.tab_rows_of(pane);
        self.set_pane_tab_rows(pane, rows, cx);
    }

    /// Height of one pane's strip above its content.
    pub(super) fn tab_strip_height(&self, pane: PaneId) -> f32 {
        if self.workspace.pane_chip_row(pane) {
            theme::GROUP_ROW_HEIGHT + theme::TAB_HEIGHT
        } else {
            theme::TAB_HEIGHT
        }
    }

    /// Shows the next (`delta` 1) or previous (-1) tab group of the focused
    /// pane: its last active tab. Works in both strip modes.
    pub(super) fn step_tab_group(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.workspace.normalize();
        let pane = self.workspace.focused_pane();
        if let Some(tab_id) = self.workspace.adjacent_group_target(pane, delta) {
            self.activate_tab_ui(tab_id, cx);
        }
    }

    pub(super) fn set_compact_tab_titles(&mut self, compact: bool, cx: &mut Context<Self>) {
        self.shell.tab_bar.compact_titles = compact;
        self.persist_tab_preference(|prefs| prefs.set_compact_tab_titles(compact), cx);
        cx.notify();
    }

    // ---- labels & colors ------------------------------------------------

    pub(super) fn connection_name(&self, connection_id: &str) -> Option<&str> {
        self.saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection_id))
            .map(|saved| saved.name.as_str())
    }

    /// Chip label: connection name, or `connection · database` (tooltip and
    /// tests; the chip draws the two parts separately).
    pub(super) fn group_label(&self, key: &TabGroupKey) -> String {
        let (name, database) = self.group_label_parts(key);
        match database {
            Some(database) => format!("{name} · {database}"),
            None => name.to_string(),
        }
    }

    /// The connection name and, when the group is per database, the database.
    fn group_label_parts<'a>(&'a self, key: &'a TabGroupKey) -> (&'a str, Option<&'a str>) {
        let name = self.connection_name(&key.connection_id).unwrap_or("Connection");
        (name, (!key.database.is_empty()).then_some(key.database.as_str()))
    }

    /// The connection's color (sidebar override, then stored color), or the
    /// accent when it has none.
    pub(super) fn group_color(&self, key: &TabGroupKey) -> Rgba {
        self.sidebar
            .color(&key.connection_id)
            .and_then(super::navigator_ui::parse_hex_color)
            .unwrap_or_else(theme::accent)
    }

    // ---- activation -----------------------------------------------------

    /// Activates a tab from any bar surface (click, picker) and keeps it in view.
    pub(super) fn activate_tab_ui(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if self.workspace.activate_tab(tab_id) {
            self.ensure_auto_refresh_task(tab_id, cx);
            self.run_deferred_reconnect_rerun(tab_id, cx);
        }
        cx.notify();
    }

    /// Chip click in the grouped-rows strip: shows the group's last active tab.
    fn activate_group_ui(
        &mut self,
        pane: PaneId,
        group: Option<&TabGroupKey>,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab_id) = self.workspace.group_target(pane, group) {
            self.activate_tab_ui(tab_id, cx);
        }
    }

    // ---- drag & drop ------------------------------------------------------

    fn set_drag_hover(&mut self, target: TabDropTarget, dragged: u64, cx: &mut Context<Self>) {
        let plan = self.workspace.plan_move(dragged, &target);
        let unchanged = self
            .shell
            .tab_bar
            .hover
            .as_ref()
            .is_some_and(|(current, current_plan)| *current == target && *current_plan == plan);
        if !unchanged {
            self.shell.tab_bar.hover = Some((target, plan));
            self.shell.tab_bar.pane_drop = None;
            self.notify_pane_views(cx);
        }
    }

    /// Completes a drop on the tab `over`, honoring the half the pointer was
    /// last over (recorded by the drag-move listener).
    fn drop_on_tab(&mut self, dragged: u64, over: u64, cx: &mut Context<Self>) {
        let target = match self.shell.tab_bar.hover.as_ref() {
            Some((target @ (TabDropTarget::Before(anchor) | TabDropTarget::After(anchor)), _))
                if *anchor == over =>
            {
                target.clone()
            }
            _ => TabDropTarget::Before(over),
        };
        self.finish_tab_drop(dragged, target, cx);
    }

    pub(super) fn finish_tab_drop(
        &mut self,
        dragged: u64,
        target: TabDropTarget,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.move_tab(dragged, &target) {
            self.shell.tab_bar.reveal_active_tabs();
            self.after_focus_change(cx);
        }
        self.shell.tab_bar.clear_drag_hover();
        cx.notify();
    }

    /// Completes a drop on a pane's content area: into its tab list or into a
    /// new pane beside it, per the zone recorded by the drag-move listener.
    pub(super) fn drop_on_pane(&mut self, dragged: u64, cx: &mut Context<Self>) {
        let hover = self.shell.tab_bar.pane_drop.take();
        self.shell.tab_bar.clear_drag_hover();
        if let Some(hover) = hover {
            if self.workspace.drop_tab(dragged, hover.to_drop()) {
                self.shell.tab_bar.reveal_active_tabs();
                self.after_focus_change(cx);
            }
        }
        cx.notify();
    }

    /// Focuses a pane (click inside it, palette); its active tab becomes the
    /// workspace's active tab.
    pub(super) fn focus_pane_ui(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.focus_pane(pane) {
            self.after_focus_change(cx);
            cx.notify();
        }
    }

    /// Per-activation work for the tab that is active now, when the focus
    /// moved to another pane or another tab (same as `activate_tab_ui`).
    pub(super) fn after_focus_change(&mut self, cx: &mut Context<Self>) {
        if let Some(tab_id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.ensure_auto_refresh_task(tab_id, cx);
            self.run_deferred_reconnect_rerun(tab_id, cx);
        }
    }

    /// Escape: cancels a tab drag first, then an inline rename.
    fn cancel_tab_interaction(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shell.tab_bar.dragging {
            cx.stop_active_drag(window);
            self.shell.tab_bar.dragging = false;
            self.shell.tab_bar.clear_drag_hover();
            cx.notify();
        } else if self.shell.tab_bar.rename.take().is_some() {
            cx.notify();
        }
    }

    // ---- close flows ------------------------------------------------------

    /// Staged row changes, or structure changes of the Properties edit mode that were not applied.
    fn tab_has_pending_changes(&self, tab_id: u64, cx: &gpui::App) -> bool {
        self.workspace.tab_has_staged_changes(tab_id) || self.view(tab_id).mongo.document_edit.is_some() || self.structure_draft_count(tab_id, cx) > 0 || self.import_running(tab_id, cx) || self.transfer_running(tab_id, cx) || self.file_run_running(tab_id, cx) || self.backup_running(tab_id, cx) || self.accounts_dirty(tab_id, cx)
    }

    /// Closes several tabs. Tabs with staged changes are confirmed once,
    /// listing them; cancelling closes nothing. When the active tab goes away
    /// the clicked `anchor` tab (if it survives) becomes active.
    pub(super) fn close_tabs_confirmed(
        &mut self,
        ids: Vec<u64>,
        anchor: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if ids.is_empty() {
            return;
        }
        // Open transactions are asked about first; the rest of the questions follow once they ended.
        let asked = {
            let ids = ids.clone();
            self.guard_transactions(
                ids.clone(),
                "Closing the tabs ends their sessions.",
                window,
                cx,
                move |this, window, cx| this.close_tabs_confirmed(ids, anchor, window, cx),
            )
        };
        if asked {
            return;
        }
        let dirty_titles: Vec<String> = ids
            .iter()
            .filter(|id| self.tab_has_pending_changes(**id, cx))
            .filter_map(|id| self.workspace.tab(*id).map(|tab| tab.title.clone()))
            .collect();
        let unsaved_files: Vec<String> = ids
            .iter()
            .filter_map(|id| self.workspace.tab(*id).filter(|tab| tab.has_unsaved_file()))
            .map(|tab| tab.title.clone())
            .collect();
        if dirty_titles.is_empty() && unsaved_files.is_empty() {
            self.close_tabs_now(&ids, anchor, cx);
            return;
        }
        let structure = ids.iter().any(|id| self.structure_draft_count(*id, cx) > 0);
        let mut body = if dirty_titles.is_empty() {
            String::new()
        } else if structure {
            format!("These tabs have changes that have not been committed or applied: {}.", dirty_titles.join(", "))
        } else {
            format!("These tabs have staged changes that have not been committed: {}.", dirty_titles.join(", "))
        };
        if !unsaved_files.is_empty() {
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(&format!("These files have changes that have not been saved: {}.", unsaved_files.join(", ")));
        }
        self.open_discard_dialog(
            "Discard pending changes?".to_owned(),
            body,
            move |this, _, cx| this.close_tabs_now(&ids, anchor, cx),
            window,
            cx,
        );
    }

    pub(super) fn close_tabs_now(&mut self, ids: &[u64], anchor: Option<u64>, cx: &mut Context<Self>) {
        let active_before = self.workspace.active_tab().map(|tab| tab.id);
        for id in ids {
            self.close_tab(*id, cx);
        }
        if let (Some(active), Some(anchor)) = (active_before, anchor) {
            if ids.contains(&active) && self.workspace.tab(anchor).is_some() {
                self.workspace.activate_tab(anchor);
            }
        }
        if let Some(active) = self.workspace.active_tab().map(|tab| tab.id) {
            self.ensure_auto_refresh_task(active, cx);
        }
        cx.notify();
    }

    /// Cmd+W: closes the active tab (pinned included, like the menu entry).
    pub(super) fn close_active_tab_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.workspace.active_tab().map(|tab| tab.id) {
            self.confirm_close_tab(id, window, cx);
        }
    }

    // ---- rename -----------------------------------------------------------

    fn begin_tab_rename(&mut self, tab_id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.workspace.tab(tab_id).filter(|tab| tab_can_rename(tab)) else {
            return;
        };
        let title = tab.title.clone();
        let input = cx.new(|cx| InputState::new(window, cx).default_value(title));
        let subscription = cx.subscribe(
            &input,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                if matches!(
                    event,
                    gpui_component::input::InputEvent::PressEnter { .. }
                        | gpui_component::input::InputEvent::Blur
                ) {
                    this.commit_tab_rename(cx);
                }
            },
        );
        input.update(cx, |state, cx| state.focus(window, cx));
        self.shell.tab_bar.rename = Some(TabRename {
            tab_id,
            input,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(super) fn commit_tab_rename(&mut self, cx: &mut Context<Self>) {
        let Some(rename) = self.shell.tab_bar.rename.take() else {
            return;
        };
        let text = rename.input.read(cx).value().to_string();
        self.workspace.rename_tab(rename.tab_id, &text);
        cx.notify();
    }

    // ---- context menu -----------------------------------------------------

    pub(super) fn tab_menu_state(&self, tab_id: u64) -> Option<TabMenuState> {
        let tab = self.workspace.tab(tab_id)?;
        let has_pinned = self.workspace.tabs().iter().any(|other| other.is_pinned);
        let pinned = tab.is_pinned;
        let disabled = |scope| self.workspace.close_targets(tab_id, scope).is_empty();
        Some(TabMenuState {
            pinned,
            can_rename: tab_can_rename(tab),
            can_duplicate: tab.kind == TabKind::Query,
            can_locate: matches!(tab.kind, TabKind::Query | TabKind::Connection)
                && self
                    .saved_connections
                    .iter()
                    .any(|saved| saved.matches_id(&tab.connection_id)),
            in_group: self.workspace.is_grouped(tab),
            compact: self.shell.tab_bar.compact_titles,
            group_by: self
                .workspace
                .pane_of(tab_id)
                .map_or_else(|| self.workspace.group_by(), |pane| self.workspace.group_by_of(pane)),
            close_label: if pinned {
                "Close Pinned Tab"
            } else {
                "Close Tab"
            },
            others_label: if pinned {
                "Close Other Pinned Tabs"
            } else if has_pinned {
                "Close Other Regular Tabs"
            } else {
                "Close Other Tabs"
            },
            all_label: if pinned {
                "Close All Pinned Tabs"
            } else if has_pinned {
                "Close All Regular Tabs"
            } else {
                "Close All Tabs"
            },
            others_disabled: disabled(TabCloseScope::Others),
            left_disabled: disabled(TabCloseScope::Left),
            right_disabled: disabled(TabCloseScope::Right),
            can_split: self
                .workspace
                .pane_of(tab_id)
                .and_then(|pane| self.workspace.panes().pane(pane))
                .is_some_and(|pane| pane.tabs().len() > 1),
            multi_pane: self.workspace.panes().pane_count() > 1,
        })
    }

    pub(super) fn run_tab_command(
        &mut self,
        tab_id: u64,
        command: TabCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            TabCommand::ToggleCompact => {
                let compact = !self.shell.tab_bar.compact_titles;
                self.set_compact_tab_titles(compact, cx);
            }
            TabCommand::Rename => self.begin_tab_rename(tab_id, window, cx),
            TabCommand::Duplicate => {
                if let Some(new_id) = self.workspace.duplicate_tab(tab_id) {
                    let table_like = self.workspace.tab(new_id).is_some_and(|tab| {
                        tab.query_mode.is_table_data()
                            || tab.query_mode.is_mongo_find()
                    });
                    if table_like {
                        self.run_active_tab_query(cx);
                    }
                }
                cx.notify();
            }
            TabCommand::CopyName => {
                if let Some(tab) = self.workspace.tab(tab_id) {
                    let label = tab_label(tab, self.shell.tab_bar.compact_titles);
                    cx.write_to_clipboard(ClipboardItem::new_string(label));
                    window.push_notification("Copied", cx);
                }
            }
            TabCommand::Locate => {
                self.activate_tab_ui(tab_id, cx);
                self.locate_active_sidebar_tab(cx);
            }
            TabCommand::TogglePin => {
                let pinned = self.workspace.tab(tab_id).is_some_and(|tab| tab.is_pinned);
                self.workspace.set_pinned(tab_id, !pinned);
                // Pinning moves the tab to the start; keep the active tab in view.
                self.shell.tab_bar.reveal_active_tabs();
                cx.notify();
            }
            TabCommand::GroupBy(mode) => {
                if let Some(pane) = self.workspace.pane_of(tab_id) {
                    self.set_pane_group_by(pane, mode, cx);
                }
            }
            TabCommand::Close(scope) => {
                let ids = self.workspace.close_targets(tab_id, scope);
                self.close_tabs_confirmed(ids, Some(tab_id), window, cx);
            }
            TabCommand::CloseTab => self.confirm_close_tab(tab_id, window, cx),
            TabCommand::SplitRight => self.split_tab_ui(tab_id, Edge::Right, cx),
            TabCommand::SplitDown => self.split_tab_ui(tab_id, Edge::Bottom, cx),
            TabCommand::MoveToOtherPane => {
                if self.workspace.move_tab_to_other_pane(tab_id) {
                    self.shell.tab_bar.reveal_active_tabs();
                    self.after_focus_change(cx);
                }
                cx.notify();
            }
            TabCommand::ClosePane => {
                if let Some(pane) = self.workspace.pane_of(tab_id) {
                    if self.workspace.close_pane(pane) {
                        self.shell.tab_bar.reveal_active_tabs();
                        self.after_focus_change(cx);
                    }
                }
                cx.notify();
            }
        }
    }

    /// What a group chip's context menu offers.
    fn group_chip_menu_state(&self, group: Option<&TabGroupKey>) -> GroupChipMenuState {
        let node = group.map(|key| NavigatorNodeId::Connection {
            connection: key.connection_id.clone(),
        });
        GroupChipMenuState {
            has_connection: group.is_some(),
            can_query: node.as_ref().is_some_and(|id| {
                self.navigator.action_target(id).is_some()
                    && self
                        .navigator
                        .available_actions(id, false)
                        .contains(&crate::navigator::NavigatorAction::NewQuery)
            }),
            can_disconnect: group
                .is_some_and(|key| self.sessions.is_connected(&key.connection_id)),
        }
    }

    pub(super) fn run_group_command(
        &mut self,
        pane: PaneId,
        group: Option<&TabGroupKey>,
        command: GroupChipCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match command {
            GroupChipCommand::NewQuery => {
                if let Some(key) = group {
                    let id = NavigatorNodeId::Connection {
                        connection: key.connection_id.clone(),
                    };
                    self.new_query_for_sidebar_node(&id, cx);
                }
            }
            GroupChipCommand::CloseAll => {
                let ids = self.workspace.group_tab_ids(pane, group);
                self.close_tabs_confirmed(ids, None, window, cx);
            }
            GroupChipCommand::Disconnect => {
                if let Some(key) = group {
                    self.request_disconnect(&key.connection_id, window, cx);
                }
            }
        }
    }

    /// Moves the tab into a new pane beside its own (context menu, palette).
    pub(super) fn split_tab_ui(&mut self, tab_id: u64, edge: Edge, cx: &mut Context<Self>) {
        if self.workspace.split_tab(tab_id, edge) {
            self.shell.tab_bar.reveal_active_tabs();
            self.after_focus_change(cx);
        }
        cx.notify();
    }

    // ---- rendering --------------------------------------------------------

    /// Shapes the name/suffix of every tab whose text is not cached yet.
    /// Steady-state frames do a hash lookup per text and no shaping.
    pub(super) fn measure_tab_texts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = self.shell.tab_bar.compact_titles;
        if self.shell.tab_bar.text_widths.len() > 2048 {
            self.shell.tab_bar.text_widths.clear();
        }
        let cache = &self.shell.tab_bar.text_widths;
        let mut missing: Vec<String> = Vec::new();
        for tab in self.workspace.tabs() {
            let (name, suffix) = tab_label_parts(tab, compact);
            if !cache.contains_key(&*name) {
                missing.push(name.into_owned());
            }
            if let Some(suffix) = suffix {
                if !cache.contains_key(suffix.as_str()) {
                    missing.push(suffix);
                }
            }
        }
        if missing.is_empty() {
            return;
        }
        let font = gpui::font(cx.theme().font_family.clone());
        let system = window.text_system().clone();
        for text in missing {
            let run = gpui::TextRun {
                len: text.len(),
                font: font.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let width = f32::from(system.layout_line(&text, px(12.0), &[run], None).width);
            self.shell.tab_bar.text_widths.insert(text, width);
        }
    }

    /// The tab strip of one pane. Call `workspace.normalize()` and
    /// `measure_tab_texts` once per frame before rendering the strips.
    ///
    /// Inline mode is one row (group chips and tabs interleaved). With
    /// `workspace.tab_rows()` the same row renderer draws two: the chips on
    /// top, the tabs of the active group below.
    pub(super) fn render_tab_bar(
        &mut self,
        pane: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active_id = self.workspace.pane_active_tab(pane).map(|tab| tab.id);
        let focused = self.workspace.focused_pane() == pane;
        let multi_pane = self.workspace.panes().pane_count() > 1;
        // Selector suffix: the first pane keeps the plain names.
        let ordinal = self
            .workspace
            .panes()
            .panes()
            .position(|other| other.id() == pane)
            .unwrap_or(0);
        let rows = self.workspace.tab_rows_of(pane);
        let group_by = self.workspace.group_by_of(pane);
        // Keep the active tab in view when it changes, when regrouping moves
        // it or when the rows mode changes the strip's contents.
        let (scroll, chips_scroll, reveal) = {
            let ui = self.shell.tab_bar.strip(pane);
            let reveal = ui.last_active != active_id
                || ui.last_group_by != group_by
                || ui.last_rows != rows;
            ui.last_active = active_id;
            ui.last_group_by = group_by;
            ui.last_rows = rows;
            (ui.scroll.clone(), ui.chips_scroll.clone(), reveal)
        };
        let compact = self.shell.tab_bar.compact_titles;

        if rows {
            let view = self.workspace.pane_rows(pane);
            // Two rows with two or more groups, or beside a pane that has a
            // chip row (aligned heights); a lone group's chip leads the tabs
            // row instead; no group, no chip.
            let chip_row = self.workspace.pane_chip_row(pane);
            let lone_chip = !chip_row && view.chips.len() == 1;
            if reveal {
                if let Some(index) = view.active {
                    chips_scroll.scroll_to_item(index);
                }
                if let Some(index) = view.tabs.iter().position(|id| Some(*id) == active_id) {
                    // The lone chip and its separator sit after the global tabs.
                    let shift = if lone_chip && index >= view.always { 2 } else { 0 };
                    scroll.scroll_to_item(index + shift);
                }
            }
            let chip_at = |index: usize, chip: &RowChip<'_>, cx: &mut Context<Self>| {
                let state = ChipState::Selector {
                    active: view.active == Some(index),
                };
                self.render_group_chip(pane, *chip, state, index, ordinal, cx)
            };
            let chips = if chip_row {
                view.chips
                    .iter()
                    .enumerate()
                    .map(|(index, chip)| chip_at(index, chip, cx))
                    .collect()
            } else {
                Vec::new()
            };
            let color = view
                .active
                .and_then(|index| view.chips[index].group)
                .map(|key| self.group_color(key));
            let mut tabs: Vec<AnyElement> = Vec::with_capacity(view.tabs.len() + 2);
            for (position, tab) in view
                .tabs
                .iter()
                .filter_map(|id| self.workspace.tab(*id))
                .enumerate()
            {
                if lone_chip && position == view.always {
                    tabs.push(chip_at(0, &view.chips[0], cx));
                    tabs.push(
                        div()
                            .debug_selector(|| "tab-group-separator".into())
                            .w(px(1.0))
                            .h(px(16.0))
                            .mx(px(4.0))
                            .flex_shrink_0()
                            .bg(theme::border())
                            .into_any_element(),
                    );
                }
                let color = color.filter(|_| self.workspace.is_grouped(tab));
                tabs.push(self.render_tab(tab, color, active_id == Some(tab.id), compact, cx));
            }
            let tab_row = self.render_strip_row(
                pane,
                ordinal,
                StripRow {
                    names: TAB_ROW,
                    height: theme::TAB_HEIGHT,
                    items: tabs,
                    scroll,
                    is_tab_row: true,
                    focus_line: multi_pane && focused && !chip_row,
                },
                window,
                cx,
            );
            if !chip_row {
                return tab_row;
            }
            let chip_row_element = self.render_strip_row(
                pane,
                ordinal,
                StripRow {
                    names: CHIP_ROW,
                    height: theme::GROUP_ROW_HEIGHT,
                    items: chips,
                    scroll: chips_scroll,
                    is_tab_row: false,
                    focus_line: multi_pane && focused,
                },
                window,
                cx,
            );
            return div()
                .id(("workspace-tab-bars", pane.get()))
                .w_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .child(chip_row_element)
                .child(tab_row)
                .into_any_element();
        }

        if reveal {
            if let Some(index) = self
                .workspace
                .pane_layout(pane)
                .iter()
                .position(|item| matches!(item, TabBarItem::Tab(id) if Some(*id) == active_id))
            {
                scroll.scroll_to_item(index);
            }
        }
        let layout_len = self.workspace.pane_layout(pane).len();
        let mut items: Vec<AnyElement> = Vec::with_capacity(layout_len + 1);
        let mut group_color: Option<Rgba> = None;
        let mut chip_ordinal = 0usize;
        for item in self.workspace.pane_layout(pane) {
            match item {
                TabBarItem::Group {
                    key,
                    count,
                    collapsed,
                } => {
                    group_color = Some(self.group_color(key));
                    let chip = RowChip {
                        group: Some(key),
                        count: *count,
                    };
                    let state = ChipState::Inline {
                        collapsed: *collapsed,
                    };
                    items.push(self.render_group_chip(
                        pane,
                        chip,
                        state,
                        chip_ordinal,
                        ordinal,
                        cx,
                    ));
                    chip_ordinal += 1;
                }
                TabBarItem::Tab(id) => {
                    let Some(tab) = self.workspace.tab(*id) else {
                        continue;
                    };
                    let color = self
                        .workspace
                        .is_grouped(tab)
                        .then_some(group_color)
                        .flatten();
                    items.push(self.render_tab(tab, color, active_id == Some(*id), compact, cx));
                }
            }
        }
        self.render_strip_row(
            pane,
            ordinal,
            StripRow {
                names: TAB_ROW,
                height: theme::TAB_HEIGHT,
                items,
                scroll,
                is_tab_row: true,
                focus_line: multi_pane && focused,
            },
            window,
            cx,
        )
    }

    /// One scrolling row of the strip: `items`, then the empty tail (drop
    /// target, bar menu, double-click to maximize). The tabs row also carries
    /// the "Open tabs" picker button.
    fn render_strip_row(
        &mut self,
        pane: PaneId,
        ordinal: usize,
        row: StripRow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let StripRow {
            names,
            height,
            items,
            scroll,
            is_tab_row,
            focus_line,
        } = row;
        let tail_root = cx.entity().downgrade();
        let tail_selector = pane_selector(names.tail, ordinal);
        let tail = div()
            .id((names.tail, pane.get()))
            .debug_selector(move || tail_selector.to_string())
            .flex_1()
            .min_w(px(24.0))
            .h_full()
            .on_drag_move(
                cx.listener(move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx).id;
                    this.set_drag_hover(TabDropTarget::End(pane), dragged, cx);
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.finish_tab_drop(drag.id, TabDropTarget::End(pane), cx);
            }))
            // Double-click on the empty strip maximizes / restores the pane.
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                if event.click_count() >= 2 {
                    this.toggle_maximize_pane_ui(pane, cx);
                }
            }))
            // gpui-component context menus do not stop propagation, so the
            // bar menu lives on the empty tail only (never around the tabs).
            .context_menu(move |menu, window, cx| bar_menu(tail_root.clone(), pane, menu, window, cx));

        let strip_selector = pane_selector(names.strip, ordinal);
        let strip = div()
            .id((names.strip, pane.get()))
            .debug_selector(move || strip_selector.to_string())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .track_scroll(&scroll)
            .children(items)
            .child(tail);

        let open_tabs = if is_tab_row {
            let overflowing = scroll.max_offset().x.abs() > px(1.0);
            let picker_here = self.shell.tab_bar.picker.open
                && self
                    .shell
                    .tab_bar
                    .picker
                    .pane
                    .unwrap_or(self.workspace.focused_pane())
                    == pane;
            let tab_count = self
                .workspace
                .panes()
                .pane(pane)
                .map_or(0, |pane| pane.tabs().len());
            let show = overflowing || tab_count >= OPEN_TABS_ALWAYS_THRESHOLD || picker_here;
            show.then(|| self.render_open_tabs_button(pane, ordinal, window, cx))
        } else {
            None
        };
        let rows_toggle = is_tab_row.then(|| self.render_tab_rows_toggle(pane, ordinal, cx));

        let bar_selector = pane_selector(names.bar, ordinal);
        div()
            .id((names.bar, pane.get()))
            .debug_selector(move || bar_selector.to_string())
            .relative()
            .h(px(height))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(strip)
            .children(rows_toggle)
            .children(open_tabs)
            // The focused pane's strip carries a thin accent line (only
            // meaningful, so only drawn, with more than one pane).
            .when(focus_line, |bar| {
                bar.child(
                    div()
                        .debug_selector(|| "pane-focus-line".into())
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.0))
                        .bg(theme::accent()),
                )
            })
            .into_any_element()
    }

    /// Fixed (never scrolls with the tabs) toggle for the grouped-rows strip,
    /// at the trailing end of the tabs row next to the "Open tabs" button.
    fn render_tab_rows_toggle(
        &self,
        pane: PaneId,
        ordinal: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selector = pane_selector("tab-rows-toggle", ordinal);
        div()
            .flex_shrink_0()
            .pl(px(4.0))
            .child(
                Button::new(("tab-rows-toggle", pane.get()))
                    .debug_selector(move || selector.to_string())
                    .icon(Icon::new(TabGlyph::Rows).size(px(16.0)))
                    .compact()
                    .ghost()
                    .selected(self.workspace.tab_rows_of(pane))
                    .tooltip("Group tabs in two rows")
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_pane_tab_rows(pane, cx))),
            )
            .into_any_element()
    }

    /// Double-click on a tab or the empty strip: show the pane alone, or
    /// restore the layout.
    pub(super) fn toggle_maximize_pane_ui(&mut self, pane: PaneId, cx: &mut Context<Self>) {
        if self.workspace.toggle_maximize_pane(pane) {
            self.after_focus_change(cx);
            cx.notify();
        }
    }

    /// A group chip. Inline it toggles the group's collapse; in the
    /// grouped-rows strip it selects the group (its last active tab). The
    /// "Other" chip (`group` is `None`) is no drop target.
    fn render_group_chip(
        &self,
        pane: PaneId,
        chip: RowChip<'_>,
        state: ChipState,
        ordinal: usize,
        pane_ordinal: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = chip.group;
        let count = chip.count;
        // The connection's state, like the sidebar row's dot ("Other" has none).
        let state_color = key.map_or_else(theme::text_faint, |key| {
            self.connection_status_color(&key.connection_id)
        });
        let color = key.map_or_else(theme::text_muted, |key| self.group_color(key));
        let label = key.map_or_else(|| "Other".to_string(), |key| self.group_label(key));
        let (name, database) = key.map_or(("Other", None), |key| self.group_label_parts(key));
        let (name, database) = (name.to_string(), database.map(str::to_string));
        let type_badge = key
            .and_then(|key| self.connection_type_of(&key.connection_id))
            .map(|ty| {
                connection_policy::type_badge(
                    ty,
                    if pane_ordinal == 0 {
                        format!("tab-group-chip-type-{ordinal}")
                    } else {
                        format!("tab-group-chip-type-{ordinal}-p{pane_ordinal}")
                    },
                )
            });
        let tooltip_label: SharedString = label.into();
        let (collapsed, selected, selector) = match state {
            ChipState::Inline { collapsed } => (collapsed, false, false),
            ChipState::Selector { active } => (false, active, true),
        };
        let hover = key.and_then(|key| {
            self.shell.tab_bar.hover.as_ref().filter(|(target, _)| {
                matches!(target, TabDropTarget::GroupStart(p, k) if *p == pane && k == key)
            })
        });
        let show_indicator = hover.is_some_and(|(_, plan)| matches!(plan, TabMovePlan::Move(_)));
        let refused = hover.is_some_and(|(_, plan)| *plan == TabMovePlan::Refused);
        let (border, fill) = if selected {
            (color, super::navigator_ui::tinted(color, 0.45))
        } else if selector {
            (
                super::navigator_ui::tinted(color, 0.4),
                super::navigator_ui::tinted(color, 0.1),
            )
        } else {
            (
                super::navigator_ui::tinted(color, 0.6),
                super::navigator_ui::tinted(color, 0.2),
            )
        };
        let click_key = key.cloned();
        div()
            .id(("tab-group-chip", pane.get() * 1000 + ordinal as u64))
            .debug_selector(move || {
                if pane_ordinal == 0 {
                    format!("tab-group-chip-{ordinal}")
                } else {
                    format!("tab-group-chip-{ordinal}-p{pane_ordinal}")
                }
            })
            .relative()
            .h(px(22.0))
            .mx(px(4.0))
            .px(px(8.0))
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(6.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(border)
            .bg(fill)
            .text_size(px(12.0))
            .text_color(if selector && !selected {
                theme::text_muted()
            } else {
                theme::text()
            })
            .cursor_pointer()
            .when(refused, |chip| chip.opacity(0.5))
            .child(
                ui::status_dot(state_color)
                    .debug_selector(move || {
                        if pane_ordinal == 0 {
                            format!("tab-group-chip-state-{ordinal}")
                        } else {
                            format!("tab-group-chip-state-{ordinal}-p{pane_ordinal}")
                        }
                    })
                    .flex_shrink_0(),
            )
            // The name gives way (middle ellipsis) before the database does,
            // so chips of one connection stay told apart.
            .child(
                div()
                    .max_w(px(if database.is_some() { 120.0 } else { 160.0 }))
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis_middle()
                    .child(name),
            )
            .when_some(database, |chip, database| {
                chip.child(
                    div()
                        .debug_selector(move || {
                            if pane_ordinal == 0 {
                                format!("tab-group-chip-db-{ordinal}")
                            } else {
                                format!("tab-group-chip-db-{ordinal}-p{pane_ordinal}")
                            }
                        })
                        .max_w(px(100.0))
                        .flex_shrink_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(theme::text_muted())
                        .child(format!("· {database}")),
                )
            })
            .children(type_badge)
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tooltip_label.clone()).build(window, cx)
            })
            .when(collapsed || selector, |chip| {
                chip.child(
                    div()
                        .px(px(5.0))
                        .rounded(px(8.0))
                        .bg(color)
                        .text_size(px(10.0))
                        .text_color(gpui::white())
                        .child(count.to_string()),
                )
            })
            .when(!selector, |chip| {
                chip.child(
                    Icon::new(if collapsed {
                        IconName::ChevronRight
                    } else {
                        IconName::ChevronDown
                    })
                    .size(px(12.0))
                    .text_color(theme::text_muted()),
                )
            })
            .when(show_indicator, |chip| chip.child(indicator(false)))
            .on_click(cx.listener(move |this, _, _, cx| match state {
                ChipState::Inline { .. } => {
                    if let Some(key) = &click_key {
                        this.workspace.toggle_group_collapsed_in(pane, key);
                    }
                    cx.notify();
                }
                ChipState::Selector { .. } => {
                    this.activate_group_ui(pane, click_key.as_ref(), cx)
                }
            }))
            .when_some(key.cloned(), |chip, key| {
                let drop_key = key.clone();
                chip.on_drag_move(cx.listener(
                    move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                        if !event.bounds.contains(&event.event.position) {
                            return;
                        }
                        let dragged = event.drag(cx).id;
                        this.set_drag_hover(
                            TabDropTarget::GroupStart(pane, key.clone()),
                            dragged,
                            cx,
                        );
                    },
                ))
                .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                    this.finish_tab_drop(
                        drag.id,
                        TabDropTarget::GroupStart(pane, drop_key.clone()),
                        cx,
                    );
                }))
            })
            .context_menu({
                let root = cx.entity().downgrade();
                let menu_key = key.cloned();
                move |menu, window, cx| {
                    group_chip_menu(root.clone(), pane, menu_key.clone(), menu, window, cx)
                }
            })
            .into_any_element()
    }

    fn render_tab(
        &self,
        tab: &Tab,
        group_color: Option<Rgba>,
        active: bool,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let pinned = tab.is_pinned;
        let (name, suffix) = tab_label_parts(tab, compact);
        let label = tab_label(tab, compact);
        let icon = self.tab_icon(tab);
        // A dot on the Driver Manager tab while driver updates are waiting.
        let driver_updates =
            tab.kind == TabKind::DriverManager && self.driver_ui.updates_available > 0;
        let renaming = self
            .shell
            .tab_bar
            .rename
            .as_ref()
            .filter(|rename| rename.tab_id == tab_id);
        let hover = self.shell.tab_bar.hover.as_ref().filter(|(target, _)| {
            matches!(target, TabDropTarget::Before(id) | TabDropTarget::After(id) if *id == tab_id)
        });
        let indicator_left = hover.and_then(|(target, plan)| {
            (matches!(plan, TabMovePlan::Move(_)))
                .then(|| matches!(target, TabDropTarget::Before(_)))
        });
        let refused = hover.is_some_and(|(_, plan)| *plan == TabMovePlan::Refused);

        // Text widths were measured (and cached) by `measure_tab_texts`; a zero
        // width means the text system cannot shape (headless tests) and the
        // per-kind default width applies.
        let widths = &self.shell.tab_bar.text_widths;
        let fit = widths
            .get(&*name)
            .copied()
            .filter(|width| *width > 0.0)
            .and_then(|name_w| match suffix.as_deref() {
                None => Some(fit_tab_text(name_w, None, pinned)),
                Some(suffix) => widths
                    .get(suffix)
                    .copied()
                    .map(|suffix_w| fit_tab_text(name_w, Some(suffix_w), pinned)),
            });

        let title = match (renaming, fit) {
            (Some(rename), _) => div()
                .flex_1()
                .min_w_0()
                .child(
                    Input::new(&rename.input)
                        .appearance(false)
                        .with_size(Size::XSmall),
                )
                .into_any_element(),
            (None, Some(fit)) => div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(
                    div()
                        .w(px(fit.name_width))
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis_middle()
                        .child(name.to_string()),
                )
                .when_some(
                    fit.suffix_width.zip(suffix.clone()),
                    |row, (width, suffix)| {
                        row.child(
                            div()
                                .w(px(width))
                                .flex_shrink_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(theme::text_muted())
                                .child(suffix),
                        )
                    },
                )
                .into_any_element(),
            (None, None) => div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis_middle()
                .child(label.clone())
                .into_any_element(),
        };

        let ghost_title: SharedString = label.into();
        let tooltip_text = ghost_title.clone();
        let root = cx.entity().downgrade();
        let menu_root = root.clone();
        let mut element = div()
            .id(("workspace-tab", tab_id))
            .debug_selector(move || format!("workspace-tab-{tab_id}"))
            .relative()
            .w(px(
                fit.map_or_else(|| tab_width(tab, compact), |fit| fit.width)
            ))
            .h_full()
            .flex()
            .items_center()
            .gap(px(7.0))
            .px(px(if pinned { 8.0 } else { 10.0 }))
            .flex_shrink_0()
            .text_size(px(12.0))
            .border_r_1()
            .border_color(theme::border())
            .when(refused, |tab| tab.opacity(0.5))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, cx)
            })
            .child(icon.element(
                13.0,
                if active {
                    theme::text()
                } else {
                    theme::text_muted()
                },
            ))
            .child(title)
            .when(driver_updates, |tab| {
                tab.child(
                    // On the icon's corner: no layout width, so the title keeps its room.
                    div()
                        .debug_selector(|| "driver-update-badge".into())
                        .absolute()
                        .left(px(19.0))
                        .top(px(9.0))
                        .size(px(6.0))
                        .rounded_full()
                        .bg(theme::accent()),
                )
            })
            .when(pinned, |tab| {
                tab.child(
                    Icon::new(TabGlyph::Pin)
                        .size(px(11.0))
                        .text_color(theme::accent()),
                )
            })
            .when(!pinned, |tab| {
                tab.child(
                    ui::icon_button(("tab-close", tab_id), IconName::Close, "Close tab")
                        .debug_selector(move || format!("tab-close-{tab_id}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_close_tab(tab_id, window, cx);
                            cx.stop_propagation();
                        })),
                )
            })
            .when_some(group_color, |tab, color| {
                tab.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(px(2.0))
                        .bg(color),
                )
            })
            // The connection type colours the top edge (Production stands out).
            .when_some(self.connection_type_of(&tab.connection_id), |tab, ty| {
                tab.child(
                    div()
                        .debug_selector(move || format!("tab-type-stripe-{tab_id}"))
                        .absolute()
                        .left_0()
                        .right_0()
                        .top_0()
                        .h(px(if ty.is_production() { 3.0 } else { 2.0 }))
                        .bg(connection_policy::type_color(ty)),
                )
            })
            .when_some(indicator_left, |tab, left| tab.child(indicator(left)))
            .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                this.activate_tab_ui(tab_id, cx);
                // Double-click: show the tab's pane alone, or restore.
                if event.click_count() >= 2 {
                    if let Some(pane) = this.workspace.pane_of(tab_id) {
                        this.toggle_maximize_pane_ui(pane, cx);
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(move |this, _, window, cx| {
                    // Pinned tabs never close by middle-click.
                    if this.workspace.tab(tab_id).is_some_and(|tab| !tab.is_pinned) {
                        this.confirm_close_tab(tab_id, window, cx);
                    }
                }),
            );
        if renaming.is_none() {
            element = element.on_drag(TabDrag { id: tab_id }, move |_, _, _, cx| {
                let root = root.clone();
                let title = ghost_title.clone();
                let _ = root.update(cx, |this, _| this.shell.tab_bar.dragging = true);
                cx.new(|cx| {
                    // The ghost lives exactly as long as gpui's active drag, so
                    // its release clears the indicator on drop, escape, and
                    // drop-outside alike (no timers, no polling).
                    cx.on_release(move |_, app| {
                        let _ = root.update(app, |this, cx| {
                            this.shell.tab_bar.dragging = false;
                            this.shell.tab_bar.clear_drag_hover();
                            cx.notify();
                        });
                    })
                    .detach();
                    TabDragGhost { title, icon }
                })
            });
        }
        let element = element
            .on_drag_move(
                cx.listener(move |this, event: &gpui::DragMoveEvent<TabDrag>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let dragged = event.drag(cx).id;
                    let target = if event.event.position.x > event.bounds.center().x {
                        TabDropTarget::After(tab_id)
                    } else {
                        TabDropTarget::Before(tab_id)
                    };
                    this.set_drag_hover(target, dragged, cx);
                }),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.drop_on_tab(drag.id, tab_id, cx);
            }));
        let element = if active {
            element.bg(theme::background()).text_color(theme::text())
        } else {
            element
                .text_color(theme::text_muted())
                .hover(|style| style.bg(theme::hover()).text_color(theme::text()))
        };
        element
            .context_menu(move |menu, window, cx| {
                tab_menu(menu_root.clone(), tab_id, menu, window, cx)
            })
            .into_any_element()
    }

    // ---- open tabs picker -----------------------------------------------

    /// Tabs matching the picker search (title, displayed label, connection
    /// name or database; case-insensitive), in bar order.
    pub(super) fn picker_filtered(&self) -> Vec<u64> {
        let query = self.shell.tab_bar.picker.query.trim().to_lowercase();
        let compact = self.shell.tab_bar.compact_titles;
        let pane = self
            .shell
            .tab_bar
            .picker
            .pane
            .unwrap_or_else(|| self.workspace.focused_pane());
        let in_pane = self.workspace.panes().pane(pane);
        self.workspace
            .tabs()
            .iter()
            .filter(|tab| in_pane.is_some_and(|pane| pane.tabs().contains(&tab.id)))
            .filter(|tab| {
                query.is_empty()
                    || tab.title.to_lowercase().contains(&query)
                    || tab_label(tab, compact).to_lowercase().contains(&query)
                    || tab.database.to_lowercase().contains(&query)
                    || self
                        .connection_name(&tab.connection_id)
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .map(|tab| tab.id)
            .collect()
    }

    /// Opens or closes the "Open tabs" picker of one pane's strip.
    fn set_open_tabs_open_in(
        &mut self,
        pane: PaneId,
        open: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let same_pane = self.shell.tab_bar.picker.pane.unwrap_or(pane) == pane;
        if self.shell.tab_bar.picker.open == open && same_pane {
            return;
        }
        self.shell.tab_bar.picker.pane = Some(pane);
        self.shell.tab_bar.picker.open = open;
        if open {
            let input = self.open_tabs_input(window, cx);
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
                state.focus(window, cx);
            });
            self.shell.tab_bar.picker.query.clear();
            let active = self.workspace.pane_active_tab(pane).map(|tab| tab.id);
            self.shell.tab_bar.picker.selected = self
                .picker_filtered()
                .iter()
                .position(|id| Some(*id) == active)
                .unwrap_or(0);
        }
        cx.notify();
    }

    fn open_tabs_input(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Entity<InputState> {
        if let Some(input) = self.shell.tab_bar.picker.input.clone() {
            return input;
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search open tabs..."));
        let input_for_events = input.clone();
        cx.subscribe(
            &input,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| match event {
                gpui_component::input::InputEvent::Change => {
                    this.shell.tab_bar.picker.query = input_for_events.read(cx).value().to_string();
                    this.shell.tab_bar.picker.selected = 0;
                    cx.notify();
                }
                gpui_component::input::InputEvent::PressEnter { .. } => {
                    this.activate_picker_selection(cx);
                }
                _ => {}
            },
        )
        .detach();
        self.shell.tab_bar.picker.input = Some(input.clone());
        input
    }

    pub(super) fn picker_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.picker_filtered().len();
        if len == 0 {
            return;
        }
        let picker = &mut self.shell.tab_bar.picker;
        let next = (picker.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        picker.selected = next;
        picker
            .list_scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    pub(super) fn activate_picker_selection(&mut self, cx: &mut Context<Self>) {
        let filtered = self.picker_filtered();
        if let Some(id) = filtered.get(self.shell.tab_bar.picker.selected).copied() {
            self.activate_tab_from_picker(id, cx);
        }
    }

    pub(super) fn activate_tab_from_picker(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        self.shell.tab_bar.picker.open = false;
        self.activate_tab_ui(tab_id, cx);
    }

    fn render_open_tabs_button(
        &mut self,
        pane: PaneId,
        ordinal: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.shell.tab_bar.picker.open
            && self
                .shell
                .tab_bar
                .picker
                .pane
                .unwrap_or_else(|| self.workspace.focused_pane())
                == pane;
        let selector = if ordinal == 0 {
            "open-tabs".to_string()
        } else {
            format!("open-tabs-p{ordinal}")
        };
        let trigger = Button::new(("open-tabs", pane.get()))
            .debug_selector(move || selector.clone())
            .icon(Icon::new(IconName::ChevronDown).size(px(16.0)))
            .compact()
            .ghost()
            .tooltip("Open tabs");
        let root = cx.entity().downgrade();
        let mut popover = Popover::new(("open-tabs-popover", pane.get()))
            .anchor(Anchor::TopRight)
            .trigger(trigger)
            .open(open)
            .p(px(4.0))
            .rounded(px(6.0))
            .bg(theme::surface())
            .on_open_change(move |open, window, app| {
                let _ = root.update(app, |this, cx| {
                    this.set_open_tabs_open_in(pane, *open, window, cx);
                });
            });
        if open {
            popover = popover.child(self.render_open_tabs_panel(window, cx));
        }
        div()
            .flex_shrink_0()
            .px(px(4.0))
            .child(popover)
            .into_any_element()
    }

    fn render_open_tabs_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let input = self.open_tabs_input(window, cx);
        let filtered = std::rc::Rc::new(self.picker_filtered());
        let search = div()
            .h(px(36.0))
            .px(px(4.0))
            .pb(px(4.0))
            .border_b_1()
            .border_color(theme::border())
            .child(
                Input::new(&input)
                    .prefix(
                        Icon::new(IconName::Search)
                            .size(px(14.0))
                            .text_color(theme::text_muted()),
                    )
                    .with_size(Size::Small),
            );
        let max_height = (f32::from(window.viewport_size().height) * 0.7).min(PICKER_MAX_HEIGHT);
        let body = if filtered.is_empty() {
            div()
                .debug_selector(|| "open-tabs-empty".into())
                .py(px(16.0))
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .flex()
                .justify_center()
                .child("No matching tabs")
                .into_any_element()
        } else {
            let height = (filtered.len() as f32 * PICKER_ROW_HEIGHT).min(max_height);
            let rows = filtered.clone();
            uniform_list(
                "open-tabs-list",
                filtered.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    range
                        .filter_map(|index| {
                            rows.get(index)
                                .copied()
                                .map(|id| this.render_picker_row(id, index, cx))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.shell.tab_bar.picker.list_scroll)
            .h(px(height))
            .pt(px(4.0))
            .into_any_element()
        };
        div()
            .key_context("tab-picker")
            .debug_selector(|| "open-tabs-panel".into())
            .w(px(PICKER_WIDTH))
            .min_w(px(224.0))
            .max_w(px(320.0))
            .flex()
            .flex_col()
            .on_action(cx.listener(|this, _: &TabPickerPrev, _, cx| this.picker_move(-1, cx)))
            .on_action(cx.listener(|this, _: &TabPickerNext, _, cx| this.picker_move(1, cx)))
            .child(search)
            .child(body)
            .into_any_element()
    }

    fn render_picker_row(&self, tab_id: u64, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.workspace.tab(tab_id) else {
            return div().into_any_element();
        };
        let active = self.workspace.active_tab().map(|active| active.id) == Some(tab_id);
        let highlighted = self.shell.tab_bar.picker.selected == index;
        let pinned = tab.is_pinned;
        let label = tab_label(tab, self.shell.tab_bar.compact_titles);
        let menu_root = cx.entity().downgrade();
        div()
            .id(("open-tab-row", tab_id))
            .debug_selector(move || format!("open-tab-row-{tab_id}"))
            .h(px(PICKER_ROW_HEIGHT))
            .w_full()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(6.0))
            .text_size(px(12.0))
            .text_color(theme::text())
            .when(active, |row| row.bg(theme::selected()))
            .when(highlighted && !active, |row| row.bg(theme::hover()))
            .hover(|style| style.bg(theme::hover()))
            .child(self.tab_icon(tab).element(14.0, theme::text_muted()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(label),
            )
            .when(pinned, |row| {
                row.child(
                    Icon::new(TabGlyph::Pin)
                        .size(px(12.0))
                        .text_color(theme::accent()),
                )
            })
            .child(div().w(px(20.0)).flex_shrink_0().when(!pinned, |slot| {
                slot.child(
                    ui::icon_button(("open-tab-close", tab_id), IconName::Close, "Close tab")
                        .debug_selector(move || format!("open-tab-close-{tab_id}"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm_close_tab(tab_id, window, cx);
                            cx.stop_propagation();
                        })),
                )
            }))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_tab_from_picker(tab_id, cx);
            }))
            .context_menu(move |menu, window, cx| {
                tab_menu(menu_root.clone(), tab_id, menu, window, cx)
            })
            .into_any_element()
    }
}

// ---- menus ------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MenuIcon {
    None,
    Glyph(TabGlyph),
    Named(&'static str),
}

/// Declarative menu content; the gpui menu and the tests both consume it, so
/// the visibility/label/disabled rules have one source of truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TabMenuEntry {
    Separator,
    Item {
        label: &'static str,
        command: TabCommand,
        icon: MenuIcon,
        disabled: bool,
        checked: bool,
    },
    /// "Group Tabs By" with None / Connection / Database children.
    GroupBySubmenu,
}

impl TabMenuEntry {
    fn item(label: &'static str, command: TabCommand, icon: MenuIcon) -> Self {
        Self::Item {
            label,
            command,
            icon,
            disabled: false,
            checked: false,
        }
    }

    fn disabled(mut self, value: bool) -> Self {
        if let Self::Item { disabled, .. } = &mut self {
            *disabled = value;
        }
        self
    }
}

/// Entries of the tab context menu. Split Right / Down need a
/// second tab in the pane; Move to Other Pane / Close Pane need two panes.
pub(super) fn tab_menu_entries(state: &TabMenuState) -> Vec<TabMenuEntry> {
    use TabCloseScope as Scope;
    use TabMenuEntry as E;
    let close = MenuIcon::Named("close");
    let mut entries = vec![E::item(
        if state.compact {
            "Show Full Tab Titles"
        } else {
            "Shorten Tab Titles"
        },
        TabCommand::ToggleCompact,
        MenuIcon::Glyph(if state.compact {
            TabGlyph::Maximize
        } else {
            TabGlyph::Minimize
        }),
    )];
    if state.can_rename {
        entries.push(E::item(
            "Rename Tab",
            TabCommand::Rename,
            MenuIcon::Glyph(TabGlyph::Pencil),
        ));
    }
    if state.can_duplicate {
        entries.push(E::item(
            "Duplicate Tab",
            TabCommand::Duplicate,
            MenuIcon::Named("copy"),
        ));
    }
    entries.push(E::item(
        "Copy Name",
        TabCommand::CopyName,
        MenuIcon::Named("copy"),
    ));
    if state.can_locate {
        entries.push(E::item(
            "Locate in Sidebar",
            TabCommand::Locate,
            MenuIcon::Glyph(TabGlyph::Crosshair),
        ));
    }
    entries.push(E::Separator);
    entries.push(E::item(
        if state.pinned { "Unpin Tab" } else { "Pin Tab" },
        TabCommand::TogglePin,
        MenuIcon::Glyph(TabGlyph::Pin),
    ));
    entries.push(E::GroupBySubmenu);
    entries.push(E::Separator);
    if state.can_split {
        entries.push(E::item(
            "Split Right",
            TabCommand::SplitRight,
            MenuIcon::Glyph(TabGlyph::SplitRight),
        ));
        entries.push(E::item(
            "Split Down",
            TabCommand::SplitDown,
            MenuIcon::Glyph(TabGlyph::SplitDown),
        ));
    }
    if state.multi_pane {
        entries.push(E::item(
            "Move to Other Pane",
            TabCommand::MoveToOtherPane,
            MenuIcon::None,
        ));
        entries.push(E::item("Close Pane", TabCommand::ClosePane, close));
    }
    if state.can_split || state.multi_pane {
        entries.push(E::Separator);
    }
    entries.push(
        E::item(state.others_label, TabCommand::Close(Scope::Others), close)
            .disabled(state.others_disabled),
    );
    entries.push(
        E::item(
            "Close Tabs to the Left",
            TabCommand::Close(Scope::Left),
            close,
        )
        .disabled(state.left_disabled),
    );
    entries.push(
        E::item(
            "Close Tabs to the Right",
            TabCommand::Close(Scope::Right),
            close,
        )
        .disabled(state.right_disabled),
    );
    entries.push(E::item(
        state.all_label,
        TabCommand::Close(Scope::All),
        close,
    ));
    if state.in_group {
        entries.push(E::item(
            "Close Tab Group",
            TabCommand::Close(Scope::Group),
            close,
        ));
    }
    entries.push(E::item(state.close_label, TabCommand::CloseTab, close));
    entries
}

/// Entries of the empty tab-bar area menu (after its "Group tabs by" label).
pub(super) fn bar_menu_entries(group_by: TabGroupBy, compact: bool) -> Vec<TabMenuEntry> {
    let mut entries: Vec<TabMenuEntry> = group_by_entries(group_by);
    entries.push(TabMenuEntry::Separator);
    entries.push(TabMenuEntry::item(
        if compact {
            "Show Full Tab Titles"
        } else {
            "Shorten Tab Titles"
        },
        TabCommand::ToggleCompact,
        MenuIcon::Glyph(if compact {
            TabGlyph::Maximize
        } else {
            TabGlyph::Minimize
        }),
    ));
    entries
}

fn group_by_entries(current: TabGroupBy) -> Vec<TabMenuEntry> {
    [
        ("None", TabGroupBy::None),
        ("Connection", TabGroupBy::Connection),
        ("Database", TabGroupBy::Database),
    ]
    .into_iter()
    .map(|(label, mode)| {
        let mut entry = TabMenuEntry::item(label, TabCommand::GroupBy(mode), MenuIcon::None);
        if let TabMenuEntry::Item { checked, .. } = &mut entry {
            *checked = current == mode;
        }
        entry
    })
    .collect()
}

fn menu_item(
    root: &gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    entry: &TabMenuEntry,
) -> Option<PopupMenuItem> {
    let TabMenuEntry::Item {
        label,
        command,
        icon,
        disabled,
        checked,
    } = entry
    else {
        return None;
    };
    let (root, command, tab_id) = (root.clone(), *command, tab_id);
    let mut item = PopupMenuItem::new(*label)
        .disabled(*disabled)
        .checked(*checked)
        .on_click(move |_, window, app| {
            let _ = root.update(app, |this, cx| {
                this.run_tab_command(tab_id, command, window, cx);
            });
        });
    item = match icon {
        MenuIcon::None => item,
        MenuIcon::Glyph(glyph) => item.icon(Icon::new(*glyph)),
        MenuIcon::Named("close") => item.icon(Icon::new(IconName::Close)),
        MenuIcon::Named(_) => item.icon(Icon::new(IconName::Copy)),
    };
    if command == TabCommand::CloseTab {
        // Shows the Cmd-W hint; the click handler above still wins.
        item = item.action(Box::new(CloseActiveTab));
    }
    Some(item)
}

fn append_entries(
    mut menu: PopupMenu,
    root: &gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    group_by: TabGroupBy,
    entries: &[TabMenuEntry],
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for entry in entries {
        match entry {
            TabMenuEntry::Separator => menu = menu.separator(),
            TabMenuEntry::GroupBySubmenu => {
                let root = root.clone();
                menu = menu.submenu("Group Tabs By", window, cx, move |sub, _, _| {
                    group_by_entries(group_by)
                        .iter()
                        .filter_map(|entry| menu_item(&root, tab_id, entry))
                        .fold(sub, |sub, item| sub.item(item))
                });
            }
            item => {
                if let Some(item) = menu_item(root, tab_id, item) {
                    menu = menu.item(item);
                }
            }
        }
    }
    menu
}

/// Actions of a group chip's context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GroupChipCommand {
    NewQuery,
    CloseAll,
    Disconnect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GroupChipMenuState {
    /// The chip stands for a connection ("Other" does not).
    pub(super) has_connection: bool,
    pub(super) can_query: bool,
    pub(super) can_disconnect: bool,
}

/// Context menu of a group chip: New Query, Close All Tabs in Group (confirms
/// staged changes like closing tabs normally) and Disconnect (the sidebar's
/// disconnect). "Other" only offers the close.
fn group_chip_menu(
    root: gpui::WeakEntity<AppRoot>,
    pane: PaneId,
    group: Option<TabGroupKey>,
    menu: PopupMenu,
    _window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(state) = root
        .update(cx, |this, _| this.group_chip_menu_state(group.as_ref()))
        .ok()
    else {
        return menu;
    };
    let item = |label: &'static str, icon: IconName, command: GroupChipCommand, disabled: bool| {
        let (root, group) = (root.clone(), group.clone());
        PopupMenuItem::new(label)
            .icon(Icon::new(icon))
            .disabled(disabled)
            .on_click(move |_, window, app| {
                let _ = root.update(app, |this, cx| {
                    this.run_group_command(pane, group.as_ref(), command, window, cx);
                });
            })
    };
    let mut menu = menu;
    if state.has_connection {
        menu = menu.item(item(
            "New Query",
            IconName::SquareTerminal,
            GroupChipCommand::NewQuery,
            !state.can_query,
        ));
    }
    menu = menu.item(item(
        "Close All Tabs in Group",
        IconName::Close,
        GroupChipCommand::CloseAll,
        false,
    ));
    if state.has_connection {
        menu = menu.item(item(
            "Disconnect",
            IconName::CircleX,
            GroupChipCommand::Disconnect,
            !state.can_disconnect,
        ));
    }
    menu
}

/// Context menu of the empty tab-bar area: grouping and title length.
///
/// Per pane: "Group tabs by" applies to the pane whose strip was clicked. The
/// menu commands address a pane through one of its tabs (`run_tab_command`
/// resolves the pane), so a representative tab of the pane is passed.
fn bar_menu(
    root: gpui::WeakEntity<AppRoot>,
    pane: PaneId,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some((group_by, compact, tab_id)) = root
        .update(cx, |this, _| {
            let representative = this
                .workspace
                .panes()
                .pane(pane)
                .and_then(|pane| pane.active().or_else(|| pane.tabs().first().copied()))
                .unwrap_or_default();
            (
                this.workspace.group_by_of(pane),
                this.shell.tab_bar.compact_titles,
                representative,
            )
        })
        .ok()
    else {
        return menu;
    };
    let menu = menu.item(PopupMenuItem::label("Group tabs by"));
    let entries = bar_menu_entries(group_by, compact);
    append_entries(menu, &root, tab_id, group_by, &entries, window, cx)
}

fn tab_menu(
    root: gpui::WeakEntity<AppRoot>,
    tab_id: u64,
    menu: PopupMenu,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let Some(state) = root
        .update(cx, |this, _| this.tab_menu_state(tab_id))
        .ok()
        .flatten()
    else {
        return menu;
    };
    let entries = tab_menu_entries(&state);
    append_entries(menu, &root, tab_id, state.group_by, &entries, window, cx)
}
