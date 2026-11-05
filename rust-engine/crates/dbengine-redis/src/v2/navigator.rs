//! Navigator: root -> databases -> keys.

use super::{parse_db, redis_error};
use crate::{
    redis_key_bytes_to_display, redis_key_bytes_to_raw, scan_key_metadata, RedisConn,
    RedisKeyMetadata,
};
use engine_driver_kit::api::{NavigatorNode, NavigatorPage, NavigatorRequest};
use engine_driver_kit::BackendError;
use redis::aio::MultiplexedConnection;

const DEFAULT_KEYS_PER_PAGE: u64 = 500;
const MAX_KEYS_PER_PAGE: u64 = 10_000;
/// `SCAN` calls per page before returning what was found so far with the
/// cursor (a rare `MATCH` on a huge keyspace must not stall the sidebar).
const MAX_SCAN_CALLS: usize = 64;
const MAX_SCAN_COUNT: u64 = 1_000;

pub(super) async fn list(
    connection: &RedisConn,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    match database_of(request)? {
        None => databases(connection).await,
        Some(db) => keys(connection, db, request).await,
    }
}

/// `None` for the root; the db index for a database node.
fn database_of(request: &NavigatorRequest) -> Result<Option<u32>, BackendError> {
    let bad = |message: String| BackendError::with_code("navigator.bad_request", message);
    if let Some(database) = request.database.as_deref().filter(|d| !d.is_empty()) {
        return database
            .trim()
            .parse()
            .map(Some)
            .map_err(|_| bad(format!("`{database}` is not a Redis db index")));
    }
    let payload = request.payload.as_ref();
    if let Some(db) = payload.and_then(|p| p.get("database").or_else(|| p.get("db"))) {
        return parse_db(db)
            .map(Some)
            .ok_or_else(|| bad("payload.database must be a Redis db index".into()));
    }
    match request
        .parent
        .as_deref()
        .filter(|parent| !parent.is_empty())
    {
        None => Ok(None),
        Some(parent) => parent
            .strip_prefix("database:")
            .and_then(|db| db.parse().ok())
            .map(Some)
            .ok_or_else(|| bad(format!("unknown Redis navigator parent `{parent}`"))),
    }
}

async fn databases(connection: &RedisConn) -> Result<NavigatorPage, BackendError> {
    let infos = connection
        .list_database_infos()
        .await
        .map_err(BackendError::new)?;
    let nodes = infos
        .into_iter()
        .map(|info| NavigatorNode {
            id: format!("database:{}", info.db),
            kind: "database".into(),
            label: format!("db{} ({} keys)", info.db, info.keys),
            subtitle: None,
            icon: Some("database".into()),
            has_children: info.keys > 0,
            payload: serde_json::json!({
                "database": info.db.to_string(),
                "db": info.db,
                "keys": info.keys,
            }),
            actions: vec!["refresh".into()],
        })
        .collect();
    Ok(NavigatorPage {
        nodes,
        next_cursor: None,
    })
}

async fn keys(
    connection: &RedisConn,
    db: u32,
    request: &NavigatorRequest,
) -> Result<NavigatorPage, BackendError> {
    let filter = |name: &str| {
        request
            .payload
            .as_ref()
            .and_then(|payload| payload.get(name))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let pattern = filter("pattern");
    let key_type = filter("type");
    let wanted = match request.limit {
        0 => DEFAULT_KEYS_PER_PAGE,
        limit => limit.min(MAX_KEYS_PER_PAGE),
    };
    let mut cursor = request
        .cursor
        .clone()
        .filter(|cursor| !cursor.is_empty())
        .unwrap_or_else(|| "0".into());
    if !cursor.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(BackendError::with_code(
            "navigator.bad_request",
            "cursor must be a Redis SCAN cursor",
        ));
    }

    let mut con = connection.connection.lock().await;
    redis::cmd("SELECT")
        .arg(db)
        .query_async::<()>(&mut *con)
        .await
        .map_err(|error| redis_error("Redis SELECT failed", error))?;
    let mut found: Vec<Vec<u8>> = Vec::new();
    for _ in 0..MAX_SCAN_CALLS {
        let (next, batch) = scan_once(
            &mut con,
            &cursor,
            pattern,
            key_type,
            wanted - found.len() as u64,
        )
        .await?;
        found.extend(batch);
        cursor = next;
        if cursor == "0" || found.len() as u64 >= wanted {
            break;
        }
    }
    let metadata = scan_key_metadata(&mut *con, found)
        .await
        .map_err(BackendError::new)?;
    Ok(NavigatorPage {
        nodes: metadata
            .into_iter()
            .map(|item| key_node(db, item))
            .collect(),
        next_cursor: (cursor != "0").then_some(cursor),
    })
}

async fn scan_once(
    con: &mut MultiplexedConnection,
    cursor: &str,
    pattern: Option<&str>,
    key_type: Option<&str>,
    want: u64,
) -> Result<(String, Vec<Vec<u8>>), BackendError> {
    let mut command = redis::cmd("SCAN");
    command.arg(cursor);
    if let Some(pattern) = pattern {
        command.arg("MATCH").arg(pattern);
    }
    command.arg("COUNT").arg(want.clamp(10, MAX_SCAN_COUNT));
    if let Some(key_type) = key_type {
        command.arg("TYPE").arg(key_type);
    }
    command
        .query_async(con)
        .await
        .map_err(|error| redis_error("Redis SCAN failed", error))
}

fn key_node(db: u32, item: RedisKeyMetadata) -> NavigatorNode {
    let raw = redis_key_bytes_to_raw(&item.key);
    let mut payload = serde_json::json!({
        "database": db.to_string(),
        "db": db,
        "key_raw": raw,
        "key_type": item.key_type,
        "ttl": item.ttl,
        "size": item.size,
    });
    if let Some(preview) = item.value_preview {
        payload["preview"] = preview.into();
    }
    NavigatorNode {
        id: format!("key:{db}:{raw}"),
        kind: "key".into(),
        label: redis_key_bytes_to_display(&item.key),
        icon: Some(item.key_type.clone()),
        subtitle: Some(item.key_type),
        has_children: false,
        payload,
        actions: vec!["open_query".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        parent: Option<&str>,
        database: Option<&str>,
        payload: Option<serde_json::Value>,
    ) -> NavigatorRequest {
        NavigatorRequest {
            parent: parent.map(str::to_owned),
            database: database.map(str::to_owned),
            payload,
            cursor: None,
            limit: 0,
        }
    }

    #[test]
    fn database_resolution_order() {
        assert_eq!(database_of(&request(None, None, None)).ok(), Some(None));
        assert_eq!(database_of(&request(Some(""), None, None)).ok(), Some(None));
        assert_eq!(
            database_of(&request(None, Some("9"), None)).ok(),
            Some(Some(9))
        );
        assert_eq!(
            database_of(&request(Some("database:3"), None, None)).ok(),
            Some(Some(3))
        );
        let payload = serde_json::json!({"database": "5", "pattern": "a*"});
        assert_eq!(
            database_of(&request(Some("database:3"), None, Some(payload))).ok(),
            Some(Some(5))
        );
        assert_eq!(
            database_of(&request(None, None, Some(serde_json::json!({"db": 4})))).ok(),
            Some(Some(4))
        );
        for bad in [
            request(Some("key:1:abc"), None, None),
            request(None, Some("x"), None),
        ] {
            let error = database_of(&bad).err().unwrap();
            assert_eq!(error.code.as_deref(), Some("navigator.bad_request"));
        }
    }

    #[test]
    fn key_node_carries_the_raw_key_type_and_ttl() {
        let node = key_node(
            9,
            RedisKeyMetadata {
                key: b"\xac\xed:1".to_vec(),
                key_type: "hash".into(),
                ttl: Some(60),
                size: Some(3),
                value_preview: None,
            },
        );
        assert_eq!(node.kind, "key");
        assert_eq!(node.subtitle.as_deref(), Some("hash"));
        assert_eq!(node.label, "\\xac\\xed:1");
        assert_eq!(
            node.id,
            format!("key:9:{}", redis_key_bytes_to_raw(b"\xac\xed:1"))
        );
        assert_eq!(node.payload["db"], 9);
        assert_eq!(node.payload["ttl"], 60);
        assert_eq!(node.payload["key_type"], "hash");
        assert!(node.payload.get("preview").is_none());
        assert!(!node.has_children);
    }
}
