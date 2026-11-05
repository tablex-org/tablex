use crate::connection_form::{decode_uri_component, ConnectionFormState};
use crate::connections::{self, ConnectionKind, SavedConnection};
use crate::navigator::NavigatorNodeId;
use crate::sessions::ConnectionStatus;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTabState {
    Missing,
    Unsupported,
    Disconnected,
    Connecting,
    PasswordRequired,
    Failed(String),
    Connected,
}

pub fn state_for(
    saved_exists: bool,
    supported: bool,
    status: ConnectionStatus,
) -> ConnectionTabState {
    if !saved_exists {
        return ConnectionTabState::Missing;
    }
    if !supported {
        return ConnectionTabState::Unsupported;
    }
    match status {
        ConnectionStatus::Disconnected => ConnectionTabState::Disconnected,
        ConnectionStatus::Connecting => ConnectionTabState::Connecting,
        ConnectionStatus::Connected => ConnectionTabState::Connected,
        ConnectionStatus::Failed(error)
            if crate::connections::is_missing_password_error(&error) =>
        {
            ConnectionTabState::PasswordRequired
        }
        ConnectionStatus::Failed(error) => ConnectionTabState::Failed(error),
    }
}

/// One `label  value` line of the Connection grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
}

impl InfoRow {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
        }
    }
}

/// What the Connection grid shows of `saved`: only rows that apply, read from
/// the password-free form of its URI (`public_connection_uri` drops the
/// password and every option that is not a known routing one), so nothing a
/// later line reads can carry a secret. SQLite shows its file; a MongoDB SRV
/// or multi-host URI shows its host list.
pub fn connection_info(saved: &SavedConnection) -> Vec<InfoRow> {
    let srv = saved.kind == ConnectionKind::Mongodb
        && saved
            .uri
            .get(..14)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("mongodb+srv://"));
    let kind = crate::connection_dialog::kind_label(saved.kind);
    let mut rows = vec![InfoRow::new(
        "Type",
        if srv { format!("{kind} (SRV)") } else { kind.to_string() },
    )];
    let Ok(public) = connections::public_connection_uri(&saved.uri) else {
        return rows;
    };
    let form = ConnectionFormState::from_saved(&SavedConnection {
        uri: public.clone(),
        ..saved.clone()
    });
    let (mut user, mut database) = (form.username.clone(), form.database.clone());
    if saved.kind == ConnectionKind::Sqlite {
        if !form.file_path.is_empty() {
            rows.push(InfoRow::new("File", form.file_path));
        }
        return rows;
    }
    if form.host.is_empty() {
        // The form leaves SRV and multi-host MongoDB URIs to the authority.
        let Some(authority) = Authority::parse(&public) else {
            return rows;
        };
        user = authority.user;
        database = authority.database;
        let label = if authority.hosts.len() > 1 { "Hosts" } else { "Host" };
        rows.push(InfoRow::new(label, authority.hosts.join(", ")));
    } else if form.port.is_empty() {
        rows.push(InfoRow::new("Host", form.host));
    } else {
        rows.push(InfoRow::new("Host", format!("{}:{}", form.host, form.port)));
    }
    if !user.is_empty() {
        rows.push(InfoRow::new("User", user));
    }
    if !database.is_empty() {
        rows.push(InfoRow::new("Database", database));
    }
    rows
}

/// The parts of a password-free URI authority that the form does not parse.
struct Authority {
    user: String,
    hosts: Vec<String>,
    database: String,
}

impl Authority {
    fn parse(uri: &str) -> Option<Self> {
        let rest = uri.split_once("://")?.1;
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (userinfo, hosts) = authority.rsplit_once('@').unwrap_or(("", authority));
        let user = userinfo.split(':').next().unwrap_or_default();
        let database = tail
            .strip_prefix('/')
            .and_then(|path| path.split(['?', '#']).next())
            .unwrap_or_default();
        Some(Self {
            user: decode_uri_component(user),
            hosts: hosts.split(',').map(str::to_owned).collect(),
            database: decode_uri_component(database),
        })
    }
}

/// `8.4.11` for the server version in MySQL's numeric form (`80411`) that
/// drivers report at connect.
pub fn server_version_text(version: u32) -> String {
    format!("{}.{}.{}", version / 10_000, version / 100 % 100, version % 100)
}

/// "PostgreSQL 16.2.0 · driver 0.3.1": the kind, then each part that is known.
pub fn overview_subtitle(
    kind: ConnectionKind,
    server_version: Option<u32>,
    driver_version: Option<&str>,
) -> String {
    let mut text = crate::connection_dialog::kind_label(kind).to_string();
    if let Some(version) = server_version {
        text.push(' ');
        text.push_str(&server_version_text(version));
    }
    if let Some(version) = driver_version.filter(|version| !version.is_empty()) {
        text.push_str(" \u{00B7} driver ");
        text.push_str(version);
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortOrder {
    pub fn toggled(self) -> Self {
        match self {
            Self::Asc => Self::Desc,
            Self::Desc => Self::Asc,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseEntry {
    pub id: NavigatorNodeId,
    pub name: String,
    /// Lower-cased once, so a filter pass allocates nothing.
    folded: String,
}

/// The databases under one connection as the overview lists them: read from
/// the navigator when its catalog changes, filtered and sorted only when the
/// query or the order changes. A frame that changed nothing does a revision
/// compare and two string compares.
#[derive(Debug, Default)]
pub struct DatabaseList {
    revision: Option<u64>,
    entries: Vec<DatabaseEntry>,
    query: String,
    needle: String,
    sort: SortOrder,
    /// Indexes into `entries`, in display order.
    shown: Vec<u32>,
}

impl DatabaseList {
    /// Brings the list up to date with the navigator `revision`, the search
    /// text and the order. `read` runs only when the revision moved. Returns
    /// whether anything changed.
    pub fn sync(
        &mut self,
        revision: u64,
        query: &str,
        sort: SortOrder,
        read: impl FnOnce() -> Vec<(NavigatorNodeId, String)>,
    ) -> bool {
        let reloaded = self.revision != Some(revision);
        if !reloaded && self.query == query && self.sort == sort {
            return false;
        }
        if reloaded {
            self.entries = read()
                .into_iter()
                .map(|(id, name)| DatabaseEntry {
                    folded: name.to_lowercase(),
                    id,
                    name,
                })
                .collect();
            self.revision = Some(revision);
        }
        if self.query != query {
            self.query = query.to_string();
            self.needle = query.trim().to_lowercase();
        }
        self.sort = sort;
        self.refilter();
        true
    }

    fn refilter(&mut self) {
        let Self {
            entries,
            needle,
            sort,
            shown,
            ..
        } = self;
        shown.clear();
        shown.extend(
            (0..entries.len() as u32)
                .filter(|&index| needle.is_empty() || entries[index as usize].folded.contains(needle.as_str())),
        );
        shown.sort_by(|&a, &b| {
            let (a, b) = (&entries[a as usize], &entries[b as usize]);
            let order = a.folded.cmp(&b.folded).then_with(|| a.name.cmp(&b.name));
            match sort {
                SortOrder::Asc => order,
                SortOrder::Desc => order.reverse(),
            }
        });
    }

    /// Every database, filtered or not.
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// The databases the search lets through.
    pub fn len(&self) -> usize {
        self.shown.len()
    }

    /// The `position`-th database in display order.
    pub fn get(&self, position: usize) -> Option<&DatabaseEntry> {
        self.entries.get(*self.shown.get(position)? as usize)
    }

    pub fn is_filtered(&self) -> bool {
        !self.needle.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_saved_connection_wins_over_runtime_status() {
        assert_eq!(
            state_for(false, true, ConnectionStatus::Connected),
            ConnectionTabState::Missing
        );
    }

    #[test]
    fn unsupported_saved_connection_is_not_connectable() {
        assert_eq!(
            state_for(true, false, ConnectionStatus::Disconnected),
            ConnectionTabState::Unsupported
        );
    }

    #[test]
    fn runtime_status_maps_to_connection_tab_state() {
        assert_eq!(
            state_for(true, true, ConnectionStatus::Connecting),
            ConnectionTabState::Connecting
        );
        assert_eq!(
            state_for(true, true, ConnectionStatus::Failed("bad auth".into())),
            ConnectionTabState::Failed("bad auth".into())
        );
        assert_eq!(
            state_for(true, true, ConnectionStatus::Connected),
            ConnectionTabState::Connected
        );
    }

    #[test]
    fn missing_password_maps_to_password_recovery_state() {
        assert_eq!(
            state_for(
                true,
                true,
                ConnectionStatus::Failed(crate::connections::missing_password_error())
            ),
            ConnectionTabState::PasswordRequired
        );
    }

    fn saved(kind: ConnectionKind, uri: &str) -> SavedConnection {
        SavedConnection {
            id: uuid::Uuid::new_v4(),
            name: "Fixture".into(),
            uri: uri.into(),
            kind,
            save_password: true,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }
    }

    fn info(kind: ConnectionKind, uri: &str) -> Vec<(&'static str, String)> {
        connection_info(&saved(kind, uri))
            .into_iter()
            .map(|row| (row.label, row.value))
            .collect()
    }

    #[test]
    fn a_sql_uri_shows_host_port_user_and_database_without_its_secrets() {
        let rows = info(
            ConnectionKind::Postgres,
            "postgres://app:hunter2@db.example.com:5433/shop?sslmode=require&password=hunter3&passfile=/secret",
        );
        assert_eq!(
            rows,
            [
                ("Type", "PostgreSQL".to_string()),
                ("Host", "db.example.com:5433".into()),
                ("User", "app".into()),
                ("Database", "shop".into()),
            ]
        );
        assert!(!format!("{rows:?}").contains("hunter"), "{rows:?}");
        // No port, no user, no database: only the rows that apply.
        assert_eq!(
            info(ConnectionKind::Mysql, "mysql://db.internal"),
            [("Type", "MySQL".to_string()), ("Host", "db.internal".into())]
        );
    }

    #[test]
    fn a_sqlite_uri_shows_the_file() {
        assert_eq!(
            info(ConnectionKind::Sqlite, "sqlite:///var/data/app file.db?mode=ro"),
            [
                ("Type", "SQLite".to_string()),
                ("File", "/var/data/app file.db".into())
            ]
        );
    }

    #[test]
    fn mongodb_srv_and_multi_host_uris_show_their_hosts() {
        let rows = info(
            ConnectionKind::Mongodb,
            "mongodb+srv://ana%40corp:hunter2@cluster0.abc.mongodb.net/shop?authSource=admin",
        );
        assert_eq!(
            rows,
            [
                ("Type", "MongoDB (SRV)".to_string()),
                ("Host", "cluster0.abc.mongodb.net".into()),
                ("User", "ana@corp".into()),
                ("Database", "shop".into()),
            ]
        );
        assert!(!format!("{rows:?}").contains("hunter"));
        assert_eq!(
            info(ConnectionKind::Mongodb, "mongodb://u:pw@a.example:27017,b.example:27018/?replicaSet=rs0"),
            [
                ("Type", "MongoDB".to_string()),
                ("Hosts", "a.example:27017, b.example:27018".into()),
                ("User", "u".into()),
            ]
        );
        assert_eq!(
            info(ConnectionKind::Mongodb, "mongodb://solo.example:27017/app"),
            [
                ("Type", "MongoDB".to_string()),
                ("Host", "solo.example:27017".into()),
                ("Database", "app".into()),
            ]
        );
    }

    #[test]
    fn an_unreadable_uri_still_shows_the_type() {
        assert_eq!(info(ConnectionKind::Postgres, "not a uri"), [("Type", "PostgreSQL".to_string())]);
    }

    #[test]
    fn the_subtitle_names_each_part_that_is_known() {
        assert_eq!(server_version_text(80_411), "8.4.11");
        assert_eq!(server_version_text(160_200), "16.2.0");
        assert_eq!(overview_subtitle(ConnectionKind::Postgres, None, None), "PostgreSQL");
        assert_eq!(
            overview_subtitle(ConnectionKind::Mariadb, Some(110_809), Some("0.3.1")),
            "MariaDB 11.8.9 \u{00B7} driver 0.3.1"
        );
        assert_eq!(
            overview_subtitle(ConnectionKind::Sqlite, None, Some("2.0.0")),
            "SQLite \u{00B7} driver 2.0.0"
        );
        assert_eq!(overview_subtitle(ConnectionKind::Sqlite, None, Some("")), "SQLite");
    }

    fn database(name: &str) -> (NavigatorNodeId, String) {
        (
            NavigatorNodeId::Database {
                connection: "c".into(),
                database: name.into(),
            },
            name.into(),
        )
    }

    fn shown(list: &DatabaseList) -> Vec<&str> {
        (0..list.len()).map(|position| list.get(position).unwrap().name.as_str()).collect()
    }

    #[test]
    fn the_database_list_reads_when_the_revision_moves_and_filters_and_sorts_when_asked() {
        let mut list = DatabaseList::default();
        let names = || ["beta", "Alpha", "gamma", "alphabet"].map(database).to_vec();
        assert!(list.sync(1, "", SortOrder::Asc, names));
        assert_eq!(shown(&list), ["Alpha", "alphabet", "beta", "gamma"], "case-insensitive A to Z");
        assert_eq!(list.total(), 4);

        // The same revision, query and order: nothing is read or recomputed.
        assert!(!list.sync(1, "", SortOrder::Asc, || panic!("read again")));

        assert!(list.sync(1, "ALPHA", SortOrder::Asc, || panic!("read again")));
        assert_eq!(shown(&list), ["Alpha", "alphabet"]);
        assert!(list.is_filtered());
        assert_eq!((list.len(), list.total()), (2, 4));

        assert!(list.sync(1, "ALPHA", SortOrder::Desc, || panic!("read again")));
        assert_eq!(shown(&list), ["alphabet", "Alpha"]);

        assert!(list.sync(1, "  ", SortOrder::Desc, || panic!("read again")));
        assert_eq!(shown(&list), ["gamma", "beta", "alphabet", "Alpha"], "blank query lets everything through");
        assert!(!list.is_filtered());

        // A new catalog revision reads again.
        assert!(list.sync(2, "  ", SortOrder::Desc, || vec![database("zeta")]));
        assert_eq!(shown(&list), ["zeta"]);
        assert_eq!(list.get(1), None);
    }

    #[test]
    fn a_thousand_databases_filter_by_substring() {
        let mut list = DatabaseList::default();
        list.sync(1, "db-12", SortOrder::Asc, || (0..2_000).map(|i| database(&format!("db-{i:04}"))).collect());
        assert_eq!(list.total(), 2_000);
        assert_eq!(list.len(), 100, "db-1200 to db-1299");
        list.sync(1, "db-001", SortOrder::Asc, || unreachable!());
        assert_eq!(list.len(), 10);
        assert_eq!(list.get(0).unwrap().name, "db-0010");
    }
}
