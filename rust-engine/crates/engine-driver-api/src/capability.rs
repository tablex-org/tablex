//! Typed driver capability tokens.
//!
//! The wire format stays a list of strings (`features`); this module maps the
//! known tokens to [`Capability`] and builds an O(1) [`CapabilitySet`] once, when
//! the capabilities are deserialized. Unknown tokens (newer drivers, test
//! fixtures) stay in the string list and still round-trip.

use std::fmt;
use std::str::FromStr;

macro_rules! capabilities {
    ($($variant:ident => $token:literal,)+) => {
        /// A feature token a v2 driver can advertise.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Capability {
            $($variant,)+
        }

        impl Capability {
            /// Every known capability, in declaration order.
            pub const ALL: &'static [Capability] = &[$(Capability::$variant,)+];

            /// The wire token (`"query.sql"`, ...).
            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Capability::$variant => $token,)+
                }
            }

            /// Parses a wire token; `None` for tokens this build does not know.
            pub fn parse(token: &str) -> Option<Self> {
                match token {
                    $($token => Some(Capability::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

capabilities! {
    Navigator => "navigator",
    QuerySql => "query.sql",
    QueryArrowCData => "query.arrow_c_data",
    QueryPageLease => "query.page_lease",
    QueryMongoFind => "query.mongo_find",
    QueryMongoScript => "query.mongo_script",
    QueryMongoAggregate => "query.mongo_aggregate",
    QueryMongoAggregateExplain => "query.mongo_aggregate_explain",
    // MongoDB: `dbengine_mongo_command`, one raw `runCommand` (the shell plugin's port).
    MongoCommand => "mongo.command",
    OperationCancel => "operation.cancel",
    ExportMongoAggregate => "export.mongo_aggregate",
    MutationInsert => "mutation.insert",
    MutationUpdate => "mutation.update",
    MutationDelete => "mutation.delete",
    MutationDropCollection => "mutation.drop_collection",
    MutationRenameCollection => "mutation.rename_collection",
    MutationCloneCollection => "mutation.clone_collection",
    MutationCreateIndex => "mutation.create_index",
    MutationDropIndex => "mutation.drop_index",
    MutationDropDatabase => "mutation.drop_database",
    DatabaseCreate => "database.create",
    BsonRendering => "bson_rendering",
    DocumentCache => "document_cache",
    AdminCollection => "admin.collection",
    AdminIndexes => "admin.indexes",
    AdminDatabase => "admin.database",
    MetadataTableProperties => "metadata.table_properties",
    // Redis (non-SQL): query kinds `redis.key` / `redis.command` and the key
    // mutation kinds, where the `MutationRequest.kind` equals the token suffix.
    QueryRedisKey => "query.redis_key",
    QueryRedisCommand => "query.redis_command",
    MutationSetString => "mutation.set_string",
    MutationSetTtl => "mutation.set_ttl",
    MutationDeleteKey => "mutation.delete_key",
    MutationDeleteKeys => "mutation.delete_keys",
    MutationHset => "mutation.hset",
    MutationHdel => "mutation.hdel",
    MutationLpush => "mutation.lpush",
    MutationRpush => "mutation.rpush",
    MutationLset => "mutation.lset",
    MutationSadd => "mutation.sadd",
    MutationSrem => "mutation.srem",
    MutationZadd => "mutation.zadd",
    MutationZrem => "mutation.zrem",
    MutationXadd => "mutation.xadd",
    MutationXdel => "mutation.xdel",
    // Added for the key browser (additive): rename a key, remove list items by value.
    MutationRenameKey => "mutation.rename_key",
    MutationLrem => "mutation.lrem",
    // SQL drivers: one `MutationRequest` of kind `batch` applies a list of
    // `sql::SqlRowChange`s atomically (all-or-nothing "Apply All").
    MutationBatch => "mutation.batch",
    // SQL drivers: `QueryRequest.payload.read_only == true` runs that one statement
    // READ ONLY (a write fails with `query.read_only_violation`); used by the agent tool layer.
    QueryReadOnly => "query.read_only",
    // SQL drivers: `QueryRequest.payload.run_to_end == true` runs the one statement exactly as written, to its end (no
    // paging wrapper, no `LIMIT`), keeping at most `limit` rows and discarding the rest; used by a script run.
    QueryRunToEnd => "query.run_to_end",
}

const MUTATION_PREFIX: &str = "mutation.";
const EXPORT_PREFIX: &str = "export.";

// The set is a single machine word; adding a 65th capability must widen it.
const _: () = assert!(Capability::ALL.len() <= u64::BITS as usize);

impl Capability {
    #[inline]
    const fn bit(self) -> u64 {
        1 << self as u32
    }

    /// Capability required to run a `MutationRequest` of this `kind`
    /// (`create_database` -> `database.create`, otherwise `mutation.<kind>`).
    /// `None` when the kind has no known token.
    pub fn for_mutation_kind(kind: &str) -> Option<Self> {
        if kind == "create_database" {
            return Some(Capability::DatabaseCreate);
        }
        Self::ALL.iter().copied().find(|capability| {
            capability
                .as_str()
                .strip_prefix(MUTATION_PREFIX)
                .is_some_and(|suffix| suffix == kind)
        })
    }
}

impl fmt::Display for Capability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error of [`Capability::from_str`]: the token is not a known capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownCapability(pub String);

impl fmt::Display for UnknownCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown capability token `{}`", self.0)
    }
}

impl std::error::Error for UnknownCapability {}

impl FromStr for Capability {
    type Err = UnknownCapability;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Self::parse(token).ok_or_else(|| UnknownCapability(token.to_owned()))
    }
}

/// O(1) membership over known [`Capability`] values plus the two token families
/// the host gates optional symbols on (`mutation.*`, `export.*`). Family flags
/// also cover tokens this build does not know.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CapabilitySet {
    bits: u64,
    any_mutation: bool,
    any_export: bool,
}

impl CapabilitySet {
    /// Builds the set from wire tokens (one pass; unknown tokens only feed the
    /// family flags).
    pub fn from_tokens<'a>(tokens: impl IntoIterator<Item = &'a str>) -> Self {
        let mut set = Self::default();
        for token in tokens {
            if let Some(capability) = Capability::parse(token) {
                set.bits |= capability.bit();
            }
            set.any_mutation |= token.starts_with(MUTATION_PREFIX);
            set.any_export |= token.starts_with(EXPORT_PREFIX);
        }
        set
    }

    #[inline]
    pub fn contains(&self, capability: Capability) -> bool {
        self.bits & capability.bit() != 0
    }

    /// Any `mutation.*` token, known or not.
    #[inline]
    pub fn has_any_mutation(&self) -> bool {
        self.any_mutation
    }

    /// Any `export.*` token, known or not.
    #[inline]
    pub fn has_any_export(&self) -> bool {
        self.any_export
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDIS_MUTATIONS: [(&str, Capability); 17] = [
        ("set_string", Capability::MutationSetString),
        ("set_ttl", Capability::MutationSetTtl),
        ("delete_key", Capability::MutationDeleteKey),
        ("delete_keys", Capability::MutationDeleteKeys),
        ("hset", Capability::MutationHset),
        ("hdel", Capability::MutationHdel),
        ("lpush", Capability::MutationLpush),
        ("rpush", Capability::MutationRpush),
        ("lset", Capability::MutationLset),
        ("sadd", Capability::MutationSadd),
        ("srem", Capability::MutationSrem),
        ("zadd", Capability::MutationZadd),
        ("zrem", Capability::MutationZrem),
        ("xadd", Capability::MutationXadd),
        ("xdel", Capability::MutationXdel),
        ("rename_key", Capability::MutationRenameKey),
        ("lrem", Capability::MutationLrem),
    ];

    #[test]
    fn every_token_round_trips_and_is_unique() {
        let mut seen = std::collections::HashSet::new();
        for &capability in Capability::ALL {
            let token = capability.as_str();
            assert!(seen.insert(token), "duplicate token {token}");
            assert_eq!(Capability::parse(token), Some(capability));
            assert_eq!(token.parse::<Capability>(), Ok(capability));
            assert_eq!(capability.to_string(), token);
        }
        assert_eq!(Capability::ALL.len(), 49);
    }

    #[test]
    fn redis_tokens_parse_and_gate_the_mutation_family() {
        assert_eq!(
            Capability::parse("query.redis_key"),
            Some(Capability::QueryRedisKey)
        );
        assert_eq!(
            "query.redis_command".parse::<Capability>(),
            Ok(Capability::QueryRedisCommand)
        );
        let set = CapabilitySet::from_tokens(["mutation.hset", "query.redis_key"]);
        assert!(set.contains(Capability::MutationHset));
        assert!(set.contains(Capability::QueryRedisKey));
        assert!(!set.contains(Capability::MutationHdel));
        assert!(!set.contains(Capability::QueryRedisCommand));
        assert!(set.has_any_mutation() && !set.has_any_export());
        // `query.*` tokens are not part of the mutation family.
        assert!(!CapabilitySet::from_tokens(["query.redis_command"]).has_any_mutation());
    }

    #[test]
    fn unknown_and_near_miss_tokens_do_not_parse() {
        assert_eq!(Capability::parse("query"), None);
        assert_eq!(Capability::parse("query.sql "), None);
        assert_eq!(Capability::parse("Query.Sql"), None);
        assert_eq!(
            "export.json".parse::<Capability>(),
            Err(UnknownCapability("export.json".into()))
        );
    }

    #[test]
    fn set_membership_is_exact_per_capability() {
        let set = CapabilitySet::from_tokens(["navigator", "query.sql", "bogus"]);
        for &capability in Capability::ALL {
            let expected = matches!(capability, Capability::Navigator | Capability::QuerySql);
            assert_eq!(set.contains(capability), expected, "{capability}");
        }
        assert!(!CapabilitySet::default().contains(Capability::Navigator));
    }

    #[test]
    fn set_covers_every_capability_without_overlap() {
        let all = CapabilitySet::from_tokens(Capability::ALL.iter().map(|c| c.as_str()));
        assert!(Capability::ALL.iter().all(|&c| all.contains(c)));
        for &only in Capability::ALL {
            let set = CapabilitySet::from_tokens([only.as_str()]);
            let hits = Capability::ALL.iter().filter(|&&c| set.contains(c)).count();
            assert_eq!(hits, 1, "{only}");
        }
    }

    #[test]
    fn families_include_unknown_tokens() {
        let none = CapabilitySet::from_tokens(["navigator", "database.create"]);
        assert!(!none.has_any_mutation() && !none.has_any_export());
        let set = CapabilitySet::from_tokens(["mutation.future_op", "export.json"]);
        assert!(set.has_any_mutation() && set.has_any_export());
        assert!(!set.contains(Capability::MutationInsert));
        let known = CapabilitySet::from_tokens(["mutation.update"]);
        assert!(known.has_any_mutation() && !known.has_any_export());
    }

    #[test]
    fn mutation_kinds_map_to_capabilities() {
        assert_eq!(
            Capability::for_mutation_kind("create_database"),
            Some(Capability::DatabaseCreate)
        );
        assert_eq!(
            Capability::for_mutation_kind("update"),
            Some(Capability::MutationUpdate)
        );
        assert_eq!(
            Capability::for_mutation_kind("drop_index"),
            Some(Capability::MutationDropIndex)
        );
        assert_eq!(Capability::for_mutation_kind("nope"), None);
        // Every Redis mutation kind resolves to its own `mutation.<kind>` token.
        for (kind, capability) in REDIS_MUTATIONS {
            assert_eq!(Capability::for_mutation_kind(kind), Some(capability));
            assert_eq!(capability.as_str(), format!("mutation.{kind}"));
        }
        // `database.create` is not a `mutation.` token.
        assert_eq!(Capability::for_mutation_kind("database.create"), None);
    }
}
