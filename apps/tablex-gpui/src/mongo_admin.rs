//! MongoDB administration model shared by the sidebar menus, the dialogs and
//! the scenario fixture. Pure data: no GPUI types, no I/O.
//!
//! Wire contract with the `dbengine-mongo` driver (see its `admin.rs`):
//!
//! * `mongo_list_indexes` (`dbengine_open_query`): rows are `listIndexes`
//!   documents annotated with an ordered `keyFields` array.
//! * Mutations (`dbengine_mutate`): `drop_collection`, `rename_collection`
//!   `{to, dropTarget:false}`, `clone_collection` `{to, include_indexes}`,
//!   `create_index` `{keys:[{field,type}], options}`, `drop_index` `{name}`,
//!   `drop_database`.
//!
//! The validators here mirror the driver's so the dialogs can refuse bad input
//! before any request is built; the driver re-validates everything.
use engine_driver_api::{
    Capability, DriverCapabilities, MutationRequest, OperationId, QueryRequest, RowField,
};

const MAX_NAMESPACE_BYTES: usize = 255;
const MAX_INDEX_NAME_BYTES: usize = 127;
const MAX_INDEX_KEYS: usize = 32;
const PROTECTED_DATABASES: [&str; 3] = ["admin", "local", "config"];

/// Which admin groups the connected driver advertises.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MongoAdminCaps {
    pub collection: bool,
    pub indexes: bool,
    pub database: bool,
}

impl MongoAdminCaps {
    #[cfg(test)]
    pub const ALL: Self = Self {
        collection: true,
        indexes: true,
        database: true,
    };

    pub fn from_capabilities(capabilities: &DriverCapabilities) -> Self {
        Self {
            collection: capabilities.has(Capability::AdminCollection),
            indexes: capabilities.has(Capability::AdminIndexes),
            database: capabilities.has(Capability::AdminDatabase),
        }
    }

    /// Admin groups among raw feature tokens (scenario fixture).
    pub fn from_features<'a>(features: impl IntoIterator<Item = &'a str>) -> Self {
        features
            .into_iter()
            .fold(Self::default(), |mut caps, feature| {
                match Capability::parse(feature) {
                    Some(Capability::AdminCollection) => caps.collection = true,
                    Some(Capability::AdminIndexes) => caps.indexes = true,
                    Some(Capability::AdminDatabase) => caps.database = true,
                    _ => {}
                }
                caps
            })
    }
}

/// Key type of one index field (`type` in the `create_index` payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IndexKeyType {
    Ascending,
    Descending,
    Text,
    Hashed,
    Geo2dSphere,
}

impl IndexKeyType {
    pub const ALL: [Self; 5] = [
        Self::Ascending,
        Self::Descending,
        Self::Text,
        Self::Hashed,
        Self::Geo2dSphere,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Ascending => "Ascending (1)",
            Self::Descending => "Descending (-1)",
            Self::Text => "Text",
            Self::Hashed => "Hashed",
            Self::Geo2dSphere => "2dsphere",
        }
    }

    fn json(self) -> serde_json::Value {
        match self {
            Self::Ascending => serde_json::json!(1),
            Self::Descending => serde_json::json!(-1),
            Self::Text => serde_json::json!("text"),
            Self::Hashed => serde_json::json!("hashed"),
            Self::Geo2dSphere => serde_json::json!("2dsphere"),
        }
    }
}

/// One validated index definition from the create form.
#[derive(Debug, Clone, PartialEq)]
pub struct NewIndex {
    pub keys: Vec<(String, IndexKeyType)>,
    pub name: Option<String>,
    pub unique: bool,
    pub sparse: bool,
    pub expire_after_seconds: Option<u32>,
}

impl NewIndex {
    /// Validates raw form text. `ttl` is the "expire after (seconds)" input.
    pub fn from_form(
        keys: &[(String, IndexKeyType)],
        name: &str,
        unique: bool,
        sparse: bool,
        ttl: &str,
    ) -> Result<Self, String> {
        if keys.is_empty() {
            return Err("Add at least one index field.".into());
        }
        if keys.len() > MAX_INDEX_KEYS {
            return Err(format!(
                "An index can have at most {MAX_INDEX_KEYS} fields."
            ));
        }
        let mut cleaned: Vec<(String, IndexKeyType)> = Vec::with_capacity(keys.len());
        for (field, key_type) in keys {
            let field = field.trim();
            if field.is_empty() {
                return Err("Select or enter a field for every index row.".into());
            }
            if field.contains('\0') || field.starts_with('$') {
                return Err(format!("'{field}' is not a valid field name."));
            }
            if cleaned.iter().any(|(existing, _)| existing == field) {
                return Err(format!("Field '{field}' is listed twice."));
            }
            cleaned.push((field.to_string(), *key_type));
        }
        let name = name.trim();
        if name.len() > MAX_INDEX_NAME_BYTES || name.contains('\0') {
            return Err("Index name is too long or contains NUL bytes.".into());
        }
        if name == "_id_" {
            return Err("The index name _id_ is reserved.".into());
        }
        let ttl = ttl.trim();
        let expire_after_seconds = if ttl.is_empty() {
            None
        } else {
            let seconds: u32 = ttl
                .parse()
                .ok()
                .filter(|seconds| *seconds <= i32::MAX as u32)
                .ok_or("Expire after must be a whole number of seconds.")?;
            if cleaned.len() != 1
                || !matches!(
                    cleaned[0].1,
                    IndexKeyType::Ascending | IndexKeyType::Descending
                )
            {
                return Err("A TTL index needs exactly one ascending or descending field.".into());
            }
            Some(seconds)
        };
        Ok(Self {
            keys: cleaned,
            name: (!name.is_empty()).then(|| name.to_string()),
            unique,
            sparse,
            expire_after_seconds,
        })
    }

    /// MongoDB's default name when the form leaves it blank.
    pub fn effective_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.keys
                .iter()
                .map(|(field, key_type)| {
                    let suffix = match key_type {
                        IndexKeyType::Ascending => "1",
                        IndexKeyType::Descending => "-1",
                        IndexKeyType::Text => "text",
                        IndexKeyType::Hashed => "hashed",
                        IndexKeyType::Geo2dSphere => "2dsphere",
                    };
                    format!("{field}_{suffix}")
                })
                .collect::<Vec<_>>()
                .join("_")
        })
    }
}

/// One admin mutation against the connected server.
#[derive(Debug, Clone, PartialEq)]
pub enum MongoAdminOp {
    DropCollection {
        collection: String,
    },
    RenameCollection {
        from: String,
        to: String,
    },
    CloneCollection {
        from: String,
        to: String,
        include_indexes: bool,
    },
    CreateIndex {
        collection: String,
        index: NewIndex,
    },
    DropIndex {
        collection: String,
        name: String,
    },
    DropDatabase,
    /// The database is named by the `database` the operation runs against.
    CreateDatabase,
}

impl MongoAdminOp {
    /// The `MutationRequest` for the driver; `database` is always set.
    pub fn mutation(&self, database: &str, operation_id: OperationId) -> MutationRequest {
        let request =
            |kind: &str, object: Option<&str>, payload: serde_json::Value| MutationRequest {
                operation_id,
                kind: kind.into(),
                database: Some(database.into()),
                object: object.map(str::to_string),
                identity: serde_json::Value::Null,
                payload,
            };
        match self {
            Self::DropCollection { collection } => {
                request("drop_collection", Some(collection), serde_json::Value::Null)
            }
            Self::RenameCollection { from, to } => request(
                "rename_collection",
                Some(from),
                serde_json::json!({"to": to, "dropTarget": false}),
            ),
            Self::CloneCollection {
                from,
                to,
                include_indexes,
            } => request(
                "clone_collection",
                Some(from),
                serde_json::json!({"to": to, "include_indexes": include_indexes}),
            ),
            Self::CreateIndex { collection, index } => {
                let keys: Vec<_> = index
                    .keys
                    .iter()
                    .map(|(field, key_type)| {
                        serde_json::json!({"field": field, "type": key_type.json()})
                    })
                    .collect();
                let mut options = serde_json::Map::new();
                if let Some(name) = &index.name {
                    options.insert("name".into(), serde_json::json!(name));
                }
                if index.unique {
                    options.insert("unique".into(), serde_json::json!(true));
                }
                if index.sparse {
                    options.insert("sparse".into(), serde_json::json!(true));
                }
                if let Some(seconds) = index.expire_after_seconds {
                    options.insert("expireAfterSeconds".into(), serde_json::json!(seconds));
                }
                request(
                    "create_index",
                    Some(collection),
                    serde_json::json!({"keys": keys, "options": options}),
                )
            }
            Self::DropIndex { collection, name } => request(
                "drop_index",
                Some(collection),
                serde_json::json!({"name": name}),
            ),
            Self::DropDatabase => request("drop_database", None, serde_json::Value::Null),
            Self::CreateDatabase => request("create_database", None, serde_json::Value::Null),
        }
    }
}

pub fn list_indexes_request(
    database: &str,
    collection: &str,
    operation_id: OperationId,
) -> QueryRequest {
    QueryRequest {
        kind: "mongo_list_indexes".into(),
        operation_id,
        database: Some(database.into()),
        object: Some(collection.into()),
        offset: 0,
        limit: 200,
        payload: serde_json::Value::Null,
    }
}

/// A new collection name (rename / clone target); mirrors the driver.
pub fn validate_new_collection_name(database: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Enter a collection name.".into());
    }
    if name.trim() != name {
        return Err("The name must not start or end with whitespace.".into());
    }
    if name.contains('\0') {
        return Err("The name must not contain NUL bytes.".into());
    }
    if name.contains('$') {
        return Err("The name must not contain '$'.".into());
    }
    if name.chars().any(char::is_control) {
        return Err("The name must not contain control characters.".into());
    }
    if name.starts_with("system.") {
        return Err("The name must not start with 'system.'.".into());
    }
    if database.len() + 1 + name.len() > MAX_NAMESPACE_BYTES {
        return Err("The name is too long.".into());
    }
    Ok(())
}

pub fn is_protected_database(name: &str) -> bool {
    PROTECTED_DATABASES.contains(&name)
}

/// System collections cannot be dropped, renamed or indexed from the UI.
pub fn is_system_collection(name: &str) -> bool {
    name.starts_with("system.")
}

/// Exact-match gate of the typed-name confirmation dialogs.
pub fn typed_name_matches(expected: &str, typed: &str) -> bool {
    !expected.is_empty() && expected == typed
}

/// One row of the index manager table.
#[derive(Debug, Clone, PartialEq)]
pub struct MongoIndexInfo {
    pub name: String,
    pub keys: Vec<(String, String)>,
    pub unique: bool,
    pub sparse: bool,
    pub ttl_seconds: Option<i64>,
    pub partial_filter: Option<String>,
}

impl MongoIndexInfo {
    pub fn is_id(&self) -> bool {
        self.name == "_id_"
    }

    /// `email:1, created:-1`
    pub fn keys_label(&self) -> String {
        self.keys
            .iter()
            .map(|(field, key_type)| format!("{field}:{key_type}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Reads one inspected `listIndexes` row. Key order comes from the
    /// driver's ordered `keyFields` array, never from JSON object order.
    pub fn from_fields(fields: &[RowField]) -> Option<Self> {
        let value = |name: &str| fields.iter().find(|field| field.name == name);
        let name = value("name")?.value.as_str()?.to_string();
        let keys = value("keyFields")?
            .value
            .as_array()?
            .iter()
            .filter_map(|entry| {
                let field = entry.get("field")?.as_str()?.to_string();
                let key_type = match entry.get("type")? {
                    serde_json::Value::String(kind) => kind.clone(),
                    number => number.to_string(),
                };
                Some((field, key_type))
            })
            .collect();
        let flag = |name: &str| {
            value(name)
                .and_then(|field| field.value.as_bool())
                .unwrap_or(false)
        };
        Some(Self {
            name,
            keys,
            unique: flag("unique"),
            sparse: flag("sparse"),
            ttl_seconds: value("expireAfterSeconds").and_then(|field| field.value.as_i64()),
            partial_filter: value("partialFilterExpression")
                .map(|field| field.display_text.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, value: serde_json::Value) -> RowField {
        RowField {
            name: name.into(),
            native_type: "x".into(),
            display_text: value.to_string(),
            value,
            editable: false,
        }
    }

    #[test]
    fn capabilities_come_only_from_advertised_features() {
        assert_eq!(
            MongoAdminCaps::from_features(["query.mongo_find", "admin.indexes"]),
            MongoAdminCaps {
                collection: false,
                indexes: true,
                database: false
            }
        );
        assert_eq!(
            MongoAdminCaps::from_features([
                Capability::AdminCollection.as_str(),
                Capability::AdminIndexes.as_str(),
                Capability::AdminDatabase.as_str()
            ]),
            MongoAdminCaps::ALL
        );
        assert_eq!(MongoAdminCaps::from_features([]), MongoAdminCaps::default());
    }

    #[test]
    fn mutation_payloads_match_the_driver_contract() {
        let rename = MongoAdminOp::RenameCollection {
            from: "a".into(),
            to: "b".into(),
        }
        .mutation("app", OperationId(7));
        assert_eq!(rename.kind, "rename_collection");
        assert_eq!(rename.database.as_deref(), Some("app"));
        assert_eq!(rename.object.as_deref(), Some("a"));
        assert_eq!(
            rename.payload,
            serde_json::json!({"to": "b", "dropTarget": false})
        );

        let clone = MongoAdminOp::CloneCollection {
            from: "a".into(),
            to: "b".into(),
            include_indexes: true,
        }
        .mutation("app", OperationId(8));
        assert_eq!(
            clone.payload,
            serde_json::json!({"to": "b", "include_indexes": true})
        );

        let create_database = MongoAdminOp::CreateDatabase.mutation("fresh", OperationId(11));
        assert_eq!(create_database.kind, "create_database");
        assert_eq!(create_database.database.as_deref(), Some("fresh"));
        assert!(create_database.object.is_none() && create_database.payload.is_null());

        let drop_database = MongoAdminOp::DropDatabase.mutation("app", OperationId(9));
        assert_eq!(drop_database.kind, "drop_database");
        assert!(drop_database.object.is_none());
        assert!(drop_database.payload.is_null());

        let index = NewIndex::from_form(
            &[
                ("zeta".into(), IndexKeyType::Ascending),
                ("alpha".into(), IndexKeyType::Descending),
            ],
            "z_a",
            true,
            false,
            "",
        )
        .unwrap();
        let create = MongoAdminOp::CreateIndex {
            collection: "c".into(),
            index,
        }
        .mutation("app", OperationId(10));
        assert_eq!(
            create.payload,
            serde_json::json!({
                "keys": [{"field": "zeta", "type": 1}, {"field": "alpha", "type": -1}],
                "options": {"name": "z_a", "unique": true}
            })
        );
        let drop_index = MongoAdminOp::DropIndex {
            collection: "c".into(),
            name: "z_a".into(),
        }
        .mutation("app", OperationId(11));
        assert_eq!(drop_index.payload, serde_json::json!({"name": "z_a"}));
    }

    #[test]
    fn index_form_validation_matches_driver_rules() {
        let key = |name: &str| vec![(name.to_string(), IndexKeyType::Ascending)];
        assert!(NewIndex::from_form(&[], "", false, false, "").is_err());
        assert!(NewIndex::from_form(&key(" "), "", false, false, "").is_err());
        assert!(NewIndex::from_form(&key("$bad"), "", false, false, "").is_err());
        assert!(NewIndex::from_form(&key("a\0"), "", false, false, "").is_err());
        assert!(NewIndex::from_form(&key("a"), "_id_", false, false, "").is_err());
        assert!(NewIndex::from_form(&key("a"), "", false, false, "abc").is_err());
        assert!(NewIndex::from_form(&key("a"), "", false, false, "-5").is_err());
        assert!(NewIndex::from_form(&key("a"), "", false, false, "4294967295").is_err());
        let duplicated = [
            ("a".to_string(), IndexKeyType::Ascending),
            ("a".to_string(), IndexKeyType::Descending),
        ];
        assert!(NewIndex::from_form(&duplicated, "", false, false, "").is_err());
        let compound = [
            ("a".to_string(), IndexKeyType::Ascending),
            ("b".to_string(), IndexKeyType::Ascending),
        ];
        assert!(NewIndex::from_form(&compound, "", false, false, "60").is_err());
        let text = [("a".to_string(), IndexKeyType::Text)];
        assert!(NewIndex::from_form(&text, "", false, false, "60").is_err());
        let ok = NewIndex::from_form(&key(" email "), " ", true, true, " 3600 ").unwrap();
        assert_eq!(ok.keys[0].0, "email");
        assert_eq!(ok.name, None);
        assert_eq!(ok.expire_after_seconds, Some(3600));
        assert_eq!(ok.effective_name(), "email_1");
    }

    #[test]
    fn collection_name_rules_match_the_driver() {
        assert!(validate_new_collection_name("app", "orders").is_ok());
        for bad in ["", " x", "x ", "a$b", "a\0b", "system.x", "tab\tname"] {
            assert!(validate_new_collection_name("app", bad).is_err(), "{bad:?}");
        }
        assert!(validate_new_collection_name("app", &"x".repeat(300)).is_err());
        assert!(is_protected_database("admin") && !is_protected_database("sales"));
        assert!(is_system_collection("system.views") && !is_system_collection("systems"));
    }

    #[test]
    fn typed_confirmation_requires_the_exact_name() {
        assert!(typed_name_matches("orders", "orders"));
        assert!(!typed_name_matches("orders", "Orders"));
        assert!(!typed_name_matches("orders", "orders "));
        assert!(!typed_name_matches("orders", ""));
        assert!(!typed_name_matches("", ""));
    }

    #[test]
    fn index_info_reads_ordered_key_fields_and_options() {
        let info = MongoIndexInfo::from_fields(&[
            field("name", serde_json::json!("z_a")),
            field(
                "keyFields",
                serde_json::json!([{"field": "zeta", "type": 1}, {"field": "alpha", "type": -1}]),
            ),
            field("unique", serde_json::json!(true)),
            field("expireAfterSeconds", serde_json::json!(3600)),
            field(
                "partialFilterExpression",
                serde_json::json!({"status": "active"}),
            ),
        ])
        .unwrap();
        assert_eq!(info.keys_label(), "zeta:1, alpha:-1");
        assert!(info.unique && !info.sparse && !info.is_id());
        assert_eq!(info.ttl_seconds, Some(3600));
        assert!(info.partial_filter.is_some());
        assert!(MongoIndexInfo::from_fields(&[field("name", serde_json::json!("x"))]).is_none());
    }
}
