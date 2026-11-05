use crate::arrow::ArrowCDataResult;
use crate::error::DbEngineError;
use bson::{doc, Bson, Document};
use std::sync::atomic::Ordering;

pub use engine_mongo_script::pipeline::*;


/// Normalize a BSON value to a string for hash-map key comparison.
/// ObjectId → hex string, String → string, numbers → string repr.
/// This handles type mismatches like ObjectId("abc") vs String("abc").
fn normalize_bson_key(val: &Bson) -> String {
    match val {
        Bson::ObjectId(oid) => oid.to_hex(),
        Bson::String(s) => s.clone(),
        Bson::Int32(n) => n.to_string(),
        Bson::Int64(n) => n.to_string(),
        Bson::Double(f) => {
            if f.is_nan() {
                "NaN".to_string()
            } else if f.is_infinite() {
                if *f > 0.0 {
                    "Inf".to_string()
                } else {
                    "-Inf".to_string()
                }
            } else {
                format!("{:.10}", f)
            }
        }
        Bson::Boolean(b) => b.to_string(),
        Bson::Null => "null".to_string(),
        other => format!("{:?}", other),
    }
}

/// Extract unique normalized keys from a list of documents for a given field.
fn extract_unique_keys(docs: &[Document], field: &str) -> Vec<Bson> {
    let mut seen = std::collections::HashSet::new();
    let mut keys = Vec::new();
    for doc in docs {
        if let Some(val) = doc.get(field) {
            let key = normalize_bson_key(val);
            if seen.insert(key) {
                keys.push(val.clone());
            }
        }
    }
    keys
}

/// Execute a compiled plan against MongoDB.
/// - Single: runs one aggregate pipeline.
/// - Federated: orchestrates source aggregate + foreign aggregate + hash join.
pub async fn execute_compiled_plan(
    client: &mongodb::Client,
    db_name: &str,
    plan: CompiledPlan,
    max_time_ms: i64,
) -> Result<ArrowCDataResult, DbEngineError> {
    use futures::StreamExt;

    let cancel_flag = {
        let engine = crate::ENGINE
            .read()
            .map_err(|e| DbEngineError::Query(format!("Engine lock: {}", e)))?;
        engine.cancel_flag.clone()
    };

    match plan {
        CompiledPlan::Single {
            db,
            collection,
            stages,
        } => {
            execute_single_aggregate(
                client,
                db_name,
                &db,
                &collection,
                stages,
                max_time_ms,
                &cancel_flag,
            )
            .await
        }
        CompiledPlan::Federated {
            source,
            pre_join_stages,
            join,
        } => {
            execute_federated(
                client,
                db_name,
                source,
                pre_join_stages,
                join,
                max_time_ms,
                &cancel_flag,
            )
            .await
        }
    }
}

/// Execute a single MongoDB aggregate pipeline.
async fn execute_single_aggregate(
    client: &mongodb::Client,
    db_name: &str,
    source_db: &str,
    collection: &str,
    stages: Vec<Document>,
    max_time_ms: i64,
    cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<ArrowCDataResult, DbEngineError> {
    use futures::StreamExt;

    let db_name_resolved = if source_db == "$current" {
        db_name.to_string()
    } else {
        source_db.to_string()
    };

    let db = client.database(&db_name_resolved);
    let coll = db.collection::<Document>(collection);

    let mut opts = mongodb::options::AggregateOptions::default();
    if max_time_ms > 0 {
        opts.max_time = Some(std::time::Duration::from_millis(max_time_ms as u64));
    }

    let cursor = coll
        .aggregate(stages)
        .with_options(opts)
        .await
        .map_err(|e| DbEngineError::Query(e.to_string()))?;

    let mut docs = Vec::new();
    let mut cancelled = false;
    let mut stream = cursor;
    while let Some(result) = stream.next().await {
        if cancel_flag.load(Ordering::Relaxed) {
            cancel_flag.store(false, Ordering::Relaxed);
            cancelled = true;
            break;
        }
        match result {
            Ok(doc) => docs.push(doc),
            Err(e) => return Err(DbEngineError::Query(format!("Cursor error: {}", e))),
        }
    }

    let mut response = crate::arrow::documents_to_arrow_cdata(&docs)
        .map_err(|e| DbEngineError::Query(e.to_string()))?;

    let column_names = response.column_names.clone();
    let session_id = {
        let mut engine = crate::ENGINE
            .write()
            .map_err(|e| DbEngineError::Query(format!("Engine lock poisoned: {}", e)))?;
        engine.cache_documents(docs, column_names)
    };
    response.total_count = Some(response.row_count);
    response.is_estimated_count = false;
    response.last_id = None;
    response.session_id = session_id;
    response.cancelled = cancelled;
    response.was_truncated = response.row_count as i64 >= MAX_SCRIPT_ROWS;
    Ok(response)
}

/// Execute a federated aggregate: source → extract keys → foreign aggregate → hash join.
async fn execute_federated(
    client: &mongodb::Client,
    db_name: &str,
    source: AggregateSource,
    pre_join_stages: Vec<Document>,
    join: JoinNode,
    max_time_ms: i64,
    cancel_flag: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<ArrowCDataResult, DbEngineError> {
    use futures::StreamExt;

    // 1. Run source aggregate
    let source_db = if source.db == "$current" {
        db_name.to_string()
    } else {
        source.db.clone()
    };
    let source_coll = client
        .database(&source_db)
        .collection::<Document>(&source.collection);

    let mut opts = mongodb::options::AggregateOptions::default();
    if max_time_ms > 0 {
        opts.max_time = Some(std::time::Duration::from_millis(max_time_ms as u64));
    }

    let source_cursor = source_coll
        .aggregate(pre_join_stages)
        .with_options(opts.clone())
        .await
        .map_err(|e| DbEngineError::Query(format!("Source aggregate failed: {}", e)))?;

    let mut source_docs = Vec::new();
    let mut cancelled = false;
    let mut stream = source_cursor;
    while let Some(result) = stream.next().await {
        if cancel_flag.load(Ordering::Relaxed) {
            cancel_flag.store(false, Ordering::Relaxed);
            cancelled = true;
            break;
        }
        match result {
            Ok(doc) => source_docs.push(doc),
            Err(e) => return Err(DbEngineError::Query(format!("Source cursor error: {}", e))),
        }
    }
    if cancelled {
        let mut response = crate::arrow::documents_to_arrow_cdata(&source_docs)
            .map_err(|e| DbEngineError::Query(e.to_string()))?;
        response.cancelled = true;
        return Ok(response);
    }

    // 2. Extract unique join keys from source docs
    let keys = extract_unique_keys(&source_docs, &join.local_field);
    if keys.is_empty() {
        // No keys → no matches possible; set empty arrays and return
        for doc in source_docs.iter_mut() {
            if join.one {
                doc.insert(&join.as_field, Bson::Null);
            } else {
                doc.insert(&join.as_field, Bson::Array(Vec::new()));
            }
        }
        let mut response = crate::arrow::documents_to_arrow_cdata(&source_docs)
            .map_err(|e| DbEngineError::Query(e.to_string()))?;
        let column_names = response.column_names.clone();
        let session_id = {
            let mut engine = crate::ENGINE
                .write()
                .map_err(|e| DbEngineError::Query(format!("Engine lock poisoned: {}", e)))?;
            engine.cache_documents(source_docs, column_names)
        };
        response.total_count = Some(response.row_count);
        response.is_estimated_count = false;
        response.last_id = None;
        response.session_id = session_id;
        response.cancelled = false;
        response.was_truncated = response.row_count as i64 >= MAX_SCRIPT_ROWS;
        return Ok(response);
    }

    // 3. Build foreign aggregate: $match $in (middleware-injected) + user's pipeline
    let mut foreign_stages = vec![doc! { "$match": { &join.foreign_field: { "$in": keys } } }];
    for stage in &join.pipeline {
        foreign_stages.push(stage.clone());
    }

    let foreign_db = if join.foreign.db == "$current" {
        db_name.to_string()
    } else {
        join.foreign.db.clone()
    };
    let foreign_coll = client
        .database(&foreign_db)
        .collection::<Document>(&join.foreign.collection);

    let foreign_cursor = foreign_coll
        .aggregate(foreign_stages)
        .with_options(opts)
        .await
        .map_err(|e| DbEngineError::Query(format!("Foreign aggregate failed: {}", e)))?;

    let mut foreign_docs = Vec::new();
    let mut stream = foreign_cursor;
    while let Some(result) = stream.next().await {
        if cancel_flag.load(Ordering::Relaxed) {
            cancel_flag.store(false, Ordering::Relaxed);
            cancelled = true;
            break;
        }
        match result {
            Ok(doc) => foreign_docs.push(doc),
            Err(e) => return Err(DbEngineError::Query(format!("Foreign cursor error: {}", e))),
        }
    }
    if cancelled {
        let mut response = crate::arrow::documents_to_arrow_cdata(&source_docs)
            .map_err(|e| DbEngineError::Query(e.to_string()))?;
        response.cancelled = true;
        return Ok(response);
    }

    // 4. Hash join: group foreign docs by foreign_field
    let mut lookup_map: std::collections::HashMap<String, Vec<Document>> =
        std::collections::HashMap::new();
    for fdoc in &foreign_docs {
        if let Some(key) = fdoc.get(&join.foreign_field) {
            let key_str = normalize_bson_key(key);
            lookup_map.entry(key_str).or_default().push(fdoc.clone());
        }
    }

    // 5. Merge into source docs
    for doc in source_docs.iter_mut() {
        let matches = if let Some(local_val) = doc.get(&join.local_field) {
            let key_str = normalize_bson_key(local_val);
            lookup_map.get(&key_str).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };

        if join.one {
            doc.insert(
                &join.as_field,
                matches
                    .into_iter()
                    .next()
                    .map(Bson::Document)
                    .unwrap_or(Bson::Null),
            );
        } else {
            doc.insert(
                &join.as_field,
                Bson::Array(matches.into_iter().map(Bson::Document).collect()),
            );
        }
    }

    // 6. Convert to Arrow
    let mut response = crate::arrow::documents_to_arrow_cdata(&source_docs)
        .map_err(|e| DbEngineError::Query(e.to_string()))?;

    let column_names = response.column_names.clone();
    let session_id = {
        let mut engine = crate::ENGINE
            .write()
            .map_err(|e| DbEngineError::Query(format!("Engine lock poisoned: {}", e)))?;
        engine.cache_documents(source_docs, column_names)
    };
    response.total_count = Some(response.row_count);
    response.is_estimated_count = false;
    response.last_id = None;
    response.session_id = session_id;
    response.cancelled = false;
    response.was_truncated = response.row_count as i64 >= MAX_SCRIPT_ROWS;
    Ok(response)
}

/// Legacy executor — single aggregate + old-style cross-DB lookup.
/// Kept for backward compat; new code should use execute_compiled_plan.
pub async fn execute_pipeline(
    client: &mongodb::Client,
    db_name: &str,
    plan: PipelinePlan,
    max_time_ms: i64,
) -> Result<ArrowCDataResult, DbEngineError> {
    let plan = ensure_limit(plan);
    let compiled = plan.compile().map_err(|e| DbEngineError::Query(e))?;
    execute_compiled_plan(client, db_name, compiled, max_time_ms).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_bson_key_objectid_vs_string() {
        let oid = bson::oid::ObjectId::from_bytes([
            0x50, 0x7f, 0x1f, 0x77, 0xbc, 0xf8, 0x6c, 0xd7, 0x99, 0x43, 0x90, 0x11,
        ]);
        let string_val = Bson::String(oid.to_hex());

        assert_eq!(
            normalize_bson_key(&Bson::ObjectId(oid)),
            normalize_bson_key(&string_val)
        );
    }

    #[test]
    fn normalize_bson_key_numbers() {
        assert_eq!(normalize_bson_key(&Bson::Int32(42)), "42");
        assert_eq!(normalize_bson_key(&Bson::Int64(42)), "42");
        assert_eq!(normalize_bson_key(&Bson::String("hello".into())), "hello");
    }

    #[test]
    fn extract_unique_keys_dedupes() {
        let docs = vec![
            doc! { "name": "a", "groupId": 1 },
            doc! { "name": "b", "groupId": 1 },
            doc! { "name": "c", "groupId": 2 },
        ];
        let keys = extract_unique_keys(&docs, "groupId");
        assert_eq!(keys.len(), 2);
    }
}