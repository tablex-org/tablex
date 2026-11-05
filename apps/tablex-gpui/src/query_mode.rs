//! Typed `Tab::query_mode`.
//!
//! The persisted representation is the camelCase string the old app wrote
//! (`"mongoFind"`, `"sqlTable"`, ...); [`QueryMode`] serializes to exactly
//! those strings, and a value this build does not know survives a load/save
//! round trip as [`QueryMode::Unknown`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum QueryMode {
    /// MongoDB collection browse tab (filter / sort / projection bar).
    MongoFind,
    /// MongoDB JavaScript script tab.
    MongoScript,
    /// MongoDB aggregation pipeline tab.
    MongoAggregate,
    /// SQL table-data tab (no editor, WHERE / ORDER BY bar).
    SqlTable,
    /// Legacy PostgreSQL SQL-editor tab that carries a table identity.
    PostgresTable,
    /// Legacy SQLite SQL-editor tab that carries a table identity.
    SqliteTable,
    /// Redis key tab: the typed value of one key (`collection` holds the
    /// base64 key, `database` the db index).
    RedisKey,
    /// Redis command console tab.
    RedisCommand,
    /// Any other persisted value, kept verbatim.
    Unknown(String),
}

impl QueryMode {
    pub fn as_str(&self) -> &str {
        match self {
            Self::MongoFind => "mongoFind",
            Self::MongoScript => "mongoScript",
            Self::MongoAggregate => "mongoAggregate",
            Self::SqlTable => "sqlTable",
            Self::PostgresTable => "postgresTable",
            Self::SqliteTable => "sqliteTable",
            Self::RedisKey => "redisKey",
            Self::RedisCommand => "redisCommand",
            Self::Unknown(value) => value,
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "mongoFind" => Self::MongoFind,
            "mongoScript" => Self::MongoScript,
            "mongoAggregate" => Self::MongoAggregate,
            "sqlTable" => Self::SqlTable,
            "postgresTable" => Self::PostgresTable,
            "sqliteTable" => Self::SqliteTable,
            "redisKey" => Self::RedisKey,
            "redisCommand" => Self::RedisCommand,
            other => Self::Unknown(other.to_owned()),
        }
    }

    pub fn is_mongo_find(&self) -> bool {
        matches!(self, Self::MongoFind)
    }

    pub fn is_mongo_script(&self) -> bool {
        matches!(self, Self::MongoScript)
    }

    pub fn is_mongo_aggregate(&self) -> bool {
        matches!(self, Self::MongoAggregate)
    }

    /// Any MongoDB tab whose results page through the Mongo page cap
    /// (find, script and aggregate).
    pub fn is_mongo(&self) -> bool {
        matches!(
            self,
            Self::MongoFind | Self::MongoScript | Self::MongoAggregate
        )
    }

    /// Legacy SQLite SQL-editor tab that carries a table identity.
    pub fn is_sqlite_table(&self) -> bool {
        matches!(self, Self::SqliteTable)
    }

    /// Any mode whose result grid is a live view of one SQL table (row editing,
    /// copy-as-SQL and database sort apply). `PostgresTable` / `SqliteTable` are
    /// the legacy SQL-editor modes that also carry a table identity.
    pub fn is_sql_table(&self) -> bool {
        matches!(
            self,
            Self::SqlTable | Self::PostgresTable | Self::SqliteTable
        )
    }

    /// Table-data tab (no editor, WHERE / ORDER BY bar).
    pub fn is_table_data(&self) -> bool {
        matches!(self, Self::SqlTable)
    }

    /// Value tab of one Redis key.
    pub fn is_redis_key(&self) -> bool {
        matches!(self, Self::RedisKey)
    }

    /// Redis command console.
    pub fn is_redis_command(&self) -> bool {
        matches!(self, Self::RedisCommand)
    }

    /// Any Redis tab.
    pub fn is_redis(&self) -> bool {
        matches!(self, Self::RedisKey | Self::RedisCommand)
    }
}

impl From<&str> for QueryMode {
    fn from(value: &str) -> Self {
        Self::parse(value)
    }
}

impl Serialize for QueryMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for QueryMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|value| match Self::parse(&value) {
            Self::Unknown(_) => Self::Unknown(value),
            known => known,
        })
    }
}

/// Predicates on a tab's optional mode (`None` matches nothing).
pub trait QueryModeExt {
    fn is_mongo_find(&self) -> bool;
    fn is_mongo_script(&self) -> bool;
    fn is_mongo_aggregate(&self) -> bool;
    fn is_mongo(&self) -> bool;
    fn is_sqlite_table(&self) -> bool;
    fn is_sql_table(&self) -> bool;
    fn is_table_data(&self) -> bool;
    fn is_redis_key(&self) -> bool;
    fn is_redis_command(&self) -> bool;
    fn is_redis(&self) -> bool;
}

impl QueryModeExt for Option<QueryMode> {
    fn is_mongo_find(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_mongo_find)
    }
    fn is_mongo_script(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_mongo_script)
    }
    fn is_mongo_aggregate(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_mongo_aggregate)
    }
    fn is_mongo(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_mongo)
    }
    fn is_sqlite_table(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_sqlite_table)
    }
    fn is_sql_table(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_sql_table)
    }
    fn is_table_data(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_table_data)
    }
    fn is_redis_key(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_redis_key)
    }
    fn is_redis_command(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_redis_command)
    }
    fn is_redis(&self) -> bool {
        self.as_ref().is_some_and(QueryMode::is_redis)
    }
}

impl QueryModeExt for Option<&QueryMode> {
    fn is_mongo_find(&self) -> bool {
        self.is_some_and(QueryMode::is_mongo_find)
    }
    fn is_mongo_script(&self) -> bool {
        self.is_some_and(QueryMode::is_mongo_script)
    }
    fn is_mongo_aggregate(&self) -> bool {
        self.is_some_and(QueryMode::is_mongo_aggregate)
    }
    fn is_mongo(&self) -> bool {
        self.is_some_and(QueryMode::is_mongo)
    }
    fn is_sqlite_table(&self) -> bool {
        self.is_some_and(QueryMode::is_sqlite_table)
    }
    fn is_sql_table(&self) -> bool {
        self.is_some_and(QueryMode::is_sql_table)
    }
    fn is_table_data(&self) -> bool {
        self.is_some_and(QueryMode::is_table_data)
    }
    fn is_redis_key(&self) -> bool {
        self.is_some_and(QueryMode::is_redis_key)
    }
    fn is_redis_command(&self) -> bool {
        self.is_some_and(QueryMode::is_redis_command)
    }
    fn is_redis(&self) -> bool {
        self.is_some_and(QueryMode::is_redis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERSISTED: [(&str, QueryMode); 8] = [
        ("mongoFind", QueryMode::MongoFind),
        ("mongoScript", QueryMode::MongoScript),
        ("mongoAggregate", QueryMode::MongoAggregate),
        ("sqlTable", QueryMode::SqlTable),
        ("postgresTable", QueryMode::PostgresTable),
        ("sqliteTable", QueryMode::SqliteTable),
        ("redisKey", QueryMode::RedisKey),
        ("redisCommand", QueryMode::RedisCommand),
    ];

    #[test]
    fn persisted_strings_round_trip_exactly() {
        for (text, mode) in PERSISTED {
            assert_eq!(mode.as_str(), text);
            assert_eq!(QueryMode::parse(text), mode);
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<QueryMode>(&json).unwrap(), mode);
        }
    }

    #[test]
    fn unknown_values_survive_serde_verbatim() {
        for text in ["postgresSql", "", "MongoFind", "sqlTable "] {
            let mode = QueryMode::parse(text);
            assert_eq!(mode, QueryMode::Unknown(text.to_owned()));
            assert_eq!(mode.as_str(), text);
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(serde_json::from_str::<QueryMode>(&json).unwrap(), mode);
            assert_eq!(json, serde_json::to_string(text).unwrap());
        }
        let none: Option<QueryMode> = serde_json::from_str("null").unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn predicates_match_the_old_string_checks() {
        for (_, mode) in PERSISTED
            .iter()
            .chain([&("x", QueryMode::Unknown("x".into()))])
        {
            assert_eq!(mode.is_mongo_find(), *mode == QueryMode::MongoFind);
            assert_eq!(mode.is_mongo_script(), *mode == QueryMode::MongoScript);
            assert_eq!(
                mode.is_mongo_aggregate(),
                *mode == QueryMode::MongoAggregate
            );
            assert_eq!(
                mode.is_sql_table(),
                matches!(mode.as_str(), "postgresTable" | "sqliteTable" | "sqlTable")
            );
            assert_eq!(mode.is_table_data(), mode.as_str() == "sqlTable");
            assert_eq!(
                mode.is_mongo(),
                matches!(
                    mode.as_str(),
                    "mongoFind" | "mongoScript" | "mongoAggregate"
                )
            );
            assert_eq!(mode.is_sqlite_table(), mode.as_str() == "sqliteTable");
            assert_eq!(mode.is_redis_key(), mode.as_str() == "redisKey");
            assert_eq!(mode.is_redis_command(), mode.as_str() == "redisCommand");
            assert_eq!(mode.is_redis(), mode.as_str().starts_with("redis"));
        }
        let none: Option<QueryMode> = None;
        assert!(!none.is_mongo_find() && !none.is_sql_table() && !none.is_table_data());
        assert!(Some(QueryMode::MongoFind).is_mongo_find());
        assert!(Some(&QueryMode::SqlTable).is_table_data());
    }
}
