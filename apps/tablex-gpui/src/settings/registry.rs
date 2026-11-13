//! The one place every setting is described. A `SettingDescriptor` carries the
//! id, section, texts, control kind, default and typed getter / setter; the
//! Settings page, its search, "Reset to default", "Reset section" and the
//! modified markers are all driven from this list, so a new setting is one
//! entry here (plus the code that reads it).

use super::{AiProvider, Choice, FormatterChoice, Settings, ThemeChoice};
use crate::sidebar_preferences::{ConnectionSort, TabGroupBy};
use engine_ai::Effort;
use engine_connect::profile::{self, AgentLevel};
use std::fmt;
use std::sync::OnceLock;

/// Descriptor-backed pages of the Settings tab, in nav order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Section {
    Appearance,
    Editor,
    Results,
    Query,
    Navigation,
    Ai,
}

impl Section {
    #[cfg(test)]
    pub const ALL: [Self; 6] = [
        Self::Appearance,
        Self::Editor,
        Self::Results,
        Self::Query,
        Self::Navigation,
        Self::Ai,
    ];

    /// Stable key (test selectors, anchors).
    pub fn key(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::Editor => "editor",
            Self::Results => "results",
            Self::Query => "query",
            Self::Navigation => "navigation",
            Self::Ai => "ai",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Editor => "Editor",
            Self::Results => "Results & Data",
            Self::Query => "Query & Connections",
            Self::Navigation => "Navigation",
            Self::Ai => "AI Assistant",
        }
    }
}

/// A value as the page edits it.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(f64),
    Text(String),
    /// The stored id of a [`Choice`] option.
    Choice(&'static str),
}

/// Which control edits a setting, with the limits that validate it.
#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Toggle,
    Number {
        min: f64,
        max: f64,
        step: f64,
    },
    Text {
        max_len: usize,
    },
    /// A font family picked from the installed fonts; empty = the default.
    Font,
    /// A model id picked from the provider's models (built in, plus the ones fetched); not empty.
    Model,
    Choice {
        options: &'static [(&'static str, &'static str)],
    },
}

/// Settings that are stored but whose behaviour is not wired yet stay out of
/// the page and the search until they work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Active,
    ComingSoon,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingError {
    WrongType {
        id: &'static str,
    },
    NotANumber {
        id: &'static str,
    },
    OutOfRange {
        id: &'static str,
        min: f64,
        max: f64,
    },
    NotWhole {
        id: &'static str,
    },
    TooLong {
        id: &'static str,
        max_len: usize,
    },
    Empty {
        id: &'static str,
    },
    UnknownOption {
        id: &'static str,
    },
}

impl fmt::Display for SettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongType { id } => write!(f, "{id}: wrong kind of value"),
            Self::NotANumber { .. } => f.write_str("Enter a number"),
            Self::OutOfRange { min, max, .. } => write!(f, "Enter a value from {min} to {max}"),
            Self::NotWhole { .. } => f.write_str("Enter a whole number"),
            Self::TooLong { max_len, .. } => write!(f, "At most {max_len} characters"),
            Self::Empty { .. } => f.write_str("Choose a model"),
            Self::UnknownOption { id } => write!(f, "{id}: unknown option"),
        }
    }
}

impl std::error::Error for SettingError {}

type Getter = Box<dyn Fn(&Settings) -> Value + Send + Sync>;
type Setter = Box<dyn Fn(&mut Settings, Value) + Send + Sync>;

/// How a setting kept once per AI provider reads its value for any provider (the getter and
/// setter of the descriptor act on the selected one).
struct ProviderScope {
    get: fn(&Settings, AiProvider) -> Value,
    default: fn(AiProvider) -> Value,
}

/// Why a setting cannot be changed with these settings; `None`: it can.
type Unavailable = fn(&Settings) -> Option<&'static str>;

pub struct SettingDescriptor {
    /// Stable dotted id (`results.null_text`); also the search anchor.
    pub id: &'static str,
    pub section: Section,
    /// Sub-heading inside the section.
    pub group: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub kind: Kind,
    pub default: Value,
    /// A per-connection override makes sense (S2).
    pub overridable: bool,
    pub availability: Availability,
    get: Getter,
    set: Setter,
    scope: Option<ProviderScope>,
    unavailable: Option<Unavailable>,
}

impl SettingDescriptor {
    pub fn get(&self, settings: &Settings) -> Value {
        (self.get)(settings)
    }

    /// Validates `value` against the kind's limits and stores it.
    pub fn set(&self, settings: &mut Settings, value: Value) -> Result<(), SettingError> {
        let value = match self.validate(value.clone()) {
            // A provider without a default model (the OpenAI wire's) may have none chosen: the id
            // is typed, and clearing the box says "no model" rather than refusing.
            Err(SettingError::Empty { .. }) if settings.ai.provider.default_model().is_empty() => {
                value
            }
            checked => checked?,
        };
        (self.set)(settings, value);
        Ok(())
    }

    /// Checks `value` without storing it (the page validates typed text).
    pub fn validate(&self, value: Value) -> Result<Value, SettingError> {
        let id = self.id;
        match (&self.kind, value) {
            (Kind::Toggle, value @ Value::Bool(_)) => Ok(value),
            (Kind::Number { min, max, step }, Value::Number(number)) => {
                if number.is_nan() {
                    return Err(SettingError::NotANumber { id });
                }
                if number < *min || number > *max {
                    return Err(SettingError::OutOfRange {
                        id,
                        min: *min,
                        max: *max,
                    });
                }
                if *step >= 1.0 && number.fract() != 0.0 {
                    return Err(SettingError::NotWhole { id });
                }
                Ok(Value::Number(number))
            }
            (Kind::Text { max_len }, Value::Text(text)) => {
                if text.chars().count() > *max_len {
                    Err(SettingError::TooLong {
                        id,
                        max_len: *max_len,
                    })
                } else {
                    Ok(Value::Text(text))
                }
            }
            (Kind::Font, Value::Text(text)) => {
                if text.chars().count() > 128 {
                    Err(SettingError::TooLong { id, max_len: 128 })
                } else {
                    Ok(Value::Text(text))
                }
            }
            (Kind::Model, Value::Text(text)) => {
                if text.trim().is_empty() {
                    Err(SettingError::Empty { id })
                } else if text.chars().count() > 128 {
                    Err(SettingError::TooLong { id, max_len: 128 })
                } else {
                    Ok(Value::Text(text))
                }
            }
            (Kind::Choice { options }, Value::Choice(choice)) => options
                .iter()
                .find(|(option, _)| *option == choice)
                .map(|(option, _)| Value::Choice(option))
                .ok_or(SettingError::UnknownOption { id }),
            _ => Err(SettingError::WrongType { id }),
        }
    }

    /// The default of this setting for the settings it is edited in: a setting kept per provider
    /// has the default of the selected provider.
    pub fn default_in(&self, settings: &Settings) -> Value {
        match &self.scope {
            Some(scope) => (scope.default)(settings.ai.provider),
            None => self.default.clone(),
        }
    }

    /// What `saved` holds for the row `draft` shows: for a setting kept per provider, the saved
    /// value of the provider the draft has selected (not of the one saved as selected).
    pub fn saved_value_for(&self, saved: &Settings, draft: &Settings) -> Value {
        match &self.scope {
            Some(scope) => (scope.get)(saved, draft.ai.provider),
            None => self.get(saved),
        }
    }

    /// Why the control is off with these settings, in one line; `None`: it works.
    pub fn unavailable_reason(&self, settings: &Settings) -> Option<&'static str> {
        self.unavailable.and_then(|reason| reason(settings))
    }

    #[cfg(test)]
    pub fn is_default(&self, settings: &Settings) -> bool {
        self.get(settings) == self.default_in(settings)
    }

    pub fn reset(&self, settings: &mut Settings) {
        let default = self.default_in(settings);
        (self.set)(settings, default);
    }

    pub fn is_visible(&self) -> bool {
        self.availability == Availability::Active
    }

    /// Case-insensitive match over title, description and section name.
    pub fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty()
            || self.title.to_lowercase().contains(&query)
            || self.description.to_lowercase().contains(&query)
            || self.section.title().to_lowercase().contains(&query)
            || self.group.to_lowercase().contains(&query)
    }

    pub fn overridable(mut self) -> Self {
        self.overridable = true;
        self
    }

    pub fn coming_soon(mut self) -> Self {
        self.availability = Availability::ComingSoon;
        self
    }

    /// The setting is kept once per AI provider.
    fn per_provider(
        mut self,
        get: fn(&Settings, AiProvider) -> Value,
        default: fn(AiProvider) -> Value,
    ) -> Self {
        self.scope = Some(ProviderScope { get, default });
        self
    }

    fn unavailable_when(mut self, reason: Unavailable) -> Self {
        self.unavailable = Some(reason);
        self
    }
}

/// Where a descriptor lives on the page.
#[derive(Clone, Copy)]
struct Place {
    id: &'static str,
    section: Section,
    group: &'static str,
    title: &'static str,
    description: &'static str,
}

fn build(
    place: Place,
    kind: Kind,
    get: Getter,
    set: Setter,
    settings: &Settings,
) -> SettingDescriptor {
    let default = get(settings);
    SettingDescriptor {
        id: place.id,
        section: place.section,
        group: place.group,
        title: place.title,
        description: place.description,
        kind,
        default,
        overridable: false,
        availability: Availability::Active,
        get,
        set,
        scope: None,
        unavailable: None,
    }
}

/// Number types a setting can store.
trait Numeric: Copy + Send + Sync + 'static {
    fn to_f64(self) -> f64;
    fn from_f64(value: f64) -> Self;
}

impl Numeric for u32 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(value: f64) -> Self {
        value as u32
    }
}

impl Numeric for f32 {
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    fn from_f64(value: f64) -> Self {
        value as f32
    }
}

fn toggle(
    place: Place,
    get: fn(&Settings) -> bool,
    set: fn(&mut Settings, bool),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Toggle,
        Box::new(move |s| Value::Bool(get(s))),
        Box::new(move |s, v| {
            if let Value::Bool(v) = v {
                set(s, v);
            }
        }),
        defaults,
    )
}

fn number<N: Numeric>(
    place: Place,
    (min, max, step): (f64, f64, f64),
    get: fn(&Settings) -> N,
    set: fn(&mut Settings, N),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Number { min, max, step },
        Box::new(move |s| Value::Number(get(s).to_f64())),
        Box::new(move |s, v| {
            if let Value::Number(v) = v {
                set(s, N::from_f64(v));
            }
        }),
        defaults,
    )
}

fn text(
    place: Place,
    max_len: usize,
    get: fn(&Settings) -> &str,
    set: fn(&mut Settings, String),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Text { max_len },
        Box::new(move |s| Value::Text(get(s).to_owned())),
        Box::new(move |s, v| {
            if let Value::Text(v) = v {
                set(s, v);
            }
        }),
        defaults,
    )
}

fn font(
    place: Place,
    get: fn(&Settings) -> &str,
    set: fn(&mut Settings, String),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Font,
        Box::new(move |s| Value::Text(get(s).to_owned())),
        Box::new(move |s, v| {
            if let Value::Text(v) = v {
                set(s, v);
            }
        }),
        defaults,
    )
}

fn model(
    place: Place,
    get: fn(&Settings) -> &str,
    set: fn(&mut Settings, String),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Model,
        Box::new(move |s| Value::Text(get(s).to_owned())),
        Box::new(move |s, v| {
            if let Value::Text(v) = v {
                set(s, v);
            }
        }),
        defaults,
    )
}

fn choice<C: Choice>(
    place: Place,
    get: fn(&Settings) -> C,
    set: fn(&mut Settings, C),
    defaults: &Settings,
) -> SettingDescriptor {
    build(
        place,
        Kind::Choice {
            options: C::OPTIONS,
        },
        Box::new(move |s| Value::Choice(get(s).id())),
        Box::new(move |s, v| {
            if let Some(v) = match v {
                Value::Choice(id) => C::from_id(id),
                _ => None,
            } {
                set(s, v);
            }
        }),
        defaults,
    )
}

macro_rules! place {
    ($id:literal, $section:ident, $group:literal, $title:literal, $description:literal) => {
        Place {
            id: $id,
            section: Section::$section,
            group: $group,
            title: $title,
            description: $description,
        }
    };
}

fn build_registry() -> Vec<SettingDescriptor> {
    let d = &Settings::default();
    vec![
        // ---- Appearance ----------------------------------------------------
        choice::<ThemeChoice>(
            place!("appearance.theme", Appearance, "Interface", "Theme", "Dark, Light, or follow the operating system's appearance and switch with it."),
            |s| s.appearance.theme,
            |s, v| s.appearance.theme = v,
            d,
        ),
        number::<f32>(
            place!("appearance.ui_scale", Appearance, "Interface", "UI scale", "Zoom of the whole interface."),
            (0.75, 2.0, 0.05),
            |s| s.appearance.ui_scale,
            |s, v| s.appearance.ui_scale = v,
            d,
        )
        .coming_soon(),
        font(
            place!("appearance.interface_font", Appearance, "Interface", "Interface font", "Font of menus, tabs, the sidebar and dialogs. Empty uses the system font."),
            |s| &s.appearance.interface_font,
            |s, v| s.appearance.interface_font = v,
            d,
        ),
        font(
            place!("appearance.grid_font", Appearance, "Result grid", "Grid font", "Font of result grid cells. Empty uses the interface font."),
            |s| &s.appearance.grid_font,
            |s, v| s.appearance.grid_font = v,
            d,
        ),
        number::<f32>(
            place!("appearance.grid_font_size", Appearance, "Result grid", "Grid font size", "Text size of result grid cells, in pixels."),
            (9.0, 20.0, 1.0),
            |s| s.appearance.grid_font_size,
            |s, v| s.appearance.grid_font_size = v,
            d,
        ),
        font(
            place!("appearance.editor_font", Appearance, "Editor", "Editor font", "Monospace font of SQL and script editors. Empty uses the platform default."),
            |s| &s.appearance.editor_font,
            |s, v| s.appearance.editor_font = v,
            d,
        ),
        number::<f32>(
            place!("appearance.editor_font_size", Appearance, "Editor", "Editor font size", "Text size of SQL and script editors, in pixels."),
            (9.0, 32.0, 1.0),
            |s| s.appearance.editor_font_size,
            |s, v| s.appearance.editor_font_size = v,
            d,
        ),
        choice::<TabGroupBy>(
            place!("appearance.tab_group_by", Appearance, "Tab bar", "Group tabs by", "Default grouping of the tab bar."),
            |s| s.appearance.tab_group_by,
            |s, v| s.appearance.tab_group_by = v,
            d,
        ),
        toggle(
            place!("appearance.tab_rows", Appearance, "Tab bar", "Show tabs in a second row", "Group chips on one row and the active group's tabs on a second. Needs grouping."),
            |s| s.appearance.tab_rows,
            |s, v| s.appearance.tab_rows = v,
            d,
        ),
        toggle(
            place!("appearance.compact_tab_titles", Appearance, "Tab bar", "Shorten tab titles", "Drop the @database suffix from tab labels."),
            |s| s.appearance.compact_tab_titles,
            |s, v| s.appearance.compact_tab_titles = v,
            d,
        ),
        toggle(
            place!("appearance.show_fps", Appearance, "Status bar", "Show FPS counter", "Debug metric: frames per second in the status bar."),
            |s| s.appearance.show_fps,
            |s, v| s.appearance.show_fps = v,
            d,
        ),
        toggle(
            place!("updates.check_automatically", Appearance, "Updates", "Check for updates automatically", "Once a day, when TableX starts, ask GitHub whether a newer version exists. Nothing but the request itself is sent. Check for Updates in the command palette works either way."),
            |s| s.updates.check_automatically,
            |s, v| s.updates.check_automatically = v,
            d,
        ),
        // ---- Editor ---------------------------------------------------------
        toggle(
            place!("editor.line_numbers", Editor, "Display", "Line numbers", "Show line numbers in the gutter."),
            |s| s.editor.line_numbers,
            |s, v| s.editor.line_numbers = v,
            d,
        ),
        toggle(
            place!("editor.soft_wrap", Editor, "Display", "Soft wrap", "Wrap long lines instead of scrolling horizontally."),
            |s| s.editor.soft_wrap,
            |s, v| s.editor.soft_wrap = v,
            d,
        ),
        number::<u32>(
            place!("editor.tab_size", Editor, "Display", "Tab size", "Width of an indent, in spaces."),
            (1.0, 16.0, 1.0),
            |s| s.editor.tab_size,
            |s, v| s.editor.tab_size = v,
            d,
        ),
        toggle(
            place!("editor.auto_close", Editor, "Typing", "Auto-close brackets and quotes", "Type an opening bracket or quote and get the closing one."),
            |s| s.editor.auto_close,
            |s, v| s.editor.auto_close = v,
            d,
        ),
        toggle(
            place!("editor.autocomplete_auto_trigger", Editor, "Autocomplete", "Open suggestions while typing", "Show the suggestion list while typing. Off disables the list."),
            |s| s.editor.autocomplete_auto_trigger,
            |s, v| s.editor.autocomplete_auto_trigger = v,
            d,
        ),
        toggle(
            place!("editor.autocomplete_insert_space", Editor, "Autocomplete", "Insert a space after keywords", "Accepting a keyword suggestion adds a trailing space."),
            |s| s.editor.autocomplete_insert_space,
            |s, v| s.editor.autocomplete_insert_space = v,
            d,
        ),
        number::<u32>(
            place!("editor.autocomplete_max_items", Editor, "Autocomplete", "Maximum suggestions", "Most entries the suggestion list shows."),
            (5.0, 50.0, 1.0),
            |s| s.editor.autocomplete_max_items,
            |s, v| s.editor.autocomplete_max_items = v,
            d,
        ),
        toggle(
            place!("editor.highlight_current_statement", Editor, "Running", "Highlight the current statement", "Tint the statement under the cursor."),
            |s| s.editor.highlight_current_statement,
            |s, v| s.editor.highlight_current_statement = v,
            d,
        )
        .coming_soon(),
        choice(
            place!("editor.run_scope", Editor, "Running", "Run executes", "What the Run button and the run shortcut execute. A selection is always preferred except for the whole-script choice."),
            |s| s.editor.run_scope,
            |s, v| s.editor.run_scope = v,
            d,
        ),
        toggle(
            place!("editor.confirm_dangerous", Editor, "Safety", "Confirm dangerous statements", "Ask before running statements a driver flags as dangerous (DROP, DELETE without WHERE, ...). Off runs them without asking.")
            ,
            |s| s.editor.confirm_dangerous,
            |s, v| s.editor.confirm_dangerous = v,
            d,
        )
        .overridable(),
        toggle(
            place!("editor.confirm_unsaved_close", Editor, "Safety", "Confirm closing unsaved scripts", "Ask before closing a query tab whose script was never saved."),
            |s| s.editor.confirm_unsaved_close,
            |s, v| s.editor.confirm_unsaved_close = v,
            d,
        ),
        choice(
            place!("editor.formatter", Editor, "Format SQL", "Formatter", "What formats SQL. The built-in formatter needs nothing. SQL Formatter (sqruff) is a separate program installed from the Driver Manager. An external command is a program of yours that reads SQL on its standard input and writes the formatted SQL on its standard output."),
            |s| s.editor.formatter,
            |s, v| s.editor.formatter = v,
            d,
        ),
        choice(
            place!("editor.format_keyword_case", Editor, "Format SQL", "Keyword case", "The case Format SQL gives reserved words (SELECT, FROM, WHERE, ...). Other words keep the case they were typed in."),
            |s| s.editor.format_keyword_case,
            |s, v| s.editor.format_keyword_case = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter == FormatterChoice::External).then_some("The external command sets the case itself.")),
        number::<u32>(
            place!("editor.format_indent", Editor, "Format SQL", "Indent width", "Spaces per indent level in formatted SQL. 0 uses the tab size."),
            (0.0, 16.0, 1.0),
            |s| s.editor.format_indent,
            |s, v| s.editor.format_indent = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter == FormatterChoice::External).then_some("The external command sets the indent itself.")),
        number::<u32>(
            place!("editor.format_lines_between_statements", Editor, "Format SQL", "Blank lines between statements", "Blank lines Format SQL leaves after each statement's semicolon."),
            (0.0, 5.0, 1.0),
            |s| s.editor.format_lines_between_statements,
            |s, v| s.editor.format_lines_between_statements = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter != FormatterChoice::BuiltIn).then_some("Only the built-in formatter uses this.")),
        text(
            place!("editor.format_external_program", Editor, "Format SQL", "External command", "The program for an external formatter: a name found on your PATH (sqruff, sqlfluff, pg_format, sql-formatter) or a full path."),
            512,
            |s| &s.editor.format_external_program,
            |s, v| s.editor.format_external_program = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter != FormatterChoice::External).then_some("Used when the formatter is an external command.")),
        text(
            place!("editor.format_external_args", Editor, "Format SQL", "External command arguments", "Arguments, separated by blanks (quote one that contains blanks). {dialect} becomes postgres, mysql, mariadb, sqlite, clickhouse or ansi; {language} the name sql-formatter uses (postgresql, sql, ...)."),
            1024,
            |s| &s.editor.format_external_args,
            |s, v| s.editor.format_external_args = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter != FormatterChoice::External).then_some("Used when the formatter is an external command.")),
        text(
            place!("editor.format_external_exit_codes", Editor, "Format SQL", "External command exit codes that succeed", "Exit codes, separated by commas, that mean the command worked. A command that prints its result and exits 1 when it could not fix everything (sqruff fix, sqlfluff) needs 0,1. Anything else is a failure and its error output is shown."),
            32,
            |s| &s.editor.format_external_exit_codes,
            |s, v| s.editor.format_external_exit_codes = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter != FormatterChoice::External).then_some("Used when the formatter is an external command.")),
        number::<u32>(
            place!("editor.format_timeout", Editor, "Format SQL", "Formatter timeout", "Seconds SQL Formatter (sqruff) or an external command may take before it is stopped."),
            (1.0, 600.0, 1.0),
            |s| s.editor.format_timeout,
            |s, v| s.editor.format_timeout = v,
            d,
        )
        .unavailable_when(|s| (s.editor.formatter == FormatterChoice::BuiltIn).then_some("The built-in formatter needs no process.")),
        // ---- Results & Data --------------------------------------------------
        number::<u32>(
            place!("results.page_size", Results, "Fetching", "Default page size", "Rows fetched per page in a new tab."),
            profile::PAGE_SIZE_LIMITS,
            |s| s.results.page_size,
            |s, v| s.results.page_size = v,
            d,
        )
        .overridable(),
        text(
            place!("results.null_text", Results, "Cells", "NULL display text", "What a NULL value shows in the grid."),
            32,
            |s| &s.results.null_text,
            |s, v| s.results.null_text = v,
            d,
        ),
        toggle(
            place!("results.right_align_numbers", Results, "Cells", "Right-align numbers", "Align numeric columns to the right."),
            |s| s.results.right_align_numbers,
            |s, v| s.results.right_align_numbers = v,
            d,
        ),
        toggle(
            place!("results.show_row_numbers", Results, "Cells", "Show row numbers", "Show the row number gutter."),
            |s| s.results.show_row_numbers,
            |s, v| s.results.show_row_numbers = v,
            d,
        ),
        number::<u32>(
            place!("results.column_width_default", Results, "Columns", "Default column width", "Initial width of a column, in pixels."),
            (40.0, 1200.0, 1.0),
            |s| s.results.column_width_default,
            |s, v| s.results.column_width_default = v,
            d,
        ),
        number::<u32>(
            place!("results.column_width_min", Results, "Columns", "Minimum column width", "Smallest width a column can be resized to, in pixels."),
            (20.0, 1200.0, 1.0),
            |s| s.results.column_width_min,
            |s, v| s.results.column_width_min = v,
            d,
        ),
        number::<u32>(
            place!("results.column_width_max", Results, "Columns", "Maximum column width", "Largest width a column can be resized to, in pixels."),
            (40.0, 2400.0, 1.0),
            |s| s.results.column_width_max,
            |s, v| s.results.column_width_max = v,
            d,
        ),
        toggle(
            place!("results.auto_size_columns", Results, "Columns", "Size columns from their values", "Fit each column to a sample of its values (within the minimum and maximum) instead of the default width."),
            |s| s.results.auto_size_columns,
            |s, v| s.results.auto_size_columns = v,
            d,
        ),
        choice(
            place!("results.boolean_display", Results, "Formats", "Booleans", "Show boolean values as text or as a checkbox."),
            |s| s.results.boolean_display,
            |s, v| s.results.boolean_display = v,
            d,
        )
        .coming_soon(),
        choice(
            place!("results.datetime_format", Results, "Formats", "Date and time format", "How date-time values are written."),
            |s| s.results.datetime_format,
            |s, v| s.results.datetime_format = v,
            d,
        )
        .coming_soon(),
        choice(
            place!("results.binary_display", Results, "Formats", "Binary values", "Show binary values as hexadecimal or Base64."),
            |s| s.results.binary_display,
            |s, v| s.results.binary_display = v,
            d,
        )
        .coming_soon(),
        number::<u32>(
            place!("results.binary_preview_max", Results, "Formats", "Binary preview length", "Most bytes of a binary value shown in a cell."),
            (8.0, 4096.0, 1.0),
            |s| s.results.binary_preview_max,
            |s, v| s.results.binary_preview_max = v,
            d,
        )
        .coming_soon(),
        choice(
            place!("results.mongo_json_mode", Results, "Formats", "MongoDB JSON mode", "Relaxed or canonical extended JSON for documents."),
            |s| s.results.mongo_json_mode,
            |s, v| s.results.mongo_json_mode = v,
            d,
        )
        .coming_soon(),
        choice(
            place!("results.default_copy_format", Results, "Copy and export", "Default copy format", "Format used by the plain Copy command."),
            |s| s.results.default_copy_format,
            |s, v| s.results.default_copy_format = v,
            d,
        ),
        text(
            place!("results.export_delimiter", Results, "Copy and export", "Export delimiter", "Column separator of CSV exports."),
            8,
            |s| &s.results.export.delimiter,
            |s, v| s.results.export.delimiter = v,
            d,
        ),
        toggle(
            place!("results.export_header", Results, "Copy and export", "Export header row", "Write column names as the first row of CSV exports."),
            |s| s.results.export.header,
            |s, v| s.results.export.header = v,
            d,
        ),
        text(
            place!("results.export_null_text", Results, "Copy and export", "Export NULL text", "What a NULL is written as in CSV exports."),
            32,
            |s| &s.results.export.null_text,
            |s, v| s.results.export.null_text = v,
            d,
        ),
        text(
            place!("results.export_folder", Results, "Copy and export", "Export folder", "Where exports are saved. Empty uses the downloads folder."),
            1024,
            |s| &s.results.export.folder,
            |s, v| s.results.export.folder = v,
            d,
        ),
        // ---- Query & Connections ---------------------------------------------
        number::<u32>(
            place!("query.query_timeout_secs", Query, "Timeouts", "Query timeout", "Seconds a query may run before it is cancelled. 0 means no limit.")
            ,
            profile::QUERY_TIMEOUT_SECS_LIMITS,
            |s| s.query.query_timeout_secs,
            |s, v| s.query.query_timeout_secs = v,
            d,
        )
        .overridable(),
        number::<u32>(
            place!("query.connect_timeout_secs", Query, "Timeouts", "Connect timeout", "Seconds to wait for a connection. 0 means the driver default."),
            profile::CONNECT_TIMEOUT_SECS_LIMITS,
            |s| s.query.connect_timeout_secs,
            |s, v| s.query.connect_timeout_secs = v,
            d,
        )
        .overridable(),
        number::<u32>(
            place!("query.keep_alive_secs", Query, "Timeouts", "Connection keep-alive interval", "Seconds an open connection may sit idle before TableX checks it is still alive, so one that died (a server restart, a night asleep) is noticed and reopened on its next use. 0 means never check.")
            ,
            profile::KEEP_ALIVE_SECS_LIMITS,
            |s| s.query.keep_alive_secs,
            |s, v| s.query.keep_alive_secs = v,
            d,
        )
        .overridable(),
        text(
            place!("query.pg_dump_path", Query, "Backup tools", "pg_dump", "The pg_dump program to back up PostgreSQL with. Empty looks for it on your PATH and in the usual install folders and takes the newest version that is not older than the server. A path set here is used as it is."),
            1024,
            |s| &s.query.pg_dump_path,
            |s, v| s.query.pg_dump_path = v,
            d,
        ),
        text(
            place!("query.pg_restore_path", Query, "Backup tools", "pg_restore", "The pg_restore program to restore PostgreSQL archives with. Empty looks for it like pg_dump."),
            1024,
            |s| &s.query.pg_restore_path,
            |s, v| s.query.pg_restore_path = v,
            d,
        ),
        text(
            place!("query.mysqldump_path", Query, "Backup tools", "mysqldump", "The mysqldump program to back up MySQL with (and MariaDB, when it has no mariadb-dump). Empty looks for it on your PATH and in the usual install folders."),
            1024,
            |s| &s.query.mysqldump_path,
            |s, v| s.query.mysqldump_path = v,
            d,
        ),
        text(
            place!("query.mariadb_dump_path", Query, "Backup tools", "mariadb-dump", "The mariadb-dump program to back up MariaDB with. Empty looks for it on your PATH and in the usual install folders."),
            1024,
            |s| &s.query.mariadb_dump_path,
            |s, v| s.query.mariadb_dump_path = v,
            d,
        ),
        // ---- Navigation --------------------------------------------------------
        choice::<ConnectionSort>(
            place!("navigation.connection_sort", Navigation, "Sidebar", "Sort connections", "Order of connections in the sidebar."),
            |s| s.navigation.connection_sort,
            |s, v| s.navigation.connection_sort = v,
            d,
        ),
        choice(
            place!("editor.on_script_error", Query, "Scripts", "On error in a script", "What a script of several statements does when one fails: stop there (the rest show as skipped in Statistics) or run the remaining statements."),
            |s| s.editor.on_script_error,
            |s, v| s.editor.on_script_error = v,
            d,
        ),
        number::<u32>(
            place!("editor.max_result_tabs", Query, "Scripts", "Result tabs per script", "Most result tabs a script run keeps. Statements past it still run and appear in Statistics as rows not kept."),
            (1.0, 100.0, 1.0),
            |s| s.editor.max_result_tabs,
            |s, v| s.editor.max_result_tabs = v,
            d,
        ),
        toggle(
            place!("editor.parameters", Query, "Scripts", "Ask for parameters", "Ask for a value before running a statement that has :name or ${name} parameters. Off runs the text as written."),
            |s| s.editor.parameters,
            |s, v| s.editor.parameters = v,
            d,
        ),
        toggle(
            place!("navigation.global_search", Navigation, "Sidebar", "Search unloaded catalogs", "Sidebar search also crawls catalogs that were never expanded."),
            |s| s.navigation.global_search,
            |s, v| s.navigation.global_search = v,
            d,
        ),
        toggle(
            place!("navigation.show_system_objects", Navigation, "Sidebar", "Show system objects", "Default for connections without their own visible-objects filter."),
            |s| s.navigation.show_system_objects,
            |s, v| s.navigation.show_system_objects = v,
            d,
        ),
        number::<u32>(
            place!("navigation.navigator_page_size", Navigation, "Sidebar", "Navigator page size", "Objects loaded per \"load more\" in the sidebar tree."),
            (10.0, 5000.0, 1.0),
            |s| s.navigation.navigator_page_size,
            |s, v| s.navigation.navigator_page_size = v,
            d,
        ),
        choice(
            place!("navigation.table_double_click", Navigation, "Opening", "Double-click a table", "What double-clicking a table or view opens."),
            |s| s.navigation.table_double_click,
            |s, v| s.navigation.table_double_click = v,
            d,
        ),
        toggle(
            place!("navigation.single_click_opens", Navigation, "Opening", "Single click opens", "A single click on a table opens it instead of only selecting it."),
            |s| s.navigation.single_click_opens,
            |s, v| s.navigation.single_click_opens = v,
            d,
        ),
        toggle(
            place!("navigation.open_data_tabs_next_to_active", Navigation, "Opening", "Open data tabs next to the active tab", "New data tabs open beside the active tab instead of at the end."),
            |s| s.navigation.open_data_tabs_next_to_active,
            |s, v| s.navigation.open_data_tabs_next_to_active = v,
            d,
        ),
        choice(
            place!("navigation.restore_tabs", Navigation, "Opening", "Restore tabs on start", "Which tabs come back after restarting TableX."),
            |s| s.navigation.restore_tabs,
            |s, v| s.navigation.restore_tabs = v,
            d,
        ),
        // ---- AI Assistant ------------------------------------------------------
        choice::<AiProvider>(
            place!("ai.provider", Ai, "Model", "Provider", "Who answers the assistant's questions. The API key, the model and the address below are kept for each provider."),
            |s| s.ai.provider,
            |s, v| s.ai.provider = v,
            d,
        ),
        model(
            place!("ai.model", Ai, "Model", "Model", "The model the assistant uses. Refresh loads the provider's current list."),
            |s| &s.ai.selected().model,
            |s, v| s.ai.selected_mut().model = v,
            d,
        )
        .per_provider(
            |s, provider| Value::Text(s.ai.providers.entry(provider).model.clone()),
            |provider| Value::Text(provider.default_model().to_owned()),
        ),
        choice::<Effort>(
            place!("ai.effort", Ai, "Model", "Reasoning effort", "How much the model thinks before it answers: low is quick, max is thorough and slower. A change applies from the next new chat."),
            |s| s.ai.effort,
            |s, v| s.ai.effort = v,
            d,
        ),
        toggle(
            place!("ai.fallback", Ai, "Model", "Retry a declined request on a fallback model", "When the model declines a request, let Anthropic answer it with a fallback model instead. Only Anthropic has this."),
            |s| s.ai.fallback,
            |s, v| s.ai.fallback = v,
            d,
        )
        .unavailable_when(|s| {
            (s.ai.provider != AiProvider::Anthropic)
                .then_some("Not available: only Anthropic can retry on a fallback model.")
        }),
        toggle(
            place!("ai.keep_conversations", Ai, "Conversations", "Keep conversations on this computer", "Each finished conversation is saved as a file in TableX's data folder (assistant/conversations), so History can reopen it. The files can contain query results. Turning this off stops saving; it does not delete what is saved."),
            |s| s.ai.keep_conversations,
            |s, v| s.ai.keep_conversations = v,
            d,
        ),
        choice::<AgentLevel>(
            place!("ai.agent_access", Ai, "Agent access", "Default agent access", "How far the assistant and MCP clients may go on a connection that sets no level itself. Production connections stay off; writes always need Read and write on the connection."),
            |s| s.ai.agent_access,
            |s, v| s.ai.agent_access = v,
            d,
        ),
        toggle(
            place!("ai.mcp_writes", Ai, "MCP server", "Let MCP clients run changes", "An MCP client (the program you connected TableX's MCP server to) may then ask to run statements that change data on connections whose assistant access is \"Read and write\". TableX asks that client to put each one to you; a client set to answer by itself can run them without you seeing them, so turn this on only for a client you trust. Never on a connection of a production type (only the built-in Production type counts; a custom type is not production). Off by default."),
            |s| s.ai.mcp_writes,
            |s, v| s.ai.mcp_writes = v,
            d,
        ),
        text(
            place!("ai.base_url", Ai, "Advanced", "Base URL", "The provider's address. Use https://; http:// only for a server on this machine, such as Ollama or LM Studio."),
            256,
            |s| &s.ai.selected().base_url,
            |s, v| s.ai.selected_mut().base_url = v,
            d,
        )
        .per_provider(
            |s, provider| Value::Text(s.ai.providers.entry(provider).base_url.clone()),
            |provider| Value::Text(provider.default_base_url().to_owned()),
        ),
    ]
}

/// Every setting, in page order.
pub fn registry() -> &'static [SettingDescriptor] {
    static REGISTRY: OnceLock<Vec<SettingDescriptor>> = OnceLock::new();
    REGISTRY.get_or_init(build_registry)
}

pub fn find(id: &str) -> Option<&'static SettingDescriptor> {
    registry().iter().find(|descriptor| descriptor.id == id)
}

/// The settings shown on the page of `section`.
pub fn visible(section: Section) -> impl Iterator<Item = &'static SettingDescriptor> {
    registry()
        .iter()
        .filter(move |descriptor| descriptor.section == section && descriptor.is_visible())
}

/// Visible settings matching `query`, in page order.
pub fn search(query: &str) -> Vec<&'static SettingDescriptor> {
    registry()
        .iter()
        .filter(|descriptor| descriptor.is_visible() && descriptor.matches(query))
        .collect()
}

/// Ids whose value differs between two snapshots.
pub fn changed_ids(a: &Settings, b: &Settings) -> Vec<&'static str> {
    registry()
        .iter()
        .filter(|descriptor| descriptor.get(a) != descriptor.get(b))
        .map(|descriptor| descriptor.id)
        .collect()
}

/// Puts every visible setting of `section` back to its default.
pub fn reset_section(settings: &mut Settings, section: Section) {
    for descriptor in visible(section) {
        descriptor.reset(settings);
    }
}

/// Puts every visible setting back to its default.
pub fn reset_all(settings: &mut Settings) {
    for descriptor in registry()
        .iter()
        .filter(|descriptor| descriptor.is_visible())
    {
        descriptor.reset(settings);
    }
}

/// Rules that span several settings; `(setting id, message)` per violation.
/// The page shows them inline and keeps Apply disabled until none is left.
pub fn cross_field_errors(settings: &Settings) -> Vec<(&'static str, &'static str)> {
    let results = &settings.results;
    let mut errors = Vec::new();
    if results.column_width_min > results.column_width_max {
        errors.push((
            "results.column_width_min",
            "Must not exceed the maximum width",
        ));
    }
    if results.column_width_default < results.column_width_min
        || results.column_width_default > results.column_width_max
    {
        errors.push((
            "results.column_width_default",
            "Must lie between the minimum and maximum width",
        ));
    }
    if settings.results.export.delimiter.is_empty() {
        errors.push(("results.export_delimiter", "Enter a delimiter"));
    }
    if let Some(problem) =
        crate::ai::base_url_problem(settings.ai.provider, &settings.ai.selected().base_url)
    {
        errors.push(("ai.base_url", problem));
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_sections_are_dotted_prefixes() {
        let mut seen = HashSet::new();
        for descriptor in registry() {
            assert!(seen.insert(descriptor.id), "duplicate id {}", descriptor.id);
            assert!(
                descriptor.id.contains('.'),
                "{} is not dotted",
                descriptor.id
            );
            assert!(!descriptor.title.is_empty() && !descriptor.description.is_empty());
        }
    }

    #[test]
    fn every_descriptor_has_a_default_getter_and_setter() {
        let defaults = Settings::default();
        for descriptor in registry() {
            // Default equals what the model's Default produces, validates, and
            // survives a set / get round trip.
            assert_eq!(
                descriptor.get(&defaults),
                descriptor.default,
                "{}",
                descriptor.id
            );
            assert_eq!(
                descriptor.validate(descriptor.default.clone()).as_ref(),
                Ok(&descriptor.default),
                "{} default fails its own validation",
                descriptor.id
            );
            let mut settings = Settings::default();
            descriptor
                .set(&mut settings, descriptor.default.clone())
                .unwrap();
            assert_eq!(
                settings, defaults,
                "{}: setting the default changed something",
                descriptor.id
            );
            assert!(descriptor.is_default(&settings));
        }
    }

    /// A value different from the default that the kind accepts.
    fn other_value(descriptor: &SettingDescriptor) -> Value {
        match (&descriptor.kind, &descriptor.default) {
            (Kind::Toggle, Value::Bool(value)) => Value::Bool(!value),
            (Kind::Number { min, max, .. }, Value::Number(value)) => {
                Value::Number(if (value - min).abs() > f64::EPSILON {
                    *min
                } else {
                    *max
                })
            }
            (Kind::Text { .. }, Value::Text(value))
            | (Kind::Font, Value::Text(value))
            | (Kind::Model, Value::Text(value)) => Value::Text(format!("{value}x")),
            (Kind::Choice { options }, Value::Choice(value)) => Value::Choice(
                options
                    .iter()
                    .map(|(id, _)| *id)
                    .find(|id| id != value)
                    .unwrap(),
            ),
            other => panic!(
                "{}: default does not fit its kind: {other:?}",
                descriptor.id
            ),
        }
    }

    #[test]
    fn every_descriptor_edits_exactly_its_own_field() {
        for descriptor in registry() {
            let mut settings = Settings::default();
            let other = other_value(descriptor);
            descriptor.set(&mut settings, other.clone()).unwrap();
            assert_eq!(descriptor.get(&settings), other, "{}", descriptor.id);
            // The provider choice moves the rows of the selected provider's entry with it.
            let expected = if descriptor.id == "ai.provider" {
                vec!["ai.provider", "ai.model", "ai.base_url"]
            } else {
                vec![descriptor.id]
            };
            assert_eq!(
                changed_ids(&Settings::default(), &settings),
                expected,
                "{} touched another setting",
                descriptor.id
            );
            assert!(!descriptor.is_default(&settings));
            descriptor.reset(&mut settings);
            assert_eq!(settings, Settings::default(), "{}", descriptor.id);
        }
    }

    #[test]
    fn the_model_and_address_rows_follow_the_selected_provider() {
        let model = find("ai.model").unwrap();
        let base_url = find("ai.base_url").unwrap();
        let text = |value: &str| Value::Text(value.to_owned());
        let mut settings = Settings::default();
        model.set(&mut settings, text("claude-haiku-5-5")).unwrap();

        // Another provider shows its own entry, and its own defaults.
        settings.ai.provider = AiProvider::Ollama;
        assert_eq!(model.get(&settings), text(""));
        assert_eq!(model.default_in(&settings), text(""));
        assert!(model.is_default(&settings));
        assert_eq!(base_url.get(&settings), text("http://localhost:11434/v1"));
        assert!(base_url.is_default(&settings));
        model.set(&mut settings, text("qwen3:8b")).unwrap();
        base_url
            .set(&mut settings, text("http://localhost:9999/v1"))
            .unwrap();
        assert_eq!(settings.ai.providers.ollama.model, "qwen3:8b");
        assert_eq!(settings.ai.providers.anthropic.model, "claude-haiku-5-5");
        assert_eq!(
            settings.ai.providers.anthropic.base_url,
            "https://api.anthropic.com"
        );

        // A row is "modified" against what is saved for the draft's provider, not the saved one's.
        let saved = Settings::default();
        assert_eq!(model.saved_value_for(&saved, &settings), text(""));
        assert_eq!(
            base_url.saved_value_for(&saved, &settings),
            text("http://localhost:11434/v1")
        );
        let mut anthropic_draft = settings.clone();
        anthropic_draft.ai.provider = AiProvider::Anthropic;
        assert_eq!(
            model.saved_value_for(&saved, &anthropic_draft),
            model.get(&saved)
        );

        // Reset puts back the selected provider's default and leaves the others alone.
        model.reset(&mut settings);
        base_url.reset(&mut settings);
        assert_eq!(
            settings.ai.providers.ollama,
            ProviderEntry::defaults(AiProvider::Ollama)
        );
        assert_eq!(settings.ai.providers.anthropic.model, "claude-haiku-5-5");
        // A compatible server has no default address: the user must give one, and Apply waits.
        settings.ai.provider = AiProvider::OpenAiCompatible;
        assert_eq!(base_url.default_in(&settings), text(""));
        assert_eq!(
            cross_field_errors(&settings)
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            ["ai.base_url"]
        );
        base_url
            .set(&mut settings, text("http://localhost:1234/v1"))
            .unwrap();
        assert!(cross_field_errors(&settings).is_empty());
        // A model id is typed for these providers, and blank means none chosen yet ...
        model.set(&mut settings, text("my-model")).unwrap();
        model.set(&mut settings, text("")).unwrap();
        assert_eq!(settings.ai.providers.openai_compatible.model, "");
        // ... while Anthropic's is never blank.
        settings.ai.provider = AiProvider::Anthropic;
        assert!(matches!(
            model.set(&mut settings, text("  ")),
            Err(SettingError::Empty { .. })
        ));
    }

    #[test]
    fn the_fallback_row_is_off_for_every_provider_but_anthropic() {
        let fallback = find("ai.fallback").unwrap();
        let mut settings = Settings::default();
        assert_eq!(fallback.unavailable_reason(&settings), None);
        for provider in AiProvider::ALL
            .into_iter()
            .filter(|p| *p != AiProvider::Anthropic)
        {
            settings.ai.provider = provider;
            let reason = fallback.unavailable_reason(&settings).expect("off");
            assert!(
                reason.contains("Anthropic") && !reason.contains('\n'),
                "{reason}"
            );
            // The stored choice is left as it was.
            assert!(settings.ai.fallback);
        }
        assert!(find("ai.model")
            .unwrap()
            .unavailable_reason(&settings)
            .is_none());
    }

    #[test]
    fn validation_rejects_out_of_range_and_wrong_values() {
        let size = find("editor.tab_size").unwrap();
        let mut settings = Settings::default();
        assert!(matches!(
            size.set(&mut settings, Value::Number(0.0)),
            Err(SettingError::OutOfRange { .. })
        ));
        assert!(matches!(
            size.set(&mut settings, Value::Number(99.0)),
            Err(SettingError::OutOfRange { .. })
        ));
        assert!(matches!(
            size.set(&mut settings, Value::Number(2.5)),
            Err(SettingError::NotWhole { .. })
        ));
        assert!(matches!(
            size.set(&mut settings, Value::Number(f64::NAN)),
            Err(SettingError::NotANumber { .. })
        ));
        assert!(matches!(
            size.set(&mut settings, Value::Bool(true)),
            Err(SettingError::WrongType { .. })
        ));
        assert_eq!(
            settings.editor.tab_size, 2,
            "a rejected value changes nothing"
        );
        size.set(&mut settings, Value::Number(8.0)).unwrap();
        assert_eq!(settings.editor.tab_size, 8);

        let scope = find("editor.run_scope").unwrap();
        assert!(matches!(
            scope.set(&mut settings, Value::Choice("bogus")),
            Err(SettingError::UnknownOption { .. })
        ));
        let null_text = find("results.null_text").unwrap();
        assert!(matches!(
            null_text.set(&mut settings, Value::Text("x".repeat(33))),
            Err(SettingError::TooLong { .. })
        ));
    }

    #[test]
    fn search_is_case_insensitive_over_title_description_and_section() {
        let titles = |query: &str| search(query).iter().map(|d| d.id).collect::<Vec<_>>();
        assert!(titles("NULL display").contains(&"results.null_text"));
        assert!(titles("null display").contains(&"results.null_text"));
        // Description text.
        assert!(titles("wrap long lines").contains(&"editor.soft_wrap"));
        // Section name matches every setting of the section.
        let navigation = titles("navigation");
        assert!(navigation.contains(&"navigation.restore_tabs"));
        assert!(navigation.iter().all(|id| find(id).is_some()));
        assert!(titles("zzz-nothing").is_empty());
        assert_eq!(
            search("").len(),
            registry().iter().filter(|d| d.is_visible()).count()
        );
    }

    #[test]
    fn coming_soon_settings_are_hidden_from_pages_and_search() {
        assert!(!search("scale")
            .iter()
            .any(|d| d.id == "appearance.ui_scale"));
        assert!(!Section::ALL
            .iter()
            .flat_map(|section| visible(*section))
            .any(|d| d.availability == Availability::ComingSoon));
        // Hidden settings still exist for storage and migration.
        assert!(find("appearance.ui_scale").is_some());
    }

    #[test]
    fn reset_section_only_touches_that_section() {
        let mut settings = Settings::default();
        settings.editor.soft_wrap = true;
        settings.results.null_text = "-".into();
        reset_section(&mut settings, Section::Editor);
        assert!(!settings.editor.soft_wrap);
        assert_eq!(settings.results.null_text, "-");
        reset_all(&mut settings);
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn cross_field_rules_flag_inverted_column_widths() {
        let mut settings = Settings::default();
        assert!(cross_field_errors(&settings).is_empty());
        settings.results.column_width_min = 500;
        let errors = cross_field_errors(&settings);
        assert!(errors
            .iter()
            .any(|(id, _)| *id == "results.column_width_min"));
        assert!(errors
            .iter()
            .any(|(id, _)| *id == "results.column_width_default"));
    }

    #[test]
    fn overridable_settings_are_the_connection_scoped_ones() {
        let ids: Vec<_> = registry()
            .iter()
            .filter(|d| d.overridable)
            .map(|d| d.id)
            .collect();
        assert!(ids.contains(&"query.query_timeout_secs"));
        assert!(ids.contains(&"results.page_size"));
        assert!(!ids.contains(&"appearance.grid_font"));
    }
}
