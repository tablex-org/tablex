//! Pure palette index: flat, pre-folded entries grouped into segments.
//!
//! Every searchable string is folded (lower-cased, one char in -> one char
//! out) once when the segment is built, together with two cheap pre-filters:
//! a word-start bitmask for word-prefix / initials matching and a character
//! bloom mask that lets the ranker reject most entries with one AND. Nothing
//! here touches gpui, so the whole module is unit-testable and benchmarkable.

use super::super::tab_bar::TabIcon;
use super::commands::StaticAction;
use crate::navigator::NavigatorNodeId;
use std::path::PathBuf;

/// What an entry is; drives the icon, the badge, the result section and the
/// tie-break bias between otherwise equal matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    Tab,
    Action,
    Connection,
    Script,
    Pipeline,
    Database,
    Schema,
    Table,
    View,
    Collection,
}

impl Kind {
    /// Short badge shown at the right end of a row.
    pub(super) fn badge(self) -> &'static str {
        match self {
            Self::Tab => "Tab",
            Self::Action => "Action",
            Self::Connection => "Connection",
            Self::Script => "Script",
            Self::Pipeline => "Pipeline",
            Self::Database => "Database",
            Self::Schema => "Schema",
            Self::Table => "Table",
            Self::View => "View",
            Self::Collection => "Collection",
        }
    }

    /// Section heading of the results list.
    pub(super) fn section(self) -> &'static str {
        match self {
            Self::Tab => "Open Tabs",
            Self::Action => "Actions",
            Self::Connection => "Connections",
            Self::Script => "Saved Scripts",
            Self::Pipeline => "Saved Pipelines",
            Self::Database => "Databases",
            Self::Schema => "Schemas",
            Self::Table => "Tables",
            Self::View => "Views",
            Self::Collection => "Collections",
        }
    }

    /// Tie-break only (always below one tier step): at equal match quality,
    /// what is already open or one keystroke away wins over catalog objects.
    pub(super) fn bias(self) -> i32 {
        match self {
            Self::Tab => 6,
            Self::Action => 5,
            Self::Connection => 4,
            Self::Script | Self::Pipeline => 3,
            Self::Database | Self::Schema => 2,
            Self::Table | Self::View | Self::Collection => 0,
        }
    }
}

/// Owned, index-independent description of what activating an entry does, so
/// "Recent" survives index rebuilds. Resolved against live `AppRoot` state at
/// activation time (a stale target degrades to a no-op there).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Command {
    ActivateTab(u64),
    OpenConnection(String),
    /// Database / schema / table / view / Mongo collection or database
    /// navigator node: tables and collections open their tab, databases and
    /// schemas are revealed in the sidebar.
    Node(NavigatorNodeId),
    OpenSavedSql { connection: String, path: PathBuf },
    OpenSavedPipeline { connection: String, id: String },
    Run(StaticAction),
    /// Secondary action (⌘↵) on a database / schema entry: its ER diagram.
    Diagram(NavigatorNodeId),
    /// Secondary action (Shift+Enter) on a database / schema entry: a New
    /// Query scoped to the node.
    NewQuery(NavigatorNodeId),
}

/// Strings shared by many entries (one connection / database / schema).
pub(super) struct Scope {
    /// Secondary row text ("connection · database.schema").
    pub(super) detail: Box<str>,
    folded: Box<str>,
    word_starts: u64,
    mask: u64,
    connection: Box<str>,
    database: Box<str>,
    schema: Box<str>,
}

impl Scope {
    pub(super) fn folded(&self) -> &str {
        &self.folded
    }

    pub(super) fn word_starts(&self) -> u64 {
        self.word_starts
    }
}

pub(super) struct Entry {
    pub(super) kind: Kind,
    icon: u8,
    scope: u32,
    pub(super) title: Box<str>,
    /// Empty when `title` is already its own folded form (the common case:
    /// lower-case ASCII identifiers), which halves the catalog's allocations.
    folded: Box<str>,
    word_starts: u64,
    /// Characters of the folded title and of its scope's search text.
    mask: u64,
    /// `None` for SQL catalog entries, whose command is derived from
    /// `scope` + `title` so 50k objects do not each own four id strings.
    command: Option<Box<Command>>,
}

impl Entry {
    pub(super) fn folded(&self) -> &str {
        if self.folded.is_empty() {
            &self.title
        } else {
            &self.folded
        }
    }

    pub(super) fn word_starts(&self) -> u64 {
        self.word_starts
    }

    pub(super) fn scope_ix(&self) -> usize {
        self.scope as usize
    }

    pub(super) fn mask(&self) -> u64 {
        self.mask
    }
}

/// A contiguous block of entries that is rebuilt (or reused) as a unit.
#[derive(Default)]
pub(super) struct Segment {
    pub(super) entries: Vec<Entry>,
    scopes: Vec<Scope>,
    icons: Vec<TabIcon>,
}

impl Segment {
    pub(super) fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub(super) fn scope(&self, entry: &Entry) -> &Scope {
        &self.scopes[entry.scope as usize]
    }

    pub(super) fn icon(&self, entry: &Entry) -> TabIcon {
        self.icons[usize::from(entry.icon)]
    }

    pub(super) fn detail(&self, entry: &Entry) -> &str {
        &self.scope(entry).detail
    }

    /// What activating `entry` does.
    pub(super) fn command(&self, entry: &Entry) -> Command {
        if let Some(command) = &entry.command {
            return (**command).clone();
        }
        let scope = self.scope(entry);
        let connection = scope.connection.to_string();
        let title = entry.title.to_string();
        Command::Node(match entry.kind {
            Kind::Database => NavigatorNodeId::Database {
                connection,
                database: title,
            },
            Kind::Schema => NavigatorNodeId::Schema {
                connection,
                database: scope.database.to_string(),
                schema: title,
            },
            // Table / view (collections and the rest carry their own command).
            _ => NavigatorNodeId::Object {
                connection,
                database: scope.database.to_string(),
                schema: scope.schema.to_string(),
                object: title,
            },
        })
    }
}

/// The searchable snapshot the ranker walks: at most a handful of segments.
#[derive(Default)]
pub(super) struct Index {
    pub(super) segments: Vec<std::sync::Arc<Segment>>,
}

impl Index {
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.segments.iter().map(|segment| segment.entries.len()).sum()
    }
}

/// Incremental constructor for one [`Segment`].
#[derive(Default)]
pub(super) struct SegmentBuilder {
    segment: Segment,
}

impl SegmentBuilder {
    pub(super) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(super) fn with_capacity(entries: usize) -> Self {
        let mut builder = Self::default();
        builder.segment.entries.reserve(entries);
        builder
    }

    fn icon(&mut self, icon: TabIcon) -> u8 {
        let position = self
            .segment
            .icons
            .iter()
            .position(|known| *known == icon)
            .unwrap_or_else(|| {
                self.segment.icons.push(icon);
                self.segment.icons.len() - 1
            });
        u8::try_from(position).unwrap_or(0)
    }

    /// Registers search/display context. `search` is extra matched-only text
    /// (action keywords); `detail` is shown and matched.
    pub(super) fn scope(
        &mut self,
        detail: &str,
        search: &str,
        connection: &str,
        database: &str,
        schema: &str,
    ) -> u32 {
        // Catalog objects arrive grouped by schema: reuse the previous scope.
        if search.is_empty() {
            if let Some(last) = self.segment.scopes.last() {
                if &*last.detail == detail
                    && &*last.connection == connection
                    && &*last.database == database
                    && &*last.schema == schema
                {
                    return (self.segment.scopes.len() - 1) as u32;
                }
            }
        }
        let mut text = String::with_capacity(detail.len() + search.len() + 1);
        text.push_str(detail);
        if !search.is_empty() {
            text.push(' ');
            text.push_str(search);
        }
        let folded = Folded::new(&text);
        let id = self.segment.scopes.len() as u32;
        self.segment.scopes.push(Scope {
            detail: detail.into(),
            folded: folded.text(&text).into(),
            word_starts: folded.word_starts,
            mask: folded.mask,
            connection: connection.into(),
            database: database.into(),
            schema: schema.into(),
        });
        id
    }

    pub(super) fn push(
        &mut self,
        kind: Kind,
        icon: TabIcon,
        scope: u32,
        title: &str,
        command: Option<Command>,
    ) {
        let folded = Folded::new(title);
        let icon = self.icon(icon);
        let scope_mask = self.segment.scopes[scope as usize].mask;
        self.segment.entries.push(Entry {
            kind,
            icon,
            scope,
            title: title.into(),
            folded: folded.text.map(String::into_boxed_str).unwrap_or_default(),
            word_starts: folded.word_starts,
            mask: folded.mask | scope_mask,
            command: command.map(Box::new),
        });
    }

    pub(super) fn finish(self) -> Segment {
        self.segment
    }
}

/// Folded text plus the pre-filters derived from it.
pub(super) struct Folded {
    /// `None` when the text is already its own folded form (no allocation).
    text: Option<String>,
    /// Bit `i` = a word starts at folded byte offset `i` (first 64 bytes).
    pub(super) word_starts: u64,
    pub(super) mask: u64,
}

impl Folded {
    pub(super) fn new(text: &str) -> Self {
        let mut word_starts = 0u64;
        let mut mask = 0u64;
        let mut offset = 0usize;
        let mut changed = false;
        let mut previous: Option<char> = None;
        for original in text.chars() {
            let starts_word = original.is_alphanumeric()
                && previous.is_none_or(|prev| {
                    !prev.is_alphanumeric() || (prev.is_lowercase() && original.is_uppercase())
                });
            if starts_word && offset < 64 {
                word_starts |= 1 << offset;
            }
            let c = fold_char(original);
            changed |= c != original;
            mask |= char_bit(c);
            offset += c.len_utf8();
            previous = Some(original);
        }
        Self {
            text: changed.then(|| text.chars().map(fold_char).collect()),
            word_starts,
            mask,
        }
    }

    /// The folded text, borrowing `original` when nothing changed.
    pub(super) fn text<'a>(&'a self, original: &'a str) -> &'a str {
        self.text.as_deref().unwrap_or(original)
    }
}

/// Lower-case a char without ever changing the char count (multi-char
/// lowercase expansions keep the original), so folded char `i` is original
/// char `i`.
pub(super) fn fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// Bloom bit of a folded char: exact for `a-z0-9`, hashed otherwise.
pub(super) fn char_bit(c: char) -> u64 {
    let bit = match c {
        'a'..='z' => c as u32 - 'a' as u32,
        '0'..='9' => 26 + c as u32 - '0' as u32,
        other => 36 + other as u32 % 28,
    };
    1 << bit
}

/// Builds the icon-bearing entries of the static action list.
pub(super) fn actions_segment(
    shortcuts: &super::super::shortcuts::Overrides,
    asks: super::super::assistant::EditorAsks,
    explain: bool,
    sql_editor: bool,
) -> Segment {
    let mut builder = SegmentBuilder::new();
    for action in StaticAction::ALL {
        // The assistant's editor actions are listed where the active tab offers them.
        let offered = match action {
            StaticAction::ExplainQuery | StaticAction::OptimiseQuery => asks.explain,
            StaticAction::FixQuery => asks.fix,
            StaticAction::ExplainPlan | StaticAction::ExplainPlanAnalyze => explain,
            StaticAction::FormatSql
            | StaticAction::SaveSqlFileAs
            | StaticAction::CommitTransaction
            | StaticAction::RollbackTransaction
            | StaticAction::ToggleAutoCommit => sql_editor,
            _ => true,
        };
        if !offered {
            continue;
        }
        let scope = builder.scope(&action.detail(shortcuts), action.keywords(), "", "", "");
        builder.push(
            Kind::Action,
            action.icon(),
            scope,
            action.title(),
            Some(Command::Run(action)),
        );
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folded_text_keeps_char_count_and_marks_word_starts() {
        let original = "userAccounts_v2 İx";
        let folded = Folded::new(original);
        assert_eq!(folded.text(original).chars().count(), original.chars().count());
        assert_eq!(folded.text(original), "useraccounts_v2 İx");
        // u(0) A(4) v(13)? "userAccounts_v2" -> offsets 0, 4, 13
        let expected = (1u64 << 0) | (1 << 4) | (1 << 13) | (1 << 16);
        assert_eq!(folded.word_starts, expected);
    }

    #[test]
    fn lower_case_ascii_titles_share_their_folded_form() {
        let mut builder = SegmentBuilder::new();
        let scope = builder.scope("pg", "", "c", "", "");
        let plain = TabIcon::Plain("icons/table.svg");
        builder.push(Kind::Table, plain, scope, "orders", None);
        builder.push(Kind::Table, plain, scope, "OrderItems", None);
        let segment = builder.finish();
        assert!(segment.entries[0].folded.is_empty());
        assert_eq!(segment.entries[0].folded(), "orders");
        assert_eq!(segment.entries[1].folded(), "orderitems");
        // One icon / one scope are interned across entries.
        assert_eq!(segment.icons.len(), 1);
        assert_eq!(segment.scopes.len(), 1);
    }

    #[test]
    fn sql_catalog_commands_are_derived_from_scope_and_title() {
        let mut builder = SegmentBuilder::new();
        let plain = TabIcon::Plain("icons/table.svg");
        let scope = builder.scope("pg · app.public", "", "c1", "app", "public");
        builder.push(Kind::Table, plain, scope, "users", None);
        let segment = builder.finish();
        assert_eq!(
            segment.command(&segment.entries[0]),
            Command::Node(NavigatorNodeId::Object {
                connection: "c1".into(),
                database: "app".into(),
                schema: "public".into(),
                object: "users".into(),
            })
        );
    }
}
