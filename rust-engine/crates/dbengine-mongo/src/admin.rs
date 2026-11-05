//! MongoDB administration operations (collection / index / database).
//!
//! Wire contract (additive on ABI v2; no `ABI_VERSION` bump, no new exports):
//!
//! * Read: `dbengine_open_query` with `kind = "mongo_list_indexes"`,
//!   `database` + `object` (collection). Returns a normal query page whose
//!   rows are the raw `listIndexes` documents (`name`, `key`, `unique`,
//!   `sparse`, `expireAfterSeconds`, ...). The host reads rows with
//!   `inspect_row` and releases the page.
//! * Mutations: `dbengine_mutate` with one of the `ADMIN_MUTATION_KINDS`.
//!   `affected_rows` is the number of documents for `clone_collection` and 1
//!   for every other kind.
//!
//! Every request is validated here, before any backend call, so the checks are
//! unit-testable without a server.
//!
//! Safety stance: nothing in this module overwrites existing data implicitly.
//! `rename_collection` always sends `dropTarget: false` (a request asking for
//! `dropTarget: true` is refused), `clone_collection` refuses an existing
//! target and copies with `$merge` whose `whenMatched` is `fail`, and
//! `drop_index` refuses the mandatory `_id_` index.
use bson::{doc, Bson, Document};
use engine_driver_api::{MutationRequest, QueryRequest};

pub(crate) const KIND_DROP_COLLECTION: &str = "drop_collection";
pub(crate) const KIND_RENAME_COLLECTION: &str = "rename_collection";
pub(crate) const KIND_CLONE_COLLECTION: &str = "clone_collection";
pub(crate) const KIND_CREATE_INDEX: &str = "create_index";
pub(crate) const KIND_DROP_INDEX: &str = "drop_index";
pub(crate) const KIND_DROP_DATABASE: &str = "drop_database";
pub(crate) const QUERY_LIST_INDEXES: &str = "mongo_list_indexes";

pub(crate) const ADMIN_MUTATION_KINDS: [&str; 6] = [
    KIND_DROP_COLLECTION,
    KIND_RENAME_COLLECTION,
    KIND_CLONE_COLLECTION,
    KIND_CREATE_INDEX,
    KIND_DROP_INDEX,
    KIND_DROP_DATABASE,
];

/// Namespace (`db.collection`) limit enforced by the server.
const MAX_NAMESPACE_BYTES: usize = 255;
const MAX_INDEX_NAME_BYTES: usize = 127;
const MAX_INDEX_KEYS: usize = 32;
const SPECIAL_INDEX_TYPES: [&str; 4] = ["text", "2dsphere", "2d", "hashed"];
const PROTECTED_DATABASES: [&str; 3] = ["admin", "local", "config"];

pub(crate) fn is_admin_mutation_kind(kind: &str) -> bool {
    ADMIN_MUTATION_KINDS.contains(&kind)
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexSpec {
    /// Ordered key list: compound index order is significant.
    pub keys: Vec<(String, Bson)>,
    pub name: Option<String>,
    pub unique: bool,
    pub sparse: bool,
    pub expire_after_seconds: Option<i64>,
    pub partial_filter: Option<Document>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum AdminMutation {
    DropCollection {
        database: String,
        collection: String,
    },
    RenameCollection {
        database: String,
        from: String,
        to: String,
    },
    CloneCollection {
        database: String,
        from: String,
        to: String,
        include_indexes: bool,
    },
    CreateIndex {
        database: String,
        collection: String,
        spec: IndexSpec,
    },
    DropIndex {
        database: String,
        collection: String,
        name: String,
    },
    DropDatabase {
        database: String,
    },
}

/// Strips connection strings (credentials) from driver error text.
pub(crate) fn redact(message: &str) -> String {
    static URI: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let uri =
        URI.get_or_init(|| regex::Regex::new(r"mongodb(\+srv)?://\S+").expect("static regex"));
    uri.replace_all(message, "mongodb://***").into_owned()
}

fn require_str<'a>(value: &'a Option<String>, label: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{label} is required"))
}

/// An existing database/collection name used as the operation *source*: only
/// reject what can never be a name or would be dangerous to forward.
fn validate_existing_name(label: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err(format!("{label} is required"));
    }
    if name.contains('\0') {
        return Err(format!("{label} must not contain NUL bytes"));
    }
    if name.len() > MAX_NAMESPACE_BYTES {
        return Err(format!("{label} is too long"));
    }
    Ok(())
}

fn validate_source_collection(name: &str) -> Result<(), String> {
    validate_existing_name("Collection name", name)?;
    if name.starts_with("system.") {
        return Err("System collections cannot be modified".into());
    }
    Ok(())
}

/// A *new* collection name (rename / clone target): strict MongoDB rules.
pub(crate) fn validate_new_collection_name(database: &str, name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Collection name is required".into());
    }
    if name.trim() != name {
        return Err("Collection name must not have leading or trailing whitespace".into());
    }
    if name.contains('\0') {
        return Err("Collection name must not contain NUL bytes".into());
    }
    if name.contains('$') {
        return Err("Collection name must not contain '$'".into());
    }
    if name.chars().any(char::is_control) {
        return Err("Collection name must not contain control characters".into());
    }
    if name.starts_with("system.") {
        return Err("Collection name must not start with 'system.'".into());
    }
    if database.len() + 1 + name.len() > MAX_NAMESPACE_BYTES {
        return Err("Collection name is too long".into());
    }
    Ok(())
}

pub(crate) fn validate_droppable_database(name: &str) -> Result<(), String> {
    validate_existing_name("Database name", name)?;
    if PROTECTED_DATABASES.contains(&name) {
        return Err(format!("The '{name}' database cannot be dropped"));
    }
    Ok(())
}

fn string_field(payload: &serde_json::Value, key: &str) -> Result<Option<String>, String> {
    match payload.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(format!("'{key}' must be a string")),
    }
}

fn bool_field(payload: &serde_json::Value, key: &str) -> Result<Option<bool>, String> {
    match payload.get(key) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(format!("'{key}' must be a boolean")),
    }
}

fn reject_unknown_fields(
    payload: &serde_json::Value,
    allowed: &[&str],
    label: &str,
) -> Result<(), String> {
    match payload {
        serde_json::Value::Null => Ok(()),
        serde_json::Value::Object(map) => {
            match map.keys().find(|key| !allowed.contains(&key.as_str())) {
                Some(key) => Err(format!("Unexpected field '{key}' in {label} request")),
                None => Ok(()),
            }
        }
        _ => Err(format!("{label} payload must be an object")),
    }
}

pub(crate) fn parse_admin_mutation(request: &MutationRequest) -> Result<AdminMutation, String> {
    let database = require_str(&request.database, "Database name")?.to_string();
    let payload = &request.payload;
    match request.kind.as_str() {
        KIND_DROP_DATABASE => {
            validate_droppable_database(&database)?;
            if request.object.is_some() {
                return Err("Drop database request has unexpected fields".into());
            }
            reject_unknown_fields(payload, &[], "drop_database")?;
            Ok(AdminMutation::DropDatabase { database })
        }
        KIND_DROP_COLLECTION => {
            validate_existing_name("Database name", &database)?;
            let collection = require_str(&request.object, "Collection name")?.to_string();
            validate_source_collection(&collection)?;
            reject_unknown_fields(payload, &[], "drop_collection")?;
            Ok(AdminMutation::DropCollection {
                database,
                collection,
            })
        }
        KIND_RENAME_COLLECTION => {
            validate_existing_name("Database name", &database)?;
            let from = require_str(&request.object, "Collection name")?.to_string();
            validate_source_collection(&from)?;
            reject_unknown_fields(payload, &["to", "dropTarget"], "rename_collection")?;
            let to = string_field(payload, "to")?.ok_or("'to' is required")?;
            validate_new_collection_name(&database, &to)?;
            if bool_field(payload, "dropTarget")?.unwrap_or(false) {
                return Err(
                    "Renaming over an existing collection (dropTarget) is not allowed".into(),
                );
            }
            if to == from {
                return Err("The new name must differ from the current name".into());
            }
            Ok(AdminMutation::RenameCollection { database, from, to })
        }
        KIND_CLONE_COLLECTION => {
            validate_existing_name("Database name", &database)?;
            let from = require_str(&request.object, "Collection name")?.to_string();
            validate_existing_name("Collection name", &from)?;
            reject_unknown_fields(payload, &["to", "include_indexes"], "clone_collection")?;
            let to = string_field(payload, "to")?.ok_or("'to' is required")?;
            validate_new_collection_name(&database, &to)?;
            if to == from {
                return Err("The clone name must differ from the source collection".into());
            }
            let include_indexes = bool_field(payload, "include_indexes")?.unwrap_or(false);
            Ok(AdminMutation::CloneCollection {
                database,
                from,
                to,
                include_indexes,
            })
        }
        KIND_CREATE_INDEX => {
            validate_existing_name("Database name", &database)?;
            let collection = require_str(&request.object, "Collection name")?.to_string();
            validate_source_collection(&collection)?;
            reject_unknown_fields(payload, &["keys", "options"], "create_index")?;
            let spec = parse_index_spec(payload)?;
            Ok(AdminMutation::CreateIndex {
                database,
                collection,
                spec,
            })
        }
        KIND_DROP_INDEX => {
            validate_existing_name("Database name", &database)?;
            let collection = require_str(&request.object, "Collection name")?.to_string();
            validate_source_collection(&collection)?;
            reject_unknown_fields(payload, &["name"], "drop_index")?;
            let name = string_field(payload, "name")?.ok_or("'name' is required")?;
            validate_index_name(&name)?;
            if name == "_id_" {
                return Err("The _id_ index cannot be dropped".into());
            }
            if name == "*" {
                return Err("Dropping all indexes is not supported".into());
            }
            Ok(AdminMutation::DropIndex {
                database,
                collection,
                name,
            })
        }
        kind => Err(format!("unsupported MongoDB admin kind: {kind}")),
    }
}

/// `(database, collection)` of a `mongo_list_indexes` query.
pub(crate) fn parse_list_indexes(request: &QueryRequest) -> Result<(String, String), String> {
    let database = require_str(&request.database, "Database name")?;
    validate_existing_name("Database name", database)?;
    let collection = require_str(&request.object, "Collection name")?;
    validate_existing_name("Collection name", collection)?;
    Ok((database.to_string(), collection.to_string()))
}

fn validate_index_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Index name is required".into());
    }
    if name.contains('\0') {
        return Err("Index name must not contain NUL bytes".into());
    }
    if name.len() > MAX_INDEX_NAME_BYTES {
        return Err("Index name is too long".into());
    }
    Ok(())
}

/// Payload: `{"keys":[{"field":"email","type":1}, ...], "options":{...}}`.
/// `keys` is an ordered array because JSON objects do not guarantee the key
/// order a compound index depends on. `type` is `1`, `-1`, or one of
/// `text`, `2dsphere`, `2d`, `hashed`.
fn parse_index_spec(payload: &serde_json::Value) -> Result<IndexSpec, String> {
    let keys = payload
        .get("keys")
        .and_then(serde_json::Value::as_array)
        .ok_or("'keys' must be a non-empty array")?;
    if keys.is_empty() {
        return Err("An index needs at least one key".into());
    }
    if keys.len() > MAX_INDEX_KEYS {
        return Err(format!("An index can have at most {MAX_INDEX_KEYS} keys"));
    }
    let mut parsed: Vec<(String, Bson)> = Vec::with_capacity(keys.len());
    for key in keys {
        let field = key
            .get("field")
            .and_then(serde_json::Value::as_str)
            .ok_or("every index key needs a 'field' string")?;
        if field.is_empty() {
            return Err("Index key field names must not be empty".into());
        }
        if field.trim() != field {
            return Err(
                "Index key field names must not have leading or trailing whitespace".into(),
            );
        }
        if field.contains('\0') {
            return Err("Index key field names must not contain NUL bytes".into());
        }
        if field.starts_with('$') {
            return Err("Index key field names must not start with '$'".into());
        }
        if parsed.iter().any(|(existing, _)| existing == field) {
            return Err(format!("Index key '{field}' is listed twice"));
        }
        let direction = match key.get("type") {
            Some(serde_json::Value::Number(number)) => match number.as_i64() {
                Some(1) => Bson::Int32(1),
                Some(-1) => Bson::Int32(-1),
                _ => return Err("Index key type must be 1, -1, or a special index type".into()),
            },
            Some(serde_json::Value::String(kind))
                if SPECIAL_INDEX_TYPES.contains(&kind.as_str()) =>
            {
                Bson::String(kind.clone())
            }
            _ => return Err("Index key type must be 1, -1, or a special index type".into()),
        };
        parsed.push((field.to_string(), direction));
    }

    let options = payload.get("options").unwrap_or(&serde_json::Value::Null);
    reject_unknown_fields(
        options,
        &[
            "name",
            "unique",
            "sparse",
            "expireAfterSeconds",
            "partialFilterExpression",
        ],
        "index options",
    )?;
    let name = string_field(options, "name")?
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    if let Some(name) = &name {
        validate_index_name(name)?;
        if name == "_id_" {
            return Err("The index name _id_ is reserved".into());
        }
    }
    let unique = bool_field(options, "unique")?.unwrap_or(false);
    let sparse = bool_field(options, "sparse")?.unwrap_or(false);
    let expire_after_seconds = match options.get("expireAfterSeconds") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let seconds = value
                .as_i64()
                .ok_or("expireAfterSeconds must be a whole number of seconds")?;
            if !(0..=i64::from(i32::MAX)).contains(&seconds) {
                return Err("expireAfterSeconds must be between 0 and 2147483647".into());
            }
            Some(seconds)
        }
    };
    if expire_after_seconds.is_some() {
        let single_ordered = parsed.len() == 1 && matches!(parsed[0].1, Bson::Int32(_));
        if !single_ordered {
            return Err("A TTL index needs exactly one ascending or descending key".into());
        }
    }
    let partial_filter = match options.get("partialFilterExpression") {
        None | Some(serde_json::Value::Null) => None,
        Some(value @ serde_json::Value::Object(_)) => Some(
            bson::to_document(value)
                .map_err(|error| format!("invalid partialFilterExpression: {error}"))?,
        ),
        Some(_) => return Err("partialFilterExpression must be an object".into()),
    };
    Ok(IndexSpec {
        keys: parsed,
        name,
        unique,
        sparse,
        expire_after_seconds,
        partial_filter,
    })
}

/// MongoDB's default index name: `field_dir` pairs joined by `_`.
pub(crate) fn default_index_name(keys: &[(String, Bson)]) -> String {
    keys.iter()
        .map(|(field, direction)| match direction {
            Bson::Int32(value) => format!("{field}_{value}"),
            Bson::String(kind) => format!("{field}_{kind}"),
            other => format!("{field}_{other}"),
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// The `createIndexes` command for one validated spec.
pub(crate) fn create_index_command(collection: &str, spec: &IndexSpec) -> Document {
    let mut key = Document::new();
    for (field, direction) in &spec.keys {
        key.insert(field.clone(), direction.clone());
    }
    let mut index = doc! {
        "key": key,
        "name": spec.name.clone().unwrap_or_else(|| default_index_name(&spec.keys)),
    };
    if spec.unique {
        index.insert("unique", true);
    }
    if spec.sparse {
        index.insert("sparse", true);
    }
    if let Some(seconds) = spec.expire_after_seconds {
        index.insert("expireAfterSeconds", seconds as i32);
    }
    if let Some(filter) = &spec.partial_filter {
        index.insert("partialFilterExpression", filter.clone());
    }
    doc! { "createIndexes": collection, "indexes": [index] }
}

/// Source index specs worth copying to a clone: everything but `_id_`, minus
/// the server-owned `v`/`ns` fields.
pub(crate) fn clonable_index_specs(listed: Vec<Document>) -> Vec<Document> {
    listed
        .into_iter()
        .filter(|spec| spec.get_str("name") != Ok("_id_"))
        .map(|mut spec| {
            spec.remove("v");
            spec.remove("ns");
            spec
        })
        .collect()
}

/// Adds `keyFields` to a raw `listIndexes` document: the key pattern as an
/// ordered array of `{field, type}` (same shape `create_index` accepts).
/// JSON objects carry no order guarantee across the FFI boundary, but a
/// compound index is defined by its key order, so hosts read this array.
pub(crate) fn annotate_index_spec(mut spec: Document) -> Document {
    let fields: Vec<Bson> = spec
        .get_document("key")
        .map(|key| {
            key.iter()
                .map(|(field, direction)| {
                    let direction = match direction {
                        Bson::Int32(value) => Bson::Int32(*value),
                        Bson::Int64(value) => Bson::Int64(*value),
                        Bson::Double(value) => Bson::Double(*value),
                        Bson::String(kind) => Bson::String(kind.clone()),
                        other => Bson::String(other.to_string()),
                    };
                    Bson::Document(doc! { "field": field.as_str(), "type": direction })
                })
                .collect()
        })
        .unwrap_or_default();
    spec.insert("keyFields", fields);
    spec
}

/// `$merge` pipeline used by `clone_collection`. `whenMatched: fail` means a
/// pre-existing `_id` aborts the copy instead of overwriting; combined with
/// the caller's "target must not exist" check nothing is replaced.
pub(crate) fn clone_pipeline(target: &str) -> Vec<Document> {
    vec![doc! {
        "$merge": {
            "into": target,
            "on": "_id",
            "whenMatched": "fail",
            "whenNotMatched": "insert",
        }
    }]
}

pub(crate) async fn run_admin_mutation(
    client: &mongodb::Client,
    operation: &AdminMutation,
) -> Result<u64, String> {
    run_admin_mutation_inner(client, operation)
        .await
        .map_err(|error| redact(&error))
}

async fn run_admin_mutation_inner(
    client: &mongodb::Client,
    operation: &AdminMutation,
) -> Result<u64, String> {
    let describe = |error: mongodb::error::Error| error.to_string();
    match operation {
        AdminMutation::DropCollection {
            database,
            collection,
        } => {
            client
                .database(database)
                .collection::<Document>(collection)
                .drop()
                .await
                .map_err(describe)?;
            Ok(1)
        }
        AdminMutation::RenameCollection { database, from, to } => {
            client
                .database("admin")
                .run_command(doc! {
                    "renameCollection": format!("{database}.{from}"),
                    "to": format!("{database}.{to}"),
                    "dropTarget": false,
                })
                .await
                .map_err(describe)?;
            Ok(1)
        }
        AdminMutation::CloneCollection {
            database,
            from,
            to,
            include_indexes,
        } => {
            let db = client.database(database);
            let names = db
                .list_collection_names()
                .filter(doc! { "name": { "$in": [from.as_str(), to.as_str()] } })
                .await
                .map_err(describe)?;
            if !names.iter().any(|name| name == from) {
                return Err(format!("Collection '{from}' does not exist"));
            }
            if names.iter().any(|name| name == to) {
                return Err(format!("Collection '{to}' already exists"));
            }
            let mut cursor = db
                .collection::<Document>(from)
                .aggregate(clone_pipeline(to))
                .await
                .map_err(|error| {
                    format!(
                        "Clone failed: {error}. '{to}' may exist partially; inspect or drop it."
                    )
                })?;
            while futures::TryStreamExt::try_next(&mut cursor)
                .await
                .map_err(|error| {
                    format!(
                        "Clone failed: {error}. '{to}' may exist partially; inspect or drop it."
                    )
                })?
                .is_some()
            {}
            if *include_indexes {
                let specs = clonable_index_specs(list_indexes_raw(client, database, from).await?);
                if !specs.is_empty() {
                    db.run_command(doc! { "createIndexes": to, "indexes": specs })
                        .await
                        .map_err(|error| {
                            format!("Documents were cloned, but copying indexes failed: {error}")
                        })?;
                }
            }
            db.collection::<Document>(to)
                .count_documents(doc! {})
                .await
                .map_err(describe)
        }
        AdminMutation::CreateIndex {
            database,
            collection,
            spec,
        } => {
            client
                .database(database)
                .run_command(create_index_command(collection, spec))
                .await
                .map_err(describe)?;
            Ok(1)
        }
        AdminMutation::DropIndex {
            database,
            collection,
            name,
        } => {
            client
                .database(database)
                .collection::<Document>(collection)
                .drop_index(name.clone())
                .await
                .map_err(describe)?;
            Ok(1)
        }
        AdminMutation::DropDatabase { database } => {
            client.database(database).drop().await.map_err(describe)?;
            Ok(1)
        }
    }
}

pub(crate) async fn list_indexes_raw(
    client: &mongodb::Client,
    database: &str,
    collection: &str,
) -> Result<Vec<Document>, String> {
    let reply = client
        .database(database)
        .run_command(doc! { "listIndexes": collection, "cursor": { "batchSize": 1000 } })
        .await
        .map_err(|error| redact(&error.to_string()))?;
    let batch = reply
        .get_document("cursor")
        .and_then(|cursor| cursor.get_array("firstBatch"))
        .map_err(|_| "listIndexes returned an unexpected reply".to_string())?;
    Ok(batch
        .iter()
        .filter_map(|entry| match entry {
            Bson::Document(spec) => Some(spec.clone()),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_driver_api::OperationId;
    use serde_json::json;

    fn mutation(
        kind: &str,
        database: Option<&str>,
        object: Option<&str>,
        payload: serde_json::Value,
    ) -> MutationRequest {
        MutationRequest {
            operation_id: OperationId(1),
            kind: kind.into(),
            database: database.map(str::to_string),
            object: object.map(str::to_string),
            identity: serde_json::Value::Null,
            payload,
        }
    }

    fn parse(
        kind: &str,
        database: Option<&str>,
        object: Option<&str>,
        payload: serde_json::Value,
    ) -> Result<AdminMutation, String> {
        parse_admin_mutation(&mutation(kind, database, object, payload))
    }

    #[test]
    fn drop_collection_requires_database_and_a_non_system_collection() {
        assert_eq!(
            parse("drop_collection", Some("app"), Some("orders"), json!(null)).unwrap(),
            AdminMutation::DropCollection {
                database: "app".into(),
                collection: "orders".into()
            }
        );
        assert!(parse("drop_collection", None, Some("orders"), json!(null)).is_err());
        assert!(parse("drop_collection", Some("app"), None, json!(null)).is_err());
        assert!(parse("drop_collection", Some("app"), Some(""), json!(null)).is_err());
        assert!(parse("drop_collection", Some("app"), Some("a\0b"), json!(null)).is_err());
        assert!(parse(
            "drop_collection",
            Some("app"),
            Some("system.users"),
            json!(null)
        )
        .is_err());
        assert!(parse(
            "drop_collection",
            Some("app"),
            Some("x"),
            json!({"force": true})
        )
        .is_err());
    }

    #[test]
    fn rename_validates_target_and_never_allows_drop_target() {
        let ok = parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b", "dropTarget": false}),
        );
        assert_eq!(
            ok.unwrap(),
            AdminMutation::RenameCollection {
                database: "app".into(),
                from: "a".into(),
                to: "b".into()
            }
        );
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b"})
        )
        .is_ok());
        for bad in [
            "",
            " ",
            "b ",
            "has$dollar",
            "nul\0",
            "system.b",
            "tab\tname",
        ] {
            assert!(
                parse(
                    "rename_collection",
                    Some("app"),
                    Some("a"),
                    json!({"to": bad})
                )
                .is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": "a"})
        )
        .is_err());
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b", "dropTarget": true})
        )
        .is_err());
        assert!(parse("rename_collection", Some("app"), Some("a"), json!({})).is_err());
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": 5})
        )
        .is_err());
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b", "extra": 1})
        )
        .is_err());
        let long = "x".repeat(300);
        assert!(parse(
            "rename_collection",
            Some("app"),
            Some("a"),
            json!({"to": long})
        )
        .is_err());
    }

    #[test]
    fn clone_defaults_to_data_only_and_validates_target() {
        let cloned = parse(
            "clone_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b"}),
        )
        .unwrap();
        assert_eq!(
            cloned,
            AdminMutation::CloneCollection {
                database: "app".into(),
                from: "a".into(),
                to: "b".into(),
                include_indexes: false
            }
        );
        let with_indexes = parse(
            "clone_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b", "include_indexes": true}),
        )
        .unwrap();
        assert!(matches!(
            with_indexes,
            AdminMutation::CloneCollection {
                include_indexes: true,
                ..
            }
        ));
        assert!(parse(
            "clone_collection",
            Some("app"),
            Some("a"),
            json!({"to": "a"})
        )
        .is_err());
        assert!(parse(
            "clone_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b$"})
        )
        .is_err());
        assert!(parse(
            "clone_collection",
            Some("app"),
            Some("a"),
            json!({"to": "b", "include_indexes": "yes"})
        )
        .is_err());
    }

    #[test]
    fn create_index_keeps_key_order_and_validates_options() {
        let parsed = parse(
            "create_index",
            Some("app"),
            Some("orders"),
            json!({
                "keys": [{"field": "zeta", "type": 1}, {"field": "alpha", "type": -1}],
                "options": {"unique": true, "sparse": true, "name": "z_a"}
            }),
        )
        .unwrap();
        let AdminMutation::CreateIndex { spec, .. } = parsed else {
            panic!("create index")
        };
        assert_eq!(spec.keys[0].0, "zeta");
        assert_eq!(spec.keys[1].0, "alpha");
        assert!(spec.unique && spec.sparse);
        let command = create_index_command("orders", &spec);
        let index = command.get_array("indexes").unwrap()[0]
            .as_document()
            .unwrap()
            .clone();
        assert_eq!(index.get_str("name").unwrap(), "z_a");
        assert_eq!(
            index
                .get_document("key")
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["zeta", "alpha"]
        );
        assert_eq!(command.get_str("createIndexes").unwrap(), "orders");
    }

    #[test]
    fn create_index_rejects_invalid_keys_and_options() {
        let create = |payload: serde_json::Value| {
            parse("create_index", Some("app"), Some("orders"), payload)
        };
        assert!(create(json!({})).is_err());
        assert!(create(json!({"keys": []})).is_err());
        assert!(create(json!({"keys": [{"field": "", "type": 1}]})).is_err());
        assert!(create(json!({"keys": [{"field": "$bad", "type": 1}]})).is_err());
        assert!(create(json!({"keys": [{"field": "a\0", "type": 1}]})).is_err());
        assert!(create(json!({"keys": [{"field": "a", "type": 2}]})).is_err());
        assert!(create(json!({"keys": [{"field": "a", "type": "weird"}]})).is_err());
        assert!(
            create(json!({"keys": [{"field": "a", "type": 1}, {"field": "a", "type": -1}]}))
                .is_err()
        );
        let many: Vec<_> = (0..33)
            .map(|i| json!({"field": format!("f{i}"), "type": 1}))
            .collect();
        assert!(create(json!({"keys": many})).is_err());
        let one = json!([{"field": "a", "type": 1}]);
        assert!(create(json!({"keys": one, "options": {"unique": "yes"}})).is_err());
        assert!(create(json!({"keys": one, "options": {"expireAfterSeconds": -1}})).is_err());
        assert!(create(json!({"keys": one, "options": {"expireAfterSeconds": 1.5}})).is_err());
        assert!(create(json!({"keys": one, "options": {"name": "_id_"}})).is_err());
        assert!(create(json!({"keys": one, "options": {"name": "x".repeat(200)}})).is_err());
        assert!(create(json!({"keys": one, "options": {"background": true}})).is_err());
        assert!(create(json!({"keys": one, "options": {"partialFilterExpression": "x"}})).is_err());
        // TTL needs a single ordered key.
        let compound = json!([{"field": "a", "type": 1}, {"field": "b", "type": 1}]);
        assert!(create(json!({"keys": compound, "options": {"expireAfterSeconds": 60}})).is_err());
        let text = json!([{"field": "a", "type": "text"}]);
        assert!(create(json!({"keys": text, "options": {"expireAfterSeconds": 60}})).is_err());
        assert!(create(json!({"keys": one, "options": {"expireAfterSeconds": 60}})).is_ok());
    }

    #[test]
    fn default_index_name_matches_mongodb_convention() {
        let keys = vec![
            ("email".to_string(), Bson::Int32(1)),
            ("bio".to_string(), Bson::String("text".into())),
        ];
        assert_eq!(default_index_name(&keys), "email_1_bio_text");
        let spec = IndexSpec {
            keys: vec![("t".into(), Bson::Int32(-1))],
            name: None,
            unique: false,
            sparse: false,
            expire_after_seconds: Some(3600),
            partial_filter: Some(doc! { "status": "active" }),
        };
        let command = create_index_command("c", &spec);
        let index = command.get_array("indexes").unwrap()[0]
            .as_document()
            .unwrap()
            .clone();
        assert_eq!(index.get_str("name").unwrap(), "t_-1");
        assert_eq!(index.get_i32("expireAfterSeconds").unwrap(), 3600);
        assert!(index.get_document("partialFilterExpression").is_ok());
        assert!(!index.contains_key("unique"));
    }

    #[test]
    fn drop_index_refuses_id_index_and_wildcard() {
        let drop = |name: serde_json::Value| {
            parse(
                "drop_index",
                Some("app"),
                Some("orders"),
                json!({"name": name}),
            )
        };
        assert_eq!(
            drop(json!("email_1")).unwrap(),
            AdminMutation::DropIndex {
                database: "app".into(),
                collection: "orders".into(),
                name: "email_1".into()
            }
        );
        assert!(drop(json!("_id_")).is_err());
        assert!(drop(json!("*")).is_err());
        assert!(drop(json!("")).is_err());
        assert!(drop(json!(7)).is_err());
        assert!(parse("drop_index", Some("app"), Some("orders"), json!({})).is_err());
    }

    #[test]
    fn drop_database_refuses_system_databases_and_extra_fields() {
        assert_eq!(
            parse("drop_database", Some("sales"), None, json!(null)).unwrap(),
            AdminMutation::DropDatabase {
                database: "sales".into()
            }
        );
        for protected in ["admin", "local", "config"] {
            assert!(parse("drop_database", Some(protected), None, json!(null)).is_err());
        }
        assert!(parse("drop_database", None, None, json!(null)).is_err());
        assert!(parse("drop_database", Some(""), None, json!(null)).is_err());
        assert!(parse("drop_database", Some("a\0b"), None, json!(null)).is_err());
        assert!(parse("drop_database", Some("sales"), Some("orders"), json!(null)).is_err());
    }

    #[test]
    fn list_indexes_query_needs_database_and_collection() {
        let query = |database: Option<&str>, object: Option<&str>| QueryRequest {
            kind: QUERY_LIST_INDEXES.into(),
            operation_id: OperationId(1),
            database: database.map(str::to_string),
            object: object.map(str::to_string),
            offset: 0,
            limit: 100,
            payload: json!(null),
        };
        assert_eq!(
            parse_list_indexes(&query(Some("app"), Some("c"))).unwrap(),
            ("app".into(), "c".into())
        );
        assert!(parse_list_indexes(&query(None, Some("c"))).is_err());
        assert!(parse_list_indexes(&query(Some("app"), None)).is_err());
        assert!(parse_list_indexes(&query(Some("app"), Some("a\0"))).is_err());
    }

    #[test]
    fn clone_helpers_never_overwrite_and_skip_the_id_index() {
        let pipeline = clone_pipeline("copy");
        let merge = pipeline[0].get_document("$merge").unwrap();
        assert_eq!(merge.get_str("into").unwrap(), "copy");
        assert_eq!(merge.get_str("whenMatched").unwrap(), "fail");
        assert_eq!(merge.get_str("whenNotMatched").unwrap(), "insert");
        let specs = clonable_index_specs(vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "a": 1 }, "name": "a_1", "unique": true, "ns": "app.c" },
        ]);
        assert_eq!(specs.len(), 1);
        assert!(!specs[0].contains_key("v") && !specs[0].contains_key("ns"));
        assert_eq!(specs[0].get_bool("unique"), Ok(true));
    }

    #[test]
    fn annotate_index_spec_keeps_compound_key_order_in_an_array() {
        let annotated = annotate_index_spec(doc! {
            "v": 2, "name": "z_a", "key": { "zeta": 1, "alpha": -1, "bio": "text" }
        });
        let fields = annotated.get_array("keyFields").unwrap();
        let order: Vec<_> = fields
            .iter()
            .map(|entry| {
                entry
                    .as_document()
                    .unwrap()
                    .get_str("field")
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(order, ["zeta", "alpha", "bio"]);
        assert_eq!(fields[1].as_document().unwrap().get_i32("type"), Ok(-1));
        assert_eq!(fields[2].as_document().unwrap().get_str("type"), Ok("text"));
        assert!(annotate_index_spec(doc! { "name": "x" })
            .get_array("keyFields")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn redact_hides_connection_strings_in_driver_errors() {
        let redacted = redact("failed for mongodb://user:secret@host:27017/db?x=1 timeout");
        assert!(!redacted.contains("secret"));
        assert!(redacted.contains("timeout"));
        assert_eq!(redact("plain failure"), "plain failure");
    }

    #[test]
    fn unknown_admin_kind_is_rejected() {
        assert!(!is_admin_mutation_kind("insert"));
        assert!(is_admin_mutation_kind("drop_index"));
        assert!(parse("truncate", Some("app"), Some("c"), json!(null)).is_err());
    }

    /// End-to-end check against a disposable `mongod`:
    /// `TABLEX_TEST_MONGO_URI=mongodb://127.0.0.1:PORT cargo test -p dbengine-mongo live_ -- --ignored`
    #[test]
    #[ignore = "needs TABLEX_TEST_MONGO_URI pointing at a disposable mongod"]
    fn live_admin_operations_roundtrip() {
        use engine_driver_api::NavigatorRequest;
        let uri = std::env::var("TABLEX_TEST_MONGO_URI").expect("TABLEX_TEST_MONGO_URI");
        let (_, driver) = crate::driver::MongoDriver::connect(&uri).expect("connect");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db = format!("tablex_admin_{stamp}");
        let run = |kind: &str, object: Option<&str>, payload: serde_json::Value| {
            driver
                .mutate(&mutation(kind, Some(&db), object, payload))
                .map_err(|error| error.to_string())
        };
        let collections = || {
            driver
                .list_nav_nodes(&NavigatorRequest {
                    parent: Some(format!("database:{db}")),
                    database: Some(db.clone()),
                    payload: None,
                    cursor: None,
                    limit: 200,
                })
                .unwrap()
                .nodes
                .into_iter()
                .map(|node| node.label)
                .collect::<Vec<_>>()
        };
        for number in 0..3 {
            run(
                "insert",
                Some("src"),
                json!({"email": format!("u{number}@x"), "n": number}),
            )
            .expect("insert");
        }
        run(
            "create_index",
            Some("src"),
            json!({"keys": [{"field": "email", "type": 1}, {"field": "n", "type": -1}], "options": {"unique": true}}),
        )
        .expect("create compound unique index");
        let listed = driver.list_indexes(&db, "src").expect("list indexes");
        let compound = listed
            .iter()
            .find(|spec| spec.get_str("name") == Ok("email_1_n_-1"))
            .expect("default compound name");
        let order: Vec<_> = compound
            .get_array("keyFields")
            .unwrap()
            .iter()
            .map(|entry| {
                entry
                    .as_document()
                    .unwrap()
                    .get_str("field")
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(order, ["email", "n"], "compound key order survives");
        assert_eq!(compound.get_bool("unique"), Ok(true));

        assert_eq!(
            run("rename_collection", Some("src"), json!({"to": "moved"})),
            Ok(1)
        );
        assert!(collections().contains(&"moved".to_string()));
        assert!(!collections().contains(&"src".to_string()));
        assert_eq!(
            run(
                "clone_collection",
                Some("moved"),
                json!({"to": "copy", "include_indexes": true})
            ),
            Ok(3)
        );
        let copied = driver.list_indexes(&db, "copy").unwrap();
        assert!(copied
            .iter()
            .any(|spec| spec.get_str("name") == Ok("email_1_n_-1")));
        let again = run("clone_collection", Some("moved"), json!({"to": "copy"}));
        assert!(
            again.is_err(),
            "an existing target is never overwritten: {again:?}"
        );
        let over = run("rename_collection", Some("copy"), json!({"to": "moved"}));
        assert!(
            over.is_err(),
            "rename never drops an existing target: {over:?}"
        );
        assert_eq!(
            run("clone_collection", Some("moved"), json!({"to": "plain"})),
            Ok(3)
        );
        assert_eq!(
            driver.list_indexes(&db, "plain").unwrap().len(),
            1,
            "data only"
        );

        assert!(run("drop_index", Some("moved"), json!({"name": "_id_"})).is_err());
        assert_eq!(
            run("drop_index", Some("moved"), json!({"name": "email_1_n_-1"})),
            Ok(1)
        );
        let remaining = driver.list_indexes(&db, "moved").unwrap();
        assert!(remaining
            .iter()
            .all(|spec| spec.get_str("name") != Ok("email_1_n_-1")));
        assert_eq!(
            run(
                "create_index",
                Some("moved"),
                json!({"keys": [{"field": "n", "type": 1}], "options": {"expireAfterSeconds": 60, "sparse": true}})
            ),
            Ok(1)
        );
        let ttl = driver.list_indexes(&db, "moved").unwrap();
        let ttl = ttl
            .iter()
            .find(|spec| spec.get_str("name") == Ok("n_1"))
            .unwrap();
        assert_eq!(ttl.get_i32("expireAfterSeconds"), Ok(60));
        assert_eq!(ttl.get_bool("sparse"), Ok(true));

        assert_eq!(run("drop_collection", Some("copy"), json!(null)), Ok(1));
        assert!(!collections().contains(&"copy".to_string()));
        assert_eq!(run("drop_database", None, json!(null)), Ok(1));
        let databases = driver
            .list_nav_nodes(&NavigatorRequest {
                parent: None,
                database: None,
                payload: None,
                cursor: None,
                limit: 200,
            })
            .unwrap();
        assert!(databases.nodes.iter().all(|node| node.label != db));
    }
}
