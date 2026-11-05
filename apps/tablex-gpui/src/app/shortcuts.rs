//! The registry of keyboard actions: the single source of the default key
//! bindings, the Settings "Shortcuts" page, conflict detection and the runtime
//! re-binding.
//!
//! Chords are stored in gpui's keystroke form with `secondary-` for the
//! primary modifier (Cmd on macOS, Ctrl elsewhere), so one `settings.json`
//! means the same on every platform. User overrides live in
//! `Settings::shortcuts`; an action without an entry keeps its defaults.
//!
//! Re-binding: gpui's keymap can only grow (`clear_key_bindings` would also
//! wipe the gpui-component editor/list/input bindings registered at startup),
//! so a change adds `Unbind` entries for the chords that went away and plain
//! bindings for the new ones. That only happens when an Apply changes the
//! shortcuts, never per frame.

use super::actions::*;
use super::menus::{Hide, HideOtherApps, Minimize, Quit};
use gpui::{Action, App, InvalidKeystrokeError, KeyBinding, KeyBindingContextPredicate, Keystroke, OperatingSystem};
use std::collections::BTreeMap;
use std::rc::Rc;

/// `Settings::shortcuts`: action id to chords.
pub(super) type Overrides = BTreeMap<String, Vec<String>>;

type Build = fn(&App, &str, Option<&str>) -> KeyBinding;

/// The host as `secondary-` reads it where no `App` is at hand. The registry picks Cmd or Ctrl when it is
/// built (`cfg!(target_os = "macos")`), so the parse must too.
const HOST_OS: OperatingSystem = if cfg!(target_os = "macos") {
    OperatingSystem::Mac
} else if cfg!(target_os = "windows") {
    OperatingSystem::Windows
} else {
    OperatingSystem::Linux
};

/// One keystroke in the registry's form, `secondary-` included (gpui's `Keystroke::parse` takes only
/// static text with explicit modifiers).
pub(super) fn parse_chord(chord: &str) -> Result<Keystroke, InvalidKeystrokeError> {
    Keystroke::parse_with_os(chord, HOST_OS)
}

/// A binding of a chord from the registry or `settings.json`. gpui's `KeyBinding::new` takes only static
/// chords with explicit modifiers; `load` resolves `secondary-` for the running platform. Panics on an
/// unreadable chord or context, as `KeyBinding::new` does: the registry only holds normalized chords.
pub(super) fn key_binding(cx: &App, chord: &str, action: impl Action, context: Option<&str>) -> KeyBinding {
    let context = context.map(|context| Rc::new(KeyBindingContextPredicate::parse(context).expect("key context")));
    KeyBinding::load(chord, Box::new(action), context, false, None, cx).expect("a normalized chord")
}

/// One bindable keyboard action.
pub(super) struct ShortcutDef {
    /// Stable id stored in `settings.json`.
    pub(super) id: &'static str,
    pub(super) category: &'static str,
    pub(super) title: &'static str,
    /// Key contexts the chords are active in (`None` = everywhere).
    pub(super) contexts: &'static [Option<&'static str>],
    mac: &'static [&'static str],
    other: &'static [&'static str],
    /// Internal keys (text-field Enter, list navigation): listed, not editable.
    pub(super) editable: bool,
    macos_only: bool,
    /// Inside its (non-global) contexts this action wins over a global
    /// action on the same chord: gpui resolves a chord to the deepest
    /// matching context, so it is not a clash (e.g. Find in Diagram over
    /// Find in Results).
    scoped_override: bool,
    build: Build,
}

impl ShortcutDef {
    const fn new(
        id: &'static str,
        category: &'static str,
        title: &'static str,
        contexts: &'static [Option<&'static str>],
        chords: &'static [&'static str],
        build: Build,
    ) -> Self {
        Self {
            id,
            category,
            title,
            contexts,
            mac: chords,
            other: chords,
            editable: true,
            macos_only: false,
            scoped_override: false,
            build,
        }
    }

    const fn scoped_override(mut self) -> Self {
        self.scoped_override = true;
        self
    }

    fn is_global(&self) -> bool {
        self.contexts.iter().any(Option::is_none)
    }

    const fn per_platform(
        mut self,
        mac: &'static [&'static str],
        other: &'static [&'static str],
    ) -> Self {
        self.mac = mac;
        self.other = other;
        self
    }

    const fn fixed(mut self) -> Self {
        self.editable = false;
        self
    }

    const fn macos_only(mut self) -> Self {
        self.macos_only = true;
        self
    }

    /// The default chords of the running platform, normalized.
    pub(super) fn default_chords(&self) -> Vec<String> {
        self.defaults_for(cfg!(target_os = "macos"))
    }

    fn defaults_for(&self, macos: bool) -> Vec<String> {
        let chords = if macos { self.mac } else { self.other };
        chords.iter().filter_map(|chord| normalize(chord)).collect()
    }

    /// The chords in force: the override, else the defaults. An override that
    /// holds only unreadable chords (a hand-edited file) falls back to the
    /// defaults; an empty override means "unbound".
    pub(super) fn effective(&self, overrides: &Overrides) -> Vec<String> {
        if !self.editable {
            return self.default_chords();
        }
        let Some(chords) = overrides.get(self.id) else {
            return self.default_chords();
        };
        let mut valid: Vec<String> = Vec::new();
        for chord in chords.iter().filter_map(|chord| normalize(chord)) {
            if !valid.contains(&chord) {
                valid.push(chord);
            }
        }
        if valid.is_empty() && !chords.is_empty() {
            self.default_chords()
        } else {
            valid
        }
    }

    pub(super) fn is_modified(&self, overrides: &Overrides) -> bool {
        self.effective(overrides) != self.default_chords()
    }

    fn bindings(&self, cx: &App, chord: &str) -> Vec<KeyBinding> {
        self.contexts
            .iter()
            .map(|context| (self.build)(cx, chord, *context))
            .collect()
    }

    /// Bindings that switch `chord` off for this action in every context.
    fn unbindings(&self, cx: &App, chord: &str) -> Vec<KeyBinding> {
        self.contexts
            .iter()
            .map(|context| {
                let name = (self.build)(cx, chord, *context).action().name();
                key_binding(cx, chord, gpui::Unbind(name.into()), *context)
            })
            .collect()
    }
}

macro_rules! bind {
    ($action:expr) => {
        |cx, chord, context| key_binding(cx, chord, $action, context)
    };
}

const ANY: &[Option<&str>] = &[None];
const GRID: &[Option<&str>] = &[Some("grid")];
const SIDEBAR: &[Option<&str>] = &[Some("sidebar")];
/// The diagram, and its search field (which would otherwise route the chord
/// to the Input's own `Search` action).
const DIAGRAM_FIND: &[Option<&str>] = &[Some("diagram"), Some("diagram > Input")];

/// Every keyboard action, in page order. The defaults here are the whole
/// default keymap of the app.
static DEFS: &[ShortcutDef] = &[
    // Primary shortcuts are `secondary-`: Cmd on macOS, Ctrl on Linux and
    // Windows (where `cmd` would be the Super/Win key).
    ShortcutDef::new(
        "workspace.open_command_palette",
        "General",
        "Open Command Palette",
        ANY,
        &["secondary-k", "secondary-p"],
        bind!(OpenCommandPalette),
    ),
    ShortcutDef::new(
        "workspace.open_settings",
        "General",
        "Open Settings",
        ANY,
        &["secondary-,"],
        bind!(OpenSettings),
    ),
    ShortcutDef::new(
        "settings.close",
        "General",
        "Close Settings",
        &[Some("settings")],
        &["escape"],
        bind!(CloseSettings),
    ),
    ShortcutDef::new(
        "app.quit",
        "General",
        "Quit",
        ANY,
        &["secondary-q"],
        bind!(Quit),
    ),
    // Hide / Hide Others / Minimize are macOS conventions; Ctrl+H / Ctrl+M mean
    // other things on Linux and Windows editors.
    ShortcutDef::new(
        "app.hide",
        "General",
        "Hide TableX",
        ANY,
        &["secondary-h"],
        bind!(Hide),
    )
    .macos_only(),
    ShortcutDef::new(
        "app.hide_others",
        "General",
        "Hide Other Applications",
        ANY,
        &["secondary-alt-h"],
        bind!(HideOtherApps),
    )
    .macos_only(),
    ShortcutDef::new(
        "app.minimize",
        "General",
        "Minimize Window",
        ANY,
        &["secondary-m"],
        bind!(Minimize),
    )
    .macos_only(),
    ShortcutDef::new(
        "editor.save_script",
        "Editor",
        "Save Script",
        ANY,
        &["secondary-s"],
        bind!(SaveActiveSql),
    ),
    ShortcutDef::new(
        "editor.save_sql_file_as",
        "Editor",
        "Save SQL File As",
        ANY,
        &["secondary-shift-s"],
        bind!(SaveSqlFileAs),
    ),
    ShortcutDef::new(
        "editor.open_sql_file",
        "Editor",
        "Open SQL File",
        ANY,
        &["secondary-o"],
        bind!(OpenSqlFile),
    ),
    // Format chord: Ctrl+Shift+F. On macOS the code editor binds Cmd+Shift+F to Replace
    // (the component's own binding, deeper than this one), so there it is Option+Shift+F, VS Code's.
    ShortcutDef::new(
        "editor.format_sql",
        "Editor",
        "Format SQL",
        ANY,
        &[],
        bind!(FormatSql),
    )
    .per_platform(&["alt-shift-f"], &["secondary-shift-f"]),
    // Explain Execution Plan chord (Ctrl+Shift+E; Cmd+Shift+E on macOS). Neither the
    // keymap nor the code editor binds it. Explain Analyze runs the statement, so it has no default
    // chord (Ctrl+Alt is AltGr on Windows layouts and types characters): the button menu and the
    // palette offer it, and Settings can bind it.
    ShortcutDef::new(
        "editor.explain",
        "Editor",
        "Explain Plan",
        ANY,
        &["secondary-shift-e"],
        bind!(ExplainPlan),
    ),
    ShortcutDef::new(
        "editor.explain_analyze",
        "Editor",
        "Explain Analyze Plan",
        ANY,
        &[],
        bind!(ExplainPlanAnalyze),
    ),
    ShortcutDef::new(
        "workspace.close_tab",
        "Tabs & Panes",
        "Close Tab",
        ANY,
        &["secondary-w"],
        bind!(CloseActiveTab),
    ),
    // Next / previous tab group of the focused pane. PageUp / PageDown
    // everywhere (Ctrl on Linux / Windows, Cmd on macOS); the Safari-style
    // bracket pair too, for macOS keyboards without page keys.
    ShortcutDef::new(
        "workspace.next_tab_group",
        "Tabs & Panes",
        "Next Tab Group",
        ANY,
        &["secondary-shift-pagedown", "secondary-shift-]"],
        bind!(NextTabGroup),
    ),
    ShortcutDef::new(
        "workspace.previous_tab_group",
        "Tabs & Panes",
        "Previous Tab Group",
        ANY,
        &["secondary-shift-pageup", "secondary-shift-["],
        bind!(PreviousTabGroup),
    ),
    ShortcutDef::new(
        "workspace.toggle_error_log",
        "General",
        "Show Event and Error Log",
        ANY,
        &["secondary-shift-l"],
        bind!(ToggleErrorLog),
    ),
    // Cmd+Shift+A (Ctrl+Shift+A elsewhere). Free in the keymap, and not an editor binding: the
    // input's `ctrl-shift-a` (select to start of line) exists on macOS only, where this chord is
    // Cmd, not Ctrl.
    // In the assistant panel: Escape stops a running answer, else leaves the panel (the composer
    // lets the key through when it has nothing of its own to dismiss).
    ShortcutDef::new(
        "assistant.escape",
        "General",
        "Stop the Assistant or Leave Its Panel",
        &[Some("assistant")],
        &["escape"],
        bind!(AssistantEscape),
    )
    .fixed(),
    ShortcutDef::new(
        "workspace.toggle_assistant",
        "General",
        "Show AI Assistant",
        ANY,
        &["secondary-shift-a"],
        bind!(ToggleAssistant),
    ),
    // Directional pane focus (Cmd+K is the palette, so no Zed-style chord).
    // macOS: Cmd+Alt. Elsewhere the primary-plus-Alt chord (Ctrl+Alt+arrows) is
    // taken by workspace switching on many Linux desktops and by the editor's
    // add-cursor bindings, so plain Alt+arrows (unbound in the editor on those
    // platforms) is used instead.
    ShortcutDef::new(
        "workspace.focus_pane_left",
        "Tabs & Panes",
        "Focus Pane to the Left",
        ANY,
        &[],
        bind!(FocusPaneLeft),
    )
    .per_platform(&["secondary-alt-left"], &["alt-left"]),
    ShortcutDef::new(
        "workspace.focus_pane_right",
        "Tabs & Panes",
        "Focus Pane to the Right",
        ANY,
        &[],
        bind!(FocusPaneRight),
    )
    .per_platform(&["secondary-alt-right"], &["alt-right"]),
    ShortcutDef::new(
        "workspace.focus_pane_up",
        "Tabs & Panes",
        "Focus Pane Above",
        ANY,
        &[],
        bind!(FocusPaneUp),
    )
    .per_platform(&["secondary-alt-up"], &["alt-up"]),
    ShortcutDef::new(
        "workspace.focus_pane_down",
        "Tabs & Panes",
        "Focus Pane Below",
        ANY,
        &[],
        bind!(FocusPaneDown),
    )
    .per_platform(&["secondary-alt-down"], &["alt-down"]),
    ShortcutDef::new(
        "results.copy_selection",
        "Results",
        "Copy Selection",
        GRID,
        &["secondary-c"],
        bind!(CopyGridSelection),
    ),
    ShortcutDef::new(
        "results.refresh",
        "Results",
        "Refresh Results",
        GRID,
        &["secondary-r"],
        bind!(RefreshGridResults),
    ),
    ShortcutDef::new(
        "results.edit_cell",
        "Results",
        "Edit Cell",
        GRID,
        &["enter"],
        bind!(EditGridCell),
    ),
    ShortcutDef::new(
        "results.toggle_record_view",
        "Results",
        "Toggle Record View",
        GRID,
        &["tab"],
        bind!(ToggleRecordView),
    ),
    ShortcutDef::new(
        "results.cancel_cell_edit",
        "Results",
        "Cancel Cell Edit",
        GRID,
        &["escape"],
        bind!(CancelCellEdit),
    ),
    // Global so it works with the result tab active but nothing focused. The
    // find bar's own field would otherwise route Cmd+F to the Input's `Search`
    // action; re-focus + select the text instead.
    ShortcutDef::new(
        "results.find",
        "Results",
        "Find in Results",
        &[None, Some("grid-find > Input")],
        &["secondary-f"],
        bind!(OpenGridFind),
    ),
    ShortcutDef::new(
        "results.close_find",
        "Results",
        "Close Find Bar",
        &[Some("grid-find")],
        &["escape"],
        bind!(CloseGridFind),
    ),
    // Cmd+F in a code editor opens the editor's own search panel (find, next / previous, replace).
    // The global "results.find" would take the chord there: an equal-depth tie goes to the binding
    // added last, so this one comes after it in the table. An input that is not searchable (the
    // component's `Search` handler passes the action on) leaves the chord to the binding behind
    // this one: the find bar of the grid or the diagram.
    ShortcutDef::new(
        "editor.find",
        "Editor",
        "Find in Editor",
        &[Some("Input")],
        &["secondary-f"],
        bind!(gpui_component::input::Search),
    )
    .scoped_override(),
    ShortcutDef::new(
        "sidebar.copy_connection_name",
        "Sidebar",
        "Copy Connection Name",
        SIDEBAR,
        &["secondary-c"],
        bind!(CopyConnectionName),
    ),
    ShortcutDef::new(
        "sidebar.edit_connection",
        "Sidebar",
        "Edit Connection",
        SIDEBAR,
        &["secondary-e"],
        bind!(EditConnection),
    ),
    ShortcutDef::new(
        "sidebar.refresh_connection",
        "Sidebar",
        "Refresh Connection",
        SIDEBAR,
        &["f5"],
        bind!(RefreshConnection),
    ),
    ShortcutDef::new(
        "diagram.find",
        "Diagram",
        "Find in Diagram",
        DIAGRAM_FIND,
        &["secondary-f"],
        bind!(FocusDiagramSearch),
    )
    .scoped_override(),
    ShortcutDef::new(
        "diagram.clear",
        "Diagram",
        "Clear Selection and Search",
        &[Some("diagram")],
        &["escape"],
        bind!(ClearDiagramSelection),
    ),
    // Undo / redo of the diagram's edits. gpui resolves a chord to the deepest
    // matching context, so inside the diagram's own text fields (search, note
    // and group-name editors, whose context is `diagram > Input` /
    // `Textarea`) the editor's text undo keeps `secondary-z`; only the canvas
    // reaches these. No global action uses the chord, hence nothing to
    // override (see the registry test).
    ShortcutDef::new(
        "diagram.undo",
        "Diagram",
        "Undo Diagram Edit",
        &[Some("diagram")],
        &["secondary-z"],
        bind!(DiagramUndo),
    ),
    ShortcutDef::new(
        "diagram.redo",
        "Diagram",
        "Redo Diagram Edit",
        &[Some("diagram")],
        &["secondary-shift-z"],
        bind!(DiagramRedo),
    )
    .per_platform(
        &["secondary-shift-z"],
        &["secondary-shift-z", "secondary-y"],
    ),
    // Undo / redo of the structure edits of a table's Properties (edit mode only: the grid is the
    // `properties-edit` context). Inside the filter box (`properties-edit > Input`) the text undo
    // keeps `secondary-z`, like the diagram's fields.
    ShortcutDef::new(
        "properties.undo",
        "Properties",
        "Undo Structure Edit",
        &[Some("properties-edit")],
        &["secondary-z"],
        bind!(PropertiesUndo),
    ),
    ShortcutDef::new(
        "properties.redo",
        "Properties",
        "Redo Structure Edit",
        &[Some("properties-edit")],
        &["secondary-shift-z"],
        bind!(PropertiesRedo),
    )
    .per_platform(
        &["secondary-shift-z"],
        &["secondary-shift-z", "secondary-y"],
    ),
    // Palette: Shift+Enter on a database / schema opens its scoped New Query
    // (Enter reveals it in the sidebar, `secondary-enter` is the diagram).
    ShortcutDef::new(
        "palette.new_query",
        "Command Palette",
        "New Query for the Highlighted Database",
        &[Some("List")],
        &["shift-enter"],
        bind!(PaletteNewQuery),
    )
    .fixed(),
    ShortcutDef::new(
        "tab_picker.previous",
        "Tab Picker",
        "Previous Tab",
        &[Some("tab-picker > Input")],
        &["up"],
        bind!(TabPickerPrev),
    )
    .fixed(),
    ShortcutDef::new(
        "tab_picker.next",
        "Tab Picker",
        "Next Tab",
        &[Some("tab-picker > Input")],
        &["down"],
        bind!(TabPickerNext),
    )
    .fixed(),
    ShortcutDef::new(
        "mongo.run_filter",
        "Mongo Query Bar",
        "Run the Filter",
        &[Some("mongo-find > Input")],
        &["enter", "secondary-enter"],
        bind!(MongoFindEnter),
    )
    .fixed(),
    ShortcutDef::new(
        "mongo.run_sort",
        "Mongo Query Bar",
        "Apply the Sort",
        &[Some("mongo-sort > Input")],
        &["enter", "secondary-enter"],
        bind!(MongoSortEnter),
    )
    .fixed(),
    // Ending a transaction cannot be undone and every plain chord is taken (Ctrl+Alt is AltGr on
    // Windows layouts): these three are bound by the user, and shown in the tooltips and palette.
    ShortcutDef::new(
        "editor.commit",
        "Transactions",
        "Commit Transaction",
        ANY,
        &[],
        bind!(CommitTransaction),
    ),
    ShortcutDef::new(
        "editor.rollback",
        "Transactions",
        "Roll Back Transaction",
        ANY,
        &[],
        bind!(RollbackTransaction),
    ),
    ShortcutDef::new(
        "editor.toggle_auto_commit",
        "Transactions",
        "Toggle Auto-commit / Manual",
        ANY,
        &[],
        bind!(ToggleAutoCommit),
    ),
];

/// The actions of the running platform, in page order.
pub(super) fn all() -> impl Iterator<Item = &'static ShortcutDef> {
    DEFS.iter()
        .filter(|def| !def.macos_only || cfg!(target_os = "macos"))
}

pub(super) fn find(id: &str) -> Option<&'static ShortcutDef> {
    all().find(|def| def.id == id)
}

// ---- chords -----------------------------------------------------------------------

/// The canonical text of a keystroke: `secondary-` for the primary modifier,
/// then `ctrl-` (macOS) / `super-`, `alt-`, `shift-`, `fn-`, then the key.
fn canonical(stroke: &Keystroke, macos: bool) -> String {
    let m = &stroke.modifiers;
    let (secondary, control, platform) = if macos {
        (m.platform, m.control, false)
    } else {
        (m.control, false, m.platform)
    };
    let mut chord = String::new();
    for (on, prefix) in [
        (secondary, "secondary-"),
        (control, "ctrl-"),
        (platform, "super-"),
        (m.alt, "alt-"),
        (m.shift, "shift-"),
        (m.function, "fn-"),
    ] {
        if on {
            chord.push_str(prefix);
        }
    }
    chord.push_str(&stroke.key);
    chord
}

/// A chord string brought to its canonical form; `None` when gpui cannot
/// parse it or it holds several keystrokes.
pub(super) fn normalize(chord: &str) -> Option<String> {
    let stroke = parse_chord(chord).ok()?;
    is_known_key(&stroke.key).then(|| canonical(&stroke, cfg!(target_os = "macos")))
}

/// gpui's parser takes any word as a key; a hand-edited "not a chord" must
/// not count as one.
fn is_known_key(key: &str) -> bool {
    const NAMED: &[&str] = &[
        "enter",
        "escape",
        "tab",
        "space",
        "backspace",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "pageup",
        "pagedown",
        "insert",
    ];
    key.chars().count() == 1
        || NAMED.contains(&key)
        || key
            .strip_prefix('f')
            .and_then(|number| number.parse::<u8>().ok())
            .is_some_and(|number| (1..=24).contains(&number))
}

fn is_modifier_key(key: &str) -> bool {
    matches!(key, "shift" | "control" | "alt" | "platform" | "function")
}

/// Why a recorded keystroke cannot become a shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ChordError {
    /// A bare letter, digit or symbol would steal normal typing.
    NeedsModifier,
    /// Another action already uses the chord in an overlapping context.
    Conflict {
        id: &'static str,
        title: &'static str,
    },
}

impl std::fmt::Display for ChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsModifier => {
                f.write_str("Add Ctrl, Alt or Cmd to this key; a bare character is typed text")
            }
            Self::Conflict { title, .. } => write!(f, "Already used by \u{201C}{title}\u{201D}"),
        }
    }
}

impl std::error::Error for ChordError {}

/// What a keystroke the user pressed while recording means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Recorded {
    /// A lone modifier: keep waiting for the real key.
    Modifier,
    Cancel,
    Chord(String),
}

pub(super) fn record(stroke: &Keystroke) -> Result<Recorded, ChordError> {
    record_on(stroke, cfg!(target_os = "macos"))
}

fn record_on(stroke: &Keystroke, macos: bool) -> Result<Recorded, ChordError> {
    if is_modifier_key(&stroke.key) {
        return Ok(Recorded::Modifier);
    }
    let m = &stroke.modifiers;
    let bare = !(m.control || m.alt || m.platform || m.function);
    if bare && stroke.key == "escape" && !m.shift {
        return Ok(Recorded::Cancel);
    }
    if bare && stroke.key.chars().count() == 1 {
        return Err(ChordError::NeedsModifier);
    }
    Ok(Recorded::Chord(canonical(stroke, macos)))
}

// ---- conflicts ---------------------------------------------------------------------

fn context_path(context: &str) -> Vec<&str> {
    context.split('>').map(str::trim).collect()
}

/// Two key contexts overlap when a keystroke can reach both: either is global,
/// they are equal, or one is an ancestor of the other.
fn contexts_overlap(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return true;
    };
    let (a, b) = (context_path(a), context_path(b));
    a.starts_with(&b) || b.starts_with(&a)
}

fn defs_overlap(a: &ShortcutDef, b: &ShortcutDef) -> bool {
    if (a.scoped_override && b.is_global()) || (b.scoped_override && a.is_global()) {
        return false;
    }
    a.contexts
        .iter()
        .any(|a| b.contexts.iter().any(|b| contexts_overlap(*a, *b)))
}

/// The other action that would clash with `chord` on `def`.
pub(super) fn conflict(
    overrides: &Overrides,
    def: &ShortcutDef,
    chord: &str,
) -> Option<&'static ShortcutDef> {
    all()
        .filter(|other| other.id != def.id && defs_overlap(def, other))
        .find(|other| {
            other
                .effective(overrides)
                .iter()
                .any(|existing| existing == chord)
        })
}

/// Checks `chord` for `def` and adds it (or replaces slot `replace`) in the
/// overrides.
pub(super) fn assign(
    overrides: &mut Overrides,
    def: &ShortcutDef,
    chord: String,
    replace: Option<usize>,
) -> Result<(), ChordError> {
    if let Some(other) = conflict(overrides, def, &chord) {
        return Err(ChordError::Conflict {
            id: other.id,
            title: other.title,
        });
    }
    let mut chords = def.effective(overrides);
    match replace.filter(|slot| *slot < chords.len()) {
        Some(slot) => chords[slot] = chord,
        None if chords.contains(&chord) => return Ok(()),
        None => chords.push(chord),
    }
    chords.dedup();
    store(overrides, def, chords);
    Ok(())
}

/// Removes the chord in `slot`.
pub(super) fn remove(overrides: &mut Overrides, def: &ShortcutDef, slot: usize) {
    let mut chords = def.effective(overrides);
    if slot < chords.len() {
        chords.remove(slot);
        store(overrides, def, chords);
    }
}

pub(super) fn reset(overrides: &mut Overrides, def: &ShortcutDef) {
    overrides.remove(def.id);
}

/// Keeps the map canonical: chords equal to the defaults are no override.
fn store(overrides: &mut Overrides, def: &ShortcutDef, chords: Vec<String>) {
    if chords == def.default_chords() {
        overrides.remove(def.id);
    } else {
        overrides.insert(def.id.to_owned(), chords);
    }
}

// ---- display and search --------------------------------------------------------------

/// Splits a canonical chord into its modifiers and key.
fn split_chord(chord: &str) -> (Vec<&str>, &str) {
    let mut rest = chord;
    let mut modifiers = Vec::new();
    'scan: loop {
        for prefix in ["secondary", "ctrl", "super", "alt", "shift", "fn"] {
            if let Some(tail) = rest
                .strip_prefix(prefix)
                .and_then(|tail| tail.strip_prefix('-'))
            {
                if !tail.is_empty() {
                    modifiers.push(prefix);
                    rest = tail;
                    continue 'scan;
                }
            }
        }
        break;
    }
    (modifiers, rest)
}

/// The text the page shows for a chord: `⌘⇧K` on macOS, `Ctrl+Shift+K`
/// elsewhere.
pub(super) fn display(chord: &str, macos: bool) -> String {
    let (modifiers, key) = split_chord(chord);
    let has = |name: &str| modifiers.contains(&name);
    let key_text = |key: &str| -> String {
        let named = match (key, macos) {
            ("left", true) => "\u{2190}",
            ("right", true) => "\u{2192}",
            ("up", true) => "\u{2191}",
            ("down", true) => "\u{2193}",
            ("enter", true) => "\u{21B5}",
            ("escape", true) => "Esc",
            ("tab", true) => "\u{21E5}",
            ("backspace", true) => "\u{232B}",
            ("pagedown", _) => "PageDown",
            ("pageup", _) => "PageUp",
            _ => "",
        };
        if !named.is_empty() {
            return named.to_owned();
        }
        let mut chars = key.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => String::new(),
        }
    };
    if macos {
        let mut text = String::new();
        for (on, symbol) in [
            (has("ctrl"), "\u{2303}"),
            (has("alt"), "\u{2325}"),
            (has("secondary") || has("super"), "\u{2318}"),
            (has("shift"), "\u{21E7}"),
            (has("fn"), "fn "),
        ] {
            if on {
                text.push_str(symbol);
            }
        }
        text.push_str(&key_text(key));
        text
    } else {
        let super_name = if cfg!(target_os = "windows") {
            "Win"
        } else {
            "Super"
        };
        let mut parts: Vec<String> = Vec::new();
        for (on, name) in [
            (has("secondary") || has("ctrl"), "Ctrl"),
            (has("super"), super_name),
            (has("alt"), "Alt"),
            (has("shift"), "Shift"),
            (has("fn"), "Fn"),
        ] {
            if on {
                parts.push(name.to_owned());
            }
        }
        parts.push(key_text(key));
        parts.join("+")
    }
}

/// The first effective chord of `id`, as the running platform shows it
/// (`None` when unbound).
pub(super) fn label(overrides: &Overrides, id: &str) -> Option<String> {
    let chord = find(id)?.effective(overrides).into_iter().next()?;
    Some(display(&chord, cfg!(target_os = "macos")))
}

/// "Title (chord)" for tooltips: the title alone when the action is unbound.
pub(super) fn with_label(title: &str, overrides: &Overrides, id: &str) -> String {
    match label(overrides, id) {
        Some(label) => format!("{title} ({label})"),
        None => title.to_owned(),
    }
}

/// Case-insensitive match over title, category and the chords (as written
/// and as displayed).
pub(super) fn matches(def: &ShortcutDef, overrides: &Overrides, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let macos = cfg!(target_os = "macos");
    def.title.to_lowercase().contains(&query)
        || def.category.to_lowercase().contains(&query)
        || def.effective(overrides).iter().any(|chord| {
            chord.to_lowercase().contains(&query)
                || display(chord, macos).to_lowercase().contains(&query)
        })
}

// ---- binding ---------------------------------------------------------------------------

/// Set once the defaults are in the keymap, so the second entry point (the
/// menu setup) does not bind them again over the user's overrides.
struct DefaultsBound;

impl gpui::Global for DefaultsBound {}

/// Binds every default chord. Idempotent per app.
pub(super) fn bind_defaults(cx: &mut App) {
    if cx.has_global::<DefaultsBound>() {
        return;
    }
    cx.set_global(DefaultsBound);
    let app: &App = cx;
    let bindings: Vec<KeyBinding> = all()
        .flat_map(|def| {
            def.default_chords()
                .into_iter()
                .flat_map(move |chord| def.bindings(app, &chord))
        })
        .collect();
    cx.bind_keys(bindings);
}

/// Moves the keymap from the chords `old` produced to the ones `new` does.
/// Only the actions whose chords differ are touched.
pub(super) fn rebind(cx: &mut App, old: &Overrides, new: &Overrides) {
    let mut bindings: Vec<KeyBinding> = Vec::new();
    for def in all().filter(|def| def.editable) {
        let (before, after) = (def.effective(old), def.effective(new));
        if before == after {
            continue;
        }
        for chord in before.iter().filter(|chord| !after.contains(chord)) {
            bindings.extend(def.unbindings(cx, chord));
        }
        for chord in after.iter().filter(|chord| !before.contains(chord)) {
            bindings.extend(def.bindings(cx, chord));
        }
    }
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(source: &str) -> Keystroke {
        parse_chord(source).expect("keystroke")
    }

    #[test]
    fn ids_are_unique_and_titles_present() {
        let mut ids: Vec<_> = DEFS.iter().map(|def| def.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count);
        assert!(DEFS
            .iter()
            .all(|def| !def.title.is_empty() && !def.category.is_empty()));
    }

    /// Every primary shortcut must be spelled `secondary-` (Cmd on macOS, Ctrl
    /// elsewhere). A literal `cmd-` / `super-` / `win-` binding would be the
    /// Super key on Linux and Windows.
    #[test]
    fn default_chords_use_secondary_for_primary_shortcuts() {
        let mut secondary = 0;
        for def in DEFS {
            for chord in def.mac.iter().chain(def.other) {
                for needle in ["cmd-", "super-", "win-", "ctrl-"] {
                    assert!(
                        !chord.starts_with(needle) && !chord.contains(&format!("-{needle}")),
                        "{chord}"
                    );
                }
                secondary += usize::from(chord.starts_with("secondary-"));
            }
        }
        assert!(secondary >= 10);
    }

    #[test]
    fn pane_focus_chords_avoid_desktop_shortcuts_off_macos() {
        let pane = find("workspace.focus_pane_left").expect("def");
        assert_eq!(pane.mac, ["secondary-alt-left"]);
        assert_eq!(pane.other, ["alt-left"]);
        for def in DEFS
            .iter()
            .filter(|def| def.id.starts_with("workspace.focus_pane"))
        {
            let stroke = parse_chord(def.other[0]).expect("valid keystroke");
            assert!(stroke.modifiers.alt);
            assert!(
                !stroke.modifiers.control && !stroke.modifiers.platform && !stroke.modifiers.shift
            );
        }
    }

    #[test]
    fn every_default_parses_and_the_defaults_do_not_conflict_with_each_other() {
        let none = Overrides::new();
        // Bound by the user only: a chord with Alt would be AltGr on some Windows layouts.
        const UNBOUND_BY_DEFAULT: [&str; 4] =
            ["editor.explain_analyze", "editor.commit", "editor.rollback", "editor.toggle_auto_commit"];
        for def in all() {
            assert!(
                !def.default_chords().is_empty() || UNBOUND_BY_DEFAULT.contains(&def.id),
                "{} has no chord",
                def.id
            );
            for chord in def.default_chords() {
                assert_eq!(
                    conflict(&none, def, &chord).map(|other| other.id),
                    None,
                    "{} {chord}",
                    def.id
                );
            }
        }
    }

    #[test]
    fn contexts_overlap_by_ancestry_not_by_name() {
        assert!(contexts_overlap(None, Some("grid")));
        assert!(contexts_overlap(Some("grid"), Some("grid")));
        assert!(contexts_overlap(
            Some("grid-find"),
            Some("grid-find > Input")
        ));
        assert!(!contexts_overlap(Some("grid"), Some("sidebar")));
        assert!(!contexts_overlap(Some("grid"), Some("grid-find")));
    }

    #[test]
    fn the_same_chord_in_disjoint_contexts_is_not_a_conflict() {
        let grid = find("results.copy_selection").unwrap();
        let sidebar = find("sidebar.copy_connection_name").unwrap();
        let none = Overrides::new();
        assert_eq!(grid.effective(&none), sidebar.effective(&none));
        assert!(conflict(&none, grid, "secondary-c").is_none());
        // A global action cannot take it: it reaches both contexts.
        let save = find("editor.save_script").unwrap();
        assert_eq!(
            conflict(&none, save, "secondary-c").map(|d| d.id),
            Some("results.copy_selection")
        );
    }

    #[test]
    fn find_in_diagram_shares_the_chord_with_find_in_results_but_only_inside_the_diagram() {
        let none = Overrides::new();
        let diagram = find("diagram.find").unwrap();
        let results = find("results.find").unwrap();
        assert_eq!(diagram.effective(&none), results.effective(&none));
        assert!(
            conflict(&none, diagram, "secondary-f").is_none(),
            "the diagram context wins inside the diagram"
        );
        assert!(conflict(&none, results, "secondary-f").is_none());
        // The exemption is for that action only: another global action still
        // cannot take the chord, and neither can another diagram action.
        let save = find("editor.save_script").unwrap();
        assert_eq!(
            conflict(&none, save, "secondary-f").map(|d| d.id),
            Some("results.find")
        );
        let clear = find("diagram.clear").unwrap();
        assert!(conflict(&none, clear, "secondary-f").is_some());
        assert_eq!(diagram.contexts, [Some("diagram"), Some("diagram > Input")]);
    }

    #[test]
    fn diagram_undo_and_redo_live_in_the_diagram_context_and_redo_adds_ctrl_y_off_macos() {
        let none = Overrides::new();
        let (undo, redo) = (find("diagram.undo").unwrap(), find("diagram.redo").unwrap());
        assert_eq!(
            (undo.contexts, redo.contexts),
            (&[Some("diagram")][..], &[Some("diagram")][..])
        );
        assert_eq!(undo.defaults_for(true), ["secondary-z"]);
        assert_eq!(undo.defaults_for(false), ["secondary-z"]);
        assert_eq!(redo.defaults_for(true), ["secondary-shift-z"]);
        assert_eq!(
            redo.defaults_for(false),
            ["secondary-shift-z", "secondary-y"]
        );
        // Text fields keep the chord: their `Input` context is deeper than
        // `diagram`, and the registry has no global action on it to override.
        assert!(conflict(&none, undo, "secondary-z").is_none());
        assert!(conflict(&none, redo, "secondary-shift-z").is_none());
        // A global action may not take it: it would reach the diagram too.
        let save = find("editor.save_script").unwrap();
        assert_eq!(
            conflict(&none, save, "secondary-z").map(|d| d.id),
            Some("diagram.undo")
        );
        // Undo and redo cannot swap chords either.
        assert_eq!(
            conflict(&none, redo, "secondary-z").map(|d| d.id),
            Some("diagram.undo")
        );
    }

    #[test]
    fn assign_blocks_conflicts_and_keeps_the_map_canonical() {
        let mut overrides = Overrides::new();
        let save = find("editor.save_script").unwrap();
        let close = find("workspace.close_tab").unwrap();
        let error = assign(&mut overrides, save, "secondary-w".into(), Some(0)).unwrap_err();
        assert_eq!(
            error,
            ChordError::Conflict {
                id: "workspace.close_tab",
                title: "Close Tab"
            }
        );
        assert!(
            overrides.is_empty(),
            "a blocked change leaves the map alone"
        );
        assign(&mut overrides, save, "secondary-alt-s".into(), Some(0)).unwrap();
        assert_eq!(
            overrides.get("editor.save_script"),
            Some(&vec!["secondary-alt-s".to_owned()])
        );
        // The freed chord can be taken now.
        assign(&mut overrides, close, "secondary-s".into(), Some(0)).unwrap();
        // Back to the default: no override entry remains.
        assign(&mut overrides, close, "secondary-w".into(), Some(0)).unwrap();
        assert!(!overrides.contains_key("workspace.close_tab"));
        reset(&mut overrides, save);
        assert!(overrides.is_empty());
    }

    #[test]
    fn removing_the_last_chord_unbinds_and_an_unknown_or_unreadable_override_is_harmless() {
        let mut overrides = Overrides::new();
        let palette = find("workspace.open_command_palette").unwrap();
        remove(&mut overrides, palette, 0);
        assert_eq!(palette.effective(&overrides), ["secondary-p"]);
        remove(&mut overrides, palette, 0);
        assert!(palette.effective(&overrides).is_empty());
        assert!(palette.is_modified(&overrides));

        let mut hand_edited = Overrides::new();
        hand_edited.insert("no.such.action".into(), vec!["secondary-x".into()]);
        hand_edited.insert("workspace.close_tab".into(), vec!["not a chord".into()]);
        let close = find("workspace.close_tab").unwrap();
        assert_eq!(close.effective(&hand_edited), close.default_chords());
        assert!(find("no.such.action").is_none());
    }

    #[test]
    fn recording_ignores_modifiers_cancels_on_escape_and_wants_a_modifier_for_characters() {
        assert_eq!(record(&stroke("shift")), Ok(Recorded::Modifier));
        assert_eq!(record(&stroke("escape")), Ok(Recorded::Cancel));
        assert_eq!(record(&stroke("k")), Err(ChordError::NeedsModifier));
        assert_eq!(record(&stroke("shift-k")), Err(ChordError::NeedsModifier));
        assert_eq!(record(&stroke("f5")), Ok(Recorded::Chord("f5".into())));
        assert_eq!(
            record(&stroke("alt-escape")),
            Ok(Recorded::Chord("alt-escape".into()))
        );
        assert_eq!(
            record(&stroke("secondary-shift-k")),
            Ok(Recorded::Chord("secondary-shift-k".into()))
        );
    }

    #[test]
    fn recorded_chords_keep_secondary_semantics_per_platform() {
        let cmd = Keystroke::parse("cmd-shift-k").unwrap();
        assert_eq!(canonical(&cmd, true), "secondary-shift-k");
        // On macOS the control key is not the primary modifier.
        let ctrl = Keystroke::parse("ctrl-k").unwrap();
        assert_eq!(canonical(&ctrl, true), "ctrl-k");
        // Elsewhere control is primary and the Super key stays Super.
        assert_eq!(canonical(&ctrl, false), "secondary-k");
        let sup = Keystroke::parse("super-k").unwrap();
        assert_eq!(canonical(&sup, false), "super-k");
        assert_eq!(
            canonical(&Keystroke::parse("alt-left").unwrap(), false),
            "alt-left"
        );
    }

    #[test]
    fn chords_display_per_platform() {
        assert_eq!(display("secondary-shift-k", true), "\u{2318}\u{21E7}K");
        assert_eq!(display("secondary-shift-k", false), "Ctrl+Shift+K");
        assert_eq!(
            display("secondary-alt-left", true),
            "\u{2325}\u{2318}\u{2190}"
        );
        assert_eq!(display("alt-left", false), "Alt+Left");
        assert_eq!(
            display("secondary-shift-pagedown", false),
            "Ctrl+Shift+PageDown"
        );
        assert_eq!(display("secondary-,", false), "Ctrl+,");
        assert_eq!(display("secondary--", false), "Ctrl+-");
        assert_eq!(display("f5", false), "F5");
        assert_eq!(display("enter", true), "\u{21B5}");
        assert_eq!(display("ctrl-k", true), "\u{2303}K");
    }

    #[test]
    fn search_matches_title_category_and_chord_text() {
        let none = Overrides::new();
        let close = find("workspace.close_tab").unwrap();
        assert!(matches(close, &none, "close"));
        assert!(matches(close, &none, "TABS"));
        assert!(matches(close, &none, "secondary-w"));
        assert!(matches(
            close,
            &none,
            &display("secondary-w", cfg!(target_os = "macos"))
        ));
        assert!(!matches(close, &none, "refresh"));
        assert!(matches(close, &none, "  "));
    }
}
