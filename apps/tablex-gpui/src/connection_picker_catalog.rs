use crate::connections::ConnectionKind;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerCategory {
    Relational,
    Analytics,
    Lightweight,
    DocsCacheSearch,
    GraphVector,
    TimeSeries,
    MessageQueues,
    RegistryConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PickerEntry {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
    pub(crate) category: PickerCategory,
    pub(crate) kind: Option<ConnectionKind>,
    pub(crate) monogram: &'static str,
}

impl PickerCategory {
    pub(crate) const fn all() -> [Self; 8] {
        [
            Self::Relational,
            Self::Analytics,
            Self::Lightweight,
            Self::DocsCacheSearch,
            Self::GraphVector,
            Self::TimeSeries,
            Self::MessageQueues,
            Self::RegistryConfig,
        ]
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Relational => "Relational",
            Self::Analytics => "Analytics",
            Self::Lightweight => "Lightweight",
            Self::DocsCacheSearch => "Docs/Cache/Search",
            Self::GraphVector => "Graph/Vector",
            Self::TimeSeries => "Time series",
            Self::MessageQueues => "Message queues",
            Self::RegistryConfig => "Registry/Config",
        }
    }
}

impl PickerCategory {
    /// True when the category lists at least one connection kind the app can
    /// open today; empty categories are hidden from the picker.
    pub(crate) fn has_available(self, drivers: &DriverAvailability) -> bool {
        available_entries(self, drivers).next().is_some()
    }
}

/// Where a kind's driver stands, for the picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DriverPresence {
    /// Usable now (the driver is installed).
    Ready,
    /// Not installed but in the driver catalog: shown with a "download" badge.
    Download,
    /// No driver at all: hidden.
    Missing,
}

/// What the picker knows about drivers. `Unknown` (the driver store has not
/// been read yet) lists every supported kind, as before drivers were
/// downloadable.
#[derive(Clone, Debug, Default)]
pub(crate) struct DriverAvailability {
    known: Option<(HashSet<String>, HashSet<String>)>,
}

impl DriverAvailability {
    /// `installed`: ids of installed, enabled drivers; `downloadable`: ids the
    /// catalog offers for install.
    pub(crate) fn known(installed: HashSet<String>, downloadable: HashSet<String>) -> Self {
        Self {
            known: Some((installed, downloadable)),
        }
    }

    fn presence(&self, kind: ConnectionKind) -> DriverPresence {
        let Some((installed, downloadable)) = &self.known else {
            return DriverPresence::Ready;
        };
        let id = kind.driver_id();
        if installed.contains(id) {
            DriverPresence::Ready
        } else if downloadable.contains(id) {
            DriverPresence::Download
        } else {
            DriverPresence::Missing
        }
    }
}

impl PickerEntry {
    /// A kind the app can open: supported, and its driver installed,
    /// downloadable, or not needed.
    pub(crate) fn is_available(self, drivers: &DriverAvailability) -> bool {
        self.kind.is_some_and(|kind| {
            kind.is_supported() && drivers.presence(kind) != DriverPresence::Missing
        })
    }

    pub(crate) fn needs_download(self, drivers: &DriverAvailability) -> bool {
        self.kind
            .is_some_and(|kind| drivers.presence(kind) == DriverPresence::Download)
    }

    /// Whether the app supports the kind at all (ignoring drivers).
    pub(crate) fn is_supported(self) -> bool {
        self.kind.is_some_and(|kind| kind.is_supported())
    }
}

const CATALOG: [PickerEntry; 20] = [
    PickerEntry {
        id: "mysql",
        label: "MySQL",
        category: PickerCategory::Relational,
        kind: Some(ConnectionKind::Mysql),
        monogram: "MY",
    },
    PickerEntry {
        id: "postgres",
        label: "PostgreSQL",
        category: PickerCategory::Relational,
        kind: Some(ConnectionKind::Postgres),
        monogram: "PG",
    },
    PickerEntry {
        id: "sql-server",
        label: "SQL Server",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "SQL",
    },
    PickerEntry {
        id: "oracle",
        label: "Oracle",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "OR",
    },
    PickerEntry {
        id: "mariadb",
        label: "MariaDB",
        category: PickerCategory::Relational,
        kind: Some(ConnectionKind::Mariadb),
        monogram: "MA",
    },
    PickerEntry {
        id: "firebird",
        label: "Firebird",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "FB",
    },
    PickerEntry {
        id: "cockroachdb",
        label: "CockroachDB",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "CR",
    },
    PickerEntry {
        id: "db2",
        label: "DB2",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "DB",
    },
    PickerEntry {
        id: "informix",
        label: "Informix",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "IF",
    },
    PickerEntry {
        id: "jdbcx",
        label: "JDBCX",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "JX",
    },
    PickerEntry {
        id: "cloud-spanner",
        label: "Cloud Spanner",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "CS",
    },
    PickerEntry {
        id: "iris",
        label: "IRIS",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "IR",
    },
    PickerEntry {
        id: "intersystems-cache",
        label: "InterSystems Caché",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "IC",
    },
    PickerEntry {
        id: "custom-mysql",
        label: "Custom (MySQL)",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "MY",
    },
    PickerEntry {
        id: "dolt",
        label: "Dolt",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "DO",
    },
    PickerEntry {
        id: "custom-postgresql",
        label: "Custom (PostgreSQL)",
        category: PickerCategory::Relational,
        kind: None,
        monogram: "PG",
    },
    PickerEntry {
        id: "clickhouse",
        label: "ClickHouse",
        category: PickerCategory::Analytics,
        kind: Some(ConnectionKind::Clickhouse),
        monogram: "CH",
    },
    PickerEntry {
        id: "sqlite",
        label: "SQLite",
        category: PickerCategory::Lightweight,
        kind: Some(ConnectionKind::Sqlite),
        monogram: "SQ",
    },
    PickerEntry {
        id: "mongodb",
        label: "MongoDB",
        category: PickerCategory::DocsCacheSearch,
        kind: Some(ConnectionKind::Mongodb),
        monogram: "MO",
    },
    PickerEntry {
        id: "redis",
        label: "Redis",
        category: PickerCategory::DocsCacheSearch,
        kind: Some(ConnectionKind::Redis),
        monogram: "RE",
    },
];

pub(crate) fn entries(category: PickerCategory) -> impl Iterator<Item = PickerEntry> {
    CATALOG
        .iter()
        .copied()
        .filter(move |entry| entry.category == category)
}

/// Entries of `category` the app can open today (the picker shows only these;
/// the full catalog keeps its order for when more kinds become supported).
pub(crate) fn available_entries<'a>(
    category: PickerCategory,
    drivers: &'a DriverAvailability,
) -> impl Iterator<Item = PickerEntry> + 'a {
    entries(category).filter(move |entry| entry.is_available(drivers))
}

#[cfg(test)]
pub(crate) fn all_entries() -> &'static [PickerEntry] {
    &CATALOG
}

pub(crate) fn logo_asset(id: &str) -> Option<&'static str> {
    match id {
        "mysql" | "custom-mysql" => Some("icons/mysql.svg"),
        "mariadb" => Some("icons/mariadb.svg"),
        "sql-server" => Some("icons/sql-server.svg"),
        "oracle" => Some("icons/oracle.svg"),
        "firebird" => Some("icons/firebird.svg"),
        "cockroachdb" => Some("icons/cockroachlabs.svg"),
        "cloud-spanner" => Some("icons/cloud-spanner.svg"),
        "db2" => Some("icons/db2.svg"),
        "informix" => Some("icons/informix.svg"),
        "iris" | "intersystems-cache" => Some("icons/iris.svg"),
        "dolt" => Some("icons/dolt.svg"),
        "custom-postgresql" => Some("icons/postgres.svg"),
        _ => None,
    }
}

pub(crate) fn entry_for_kind(kind: ConnectionKind) -> Option<PickerEntry> {
    CATALOG
        .iter()
        .copied()
        .find(|entry| entry.kind == Some(kind))
}

pub(crate) fn matches_query(entry: PickerEntry, query: &str) -> bool {
    query.is_empty() || entry.label.to_ascii_lowercase().contains(query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_keeps_relational_order_and_density() {
        let labels = entries(PickerCategory::Relational)
            .map(|entry| entry.label)
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 16);
        assert_eq!(
            labels,
            vec![
                "MySQL",
                "PostgreSQL",
                "SQL Server",
                "Oracle",
                "MariaDB",
                "Firebird",
                "CockroachDB",
                "DB2",
                "Informix",
                "JDBCX",
                "Cloud Spanner",
                "IRIS",
                "InterSystems Caché",
                "Custom (MySQL)",
                "Dolt",
                "Custom (PostgreSQL)",
            ]
        );
    }

    #[test]
    fn catalog_exposes_only_known_available_kinds() {
        assert!(entry_for_kind(ConnectionKind::Postgres)
            .expect("PostgreSQL should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Sqlite)
            .expect("SQLite should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Mongodb)
            .expect("MongoDB should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Mysql)
            .expect("MySQL should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Mariadb)
            .expect("MariaDB should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Clickhouse)
            .expect("ClickHouse should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(entry_for_kind(ConnectionKind::Redis)
            .expect("Redis should be catalogued")
            .is_available(&DriverAvailability::default()));
        assert!(!all_entries()
            .iter()
            .find(|entry| entry.id == "jdbcx")
            .expect("JDBCX should be catalogued")
            .is_available(&DriverAvailability::default()));
    }

    #[test]
    fn catalog_keeps_all_sidebar_categories_in_reference_order() {
        assert_eq!(
            PickerCategory::all()
                .into_iter()
                .map(PickerCategory::label)
                .collect::<Vec<_>>(),
            vec![
                "Relational",
                "Analytics",
                "Lightweight",
                "Docs/Cache/Search",
                "Graph/Vector",
                "Time series",
                "Message queues",
                "Registry/Config",
            ]
        );
    }

    fn ids(items: &[&str]) -> HashSet<String> {
        items.iter().map(|id| id.to_string()).collect()
    }

    #[test]
    fn download_only_kinds_follow_the_installed_set_and_the_driver_catalog() {
        let entry = |kind| entry_for_kind(kind).expect("catalogued");
        let (mysql, postgres) = (entry(ConnectionKind::Mysql), entry(ConnectionKind::Postgres));

        // Nothing known yet: every supported kind is listed, none badged.
        let unknown = DriverAvailability::default();
        assert!(mysql.is_available(&unknown) && !mysql.needs_download(&unknown));

        // Known, MySQL only downloadable: listed with the download badge.
        let catalog = DriverAvailability::known(ids(&[]), ids(&["mysql"]));
        assert!(mysql.is_available(&catalog) && mysql.needs_download(&catalog));
        // PostgreSQL needs its driver like every kind: not offered while it is
        // neither installed nor in the catalog.
        assert!(!postgres.is_available(&catalog));

        // Installed: listed, no badge.
        let installed = DriverAvailability::known(ids(&["mysql"]), ids(&[]));
        assert!(mysql.is_available(&installed) && !mysql.needs_download(&installed));

        // No driver at all (not installed, not in the catalog): hidden.
        let none = DriverAvailability::known(ids(&[]), ids(&[]));
        assert!(!mysql.is_available(&none), "a kind with no driver stays hidden");
        assert!(!postgres.is_available(&none));
        assert!(!PickerCategory::Relational.has_available(&none), "no driver for any kind of the category");
        assert!(!PickerCategory::DocsCacheSearch.has_available(&DriverAvailability::known(ids(&[]), ids(&[]))), "no driver for any kind of the category");
        assert!(available_entries(PickerCategory::Relational, &catalog).any(|e| e.id == "mysql"));
        assert!(!available_entries(PickerCategory::Relational, &none).any(|e| e.id == "mysql"));
    }
}
