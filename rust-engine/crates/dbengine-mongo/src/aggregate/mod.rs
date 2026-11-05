//! MongoDB aggregation pipelines: parsing, validation, previews, explain,
//! editability and the `mongo_aggregate` request contract.
//!
//! Everything here is pure (no server, no FFI), so it is unit-tested directly
//! and shared with the GPUI app, which includes this directory with
//! `#[path]` to validate pipeline text while the user types. Only `bson`,
//! `chrono`, `serde` and `serde_json` are used.
//!
//! Corrections relative to the pre-GPUI implementation (`lib.rs`
//! `dbengine_execute_aggregate_arrow` and friends):
//!
//! * previews **always** end in `$limit N`; the old code skipped the cap when
//!   the pipeline already contained any `$limit` (even a huge or mid-pipeline
//!   one) — see [`preview_pipeline`];
//! * `$out` / `$merge` are stripped from every preview, per-stage preview and
//!   `executionStats` explain, and a full run of a pipeline ending in a write
//!   stage never gets `$skip` / `$limit` appended after it — see
//!   [`full_pipeline`], [`explain_pipeline`];
//! * parsing understands shell syntax and returns structured errors — see
//!   [`parse`];
//! * formatting preserves key order — see [`format`].
pub mod format;
pub mod parse;

use bson::{Bson, Document};
use serde::{Deserialize, Serialize};

// Re-exports for the driver and the GPUI app; each consumer uses a subset.
#[allow(unused_imports)]
pub use format::{format_compact, format_ejson_pipeline, format_pipeline, format_stage, format_value};
#[allow(unused_imports)]
pub use parse::{line_col, parse_pipeline, parse_stage, parse_value, ParsedPipeline, PipelineError};

pub const DEFAULT_SAMPLE_SIZE: u64 = 20;
pub const MAX_SAMPLE_SIZE: u64 = 1000;
/// `maxTimeMS` applied to previews unless the request overrides it.
pub const DEFAULT_PREVIEW_MAX_TIME_MS: u64 = 30_000;
/// `maxTimeMS` applied to full runs unless the request overrides it.
pub const DEFAULT_RUN_MAX_TIME_MS: u64 = 60_000;
pub const WRITE_STAGES: [&str; 2] = ["$out", "$merge"];

/// The single operator key of a stage (`None` for an empty / multi-key
/// document).
pub fn stage_operator(stage: &Document) -> Option<&str> {
    if stage.len() == 1 {
        stage.keys().next().map(String::as_str)
    } else {
        None
    }
}

fn write_operator(stage: &Document) -> Option<&'static str> {
    let operator = stage_operator(stage)?;
    WRITE_STAGES.iter().copied().find(|name| *name == operator)
}

/// A `$out` / `$merge` stage and where it writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteStage {
    pub index: usize,
    pub operator: &'static str,
    /// Human readable target: `coll` or `db.coll`.
    pub target: String,
}

fn write_target(operator: &str, stage: &Document) -> String {
    let namespace = |value: &Bson| -> Option<String> {
        match value {
            Bson::String(coll) => Some(coll.clone()),
            Bson::Document(inner) => {
                let coll = inner.get_str("coll").ok()?;
                Some(match inner.get_str("db") {
                    Ok(db) => format!("{db}.{coll}"),
                    Err(_) => coll.to_string(),
                })
            }
            _ => None,
        }
    };
    let spec = stage.get(operator);
    let target = match (operator, spec) {
        ("$out", Some(value)) => namespace(value),
        ("$merge", Some(Bson::Document(inner))) => inner.get("into").and_then(namespace),
        ("$merge", Some(value)) => namespace(value),
        _ => None,
    };
    target.unwrap_or_else(|| "(unknown)".into())
}

/// The write stage when the **last** stage is `$out` / `$merge`.
pub fn final_write_stage(stages: &[Document]) -> Option<WriteStage> {
    let index = stages.len().checked_sub(1)?;
    let operator = write_operator(&stages[index])?;
    Some(WriteStage {
        index,
        operator,
        target: write_target(operator, &stages[index]),
    })
}

/// Structural rules shared by every entry point: each stage is a non-empty
/// object with exactly one key, that key starts with `$`, and `$out` /
/// `$merge` (which the server only allows last) is not followed by anything.
/// `positions` (from [`ParsedPipeline`]) lets errors point into the text;
/// pass `&[]` when there is no source text.
pub fn validate_stages(
    stages: &[Document],
    positions: &[(usize, usize)],
) -> Result<(), PipelineError> {
    let error = |index: usize, message: String| {
        let (line, col) = positions.get(index).copied().unwrap_or((0, 0));
        PipelineError {
            stage: Some(index + 1),
            line,
            col,
            message,
        }
    };
    for (index, stage) in stages.iter().enumerate() {
        if stage.is_empty() {
            return Err(error(index, "Stage is empty.".into()));
        }
        if stage.len() > 1 {
            return Err(error(
                index,
                format!(
                    "Stage has {} keys; each stage must be a single-key object like {{ $match: … }}.",
                    stage.len()
                ),
            ));
        }
        let key = stage.keys().next().map(String::as_str).unwrap_or_default();
        if !key.starts_with('$') {
            return Err(error(
                index,
                format!("Stage key '{key}' must start with '$'."),
            ));
        }
        if write_operator(stage).is_some() && index + 1 != stages.len() {
            return Err(error(
                index,
                format!("{key} must be the last stage of the pipeline."),
            ));
        }
    }
    Ok(())
}

/// Parses and validates pipeline text in one step.
pub fn parse_and_validate(text: &str) -> Result<ParsedPipeline, PipelineError> {
    let parsed = parse_pipeline(text)?;
    validate_stages(&parsed.stages, &parsed.positions)?;
    Ok(parsed)
}

pub fn clamp_sample_size(requested: u64) -> u64 {
    requested.clamp(1, MAX_SAMPLE_SIZE)
}

/// Pipeline for a preview: stages `0..=through_stage` (all when `None`), with
/// every `$out` / `$merge` removed (a preview must never write) and a
/// trailing `$limit sample_size` that is **always** appended.
pub fn preview_pipeline(
    stages: &[Document],
    through_stage: Option<usize>,
    sample_size: u64,
) -> Vec<Document> {
    let end = through_stage
        .map(|index| (index + 1).min(stages.len()))
        .unwrap_or(stages.len());
    let mut pipeline: Vec<Document> = stages[..end]
        .iter()
        .filter(|stage| write_operator(stage).is_none())
        .cloned()
        .collect();
    pipeline.push(bson::doc! { "$limit": clamp_sample_size(sample_size) as i64 });
    pipeline
}

/// Pipeline for one page of a full run, plus what the caller must know.
#[derive(Debug, Clone, PartialEq)]
pub struct FullPlan {
    pub pipeline: Vec<Document>,
    /// Set when the pipeline ends in `$out` / `$merge`: running it writes, so
    /// the caller must hold an explicit confirmation, and no page window is
    /// applied (nothing may follow a write stage).
    pub write: Option<WriteStage>,
    /// Rows to keep from the result; the pipeline fetches one extra row to
    /// learn whether a next page exists.
    pub page_limit: u64,
}

/// Compass-style paging: `$skip offset` (when non-zero) and `$limit limit+1`
/// appended after the user's stages.
pub fn full_pipeline(stages: &[Document], offset: u64, limit: u64) -> FullPlan {
    let limit = limit.max(1);
    if let Some(write) = final_write_stage(stages) {
        return FullPlan {
            pipeline: stages.to_vec(),
            write: Some(write),
            page_limit: limit,
        };
    }
    let mut pipeline = stages.to_vec();
    if offset > 0 {
        pipeline.push(bson::doc! { "$skip": i64::try_from(offset).unwrap_or(i64::MAX) });
    }
    pipeline.push(bson::doc! { "$limit": i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX) });
    FullPlan {
        pipeline,
        write: None,
        page_limit: limit,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainVerbosity {
    QueryPlanner,
    ExecutionStats,
    AllPlansExecution,
}

impl ExplainVerbosity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueryPlanner => "queryPlanner",
            Self::ExecutionStats => "executionStats",
            Self::AllPlansExecution => "allPlansExecution",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "queryPlanner" => Some(Self::QueryPlanner),
            "executionStats" => Some(Self::ExecutionStats),
            "allPlansExecution" => Some(Self::AllPlansExecution),
            _ => None,
        }
    }

    /// Whether the server actually executes the pipeline.
    pub fn executes(self) -> bool {
        !matches!(self, Self::QueryPlanner)
    }
}

/// Stages sent to `explain`. `queryPlanner` never runs the pipeline, so a
/// final `$out` / `$merge` may stay and show up in the plan; the executing
/// verbosities would run it, so it is stripped.
pub fn explain_pipeline(stages: &[Document], verbosity: ExplainVerbosity) -> Vec<Document> {
    if verbosity.executes() {
        stages
            .iter()
            .filter(|stage| write_operator(stage).is_none())
            .cloned()
            .collect()
    } else {
        stages.to_vec()
    }
}

/// `{ explain: { aggregate, pipeline, cursor: {}, … }, verbosity }`.
pub fn explain_command(
    collection: &str,
    pipeline: Vec<Document>,
    verbosity: ExplainVerbosity,
    max_time_ms: Option<u64>,
    allow_disk_use: bool,
    collation: Option<&Document>,
) -> Document {
    let mut inner = bson::doc! {
        "aggregate": collection,
        "pipeline": pipeline,
        "cursor": {},
    };
    if let Some(ms) = max_time_ms {
        inner.insert("maxTimeMS", i64::try_from(ms).unwrap_or(i64::MAX));
    }
    if allow_disk_use {
        inner.insert("allowDiskUse", true);
    }
    if let Some(collation) = collation {
        inner.insert("collation", collation.clone());
    }
    bson::doc! { "explain": inner, "verbosity": verbosity.as_str() }
}

/// Whether editing documents of the output makes sense.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Editability {
    pub editable: bool,
    pub reason: Option<String>,
}

/// Output of a pipeline made only of `$match` / `$sort` / `$skip` / `$limit`
/// still consists of the collection's own documents, so cell edits can be
/// written back by `_id`; any other stage derives new documents. The output
/// must also carry `_id`.
pub fn editability(stages: &[Document], output_has_id: bool) -> Editability {
    let read_only = |reason: String| Editability {
        editable: false,
        reason: Some(reason),
    };
    if !output_has_id {
        return read_only("Output has no _id field.".into());
    }
    for (index, stage) in stages.iter().enumerate() {
        let Some(operator) = stage_operator(stage) else {
            return read_only(format!("Stage {} is not a single-key stage.", index + 1));
        };
        if !matches!(operator, "$match" | "$sort" | "$skip" | "$limit") {
            return read_only(format!("Stage {operator} produces derived output."));
        }
    }
    Editability {
        editable: true,
        reason: None,
    }
}

/// `db.coll.aggregate([...])`, with `db.getCollection("name")` for names that
/// are not plain identifiers.
pub fn mongosh_code(collection: &str, stages: &[Document]) -> String {
    let plain = !collection.is_empty()
        && collection
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
        && collection
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !matches!(
            collection,
            "aggregate" | "find" | "count" | "drop" | "watch" | "stats" | "insert" | "remove"
        );
    let reference = if plain {
        format!("db.{collection}")
    } else {
        format!(
            "db.getCollection({})",
            serde_json::to_string(collection).unwrap_or_else(|_| "\"\"".into())
        )
    };
    format!("{reference}.aggregate({})", format_pipeline(stages))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Preview,
    Full,
}

/// Decoded `mongo_aggregate` payload.
///
/// ```json
/// { "pipeline": "<text>" | ["<stage text>" | {stage}, …],
///   "mode": "preview" | "full",          // default "full"
///   "stage_index": 2,                    // preview only: truncate after stage 2 (0-based)
///   "sample_size": 20,                   // preview, clamped to 1..=1000
///   "max_time_ms": 30000,                // 0 = no limit; default per mode
///   "allow_disk_use": false,
///   "collation": { "locale": "en" } | "<text>",
///   "confirm_write": false }             // required to run a final $out/$merge
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateRequest {
    pub stages: Vec<Document>,
    pub positions: Vec<(usize, usize)>,
    pub mode: RunMode,
    pub stage_index: Option<usize>,
    pub sample_size: u64,
    pub max_time_ms: Option<u64>,
    pub allow_disk_use: bool,
    pub collation: Option<Document>,
    pub confirm_write: bool,
}

fn payload_error(message: impl Into<String>) -> PipelineError {
    PipelineError {
        stage: None,
        line: 0,
        col: 0,
        message: message.into(),
    }
}

fn value_to_bson(value: &serde_json::Value) -> Result<Bson, PipelineError> {
    // JSON is a subset of the relaxed syntax: one parser (Int32 mapping,
    // Extended JSON wrappers) for text and structured input alike.
    parse_value(&value.to_string())
}

pub fn parse_request_payload(payload: &serde_json::Value) -> Result<AggregateRequest, PipelineError> {
    let (stages, positions) = match payload.get("pipeline") {
        Some(serde_json::Value::String(text)) => {
            let parsed = parse_pipeline(text)?;
            (parsed.stages, parsed.positions)
        }
        Some(serde_json::Value::Array(items)) => {
            let mut stages = Vec::with_capacity(items.len());
            for (index, item) in items.iter().enumerate() {
                let with_stage = |mut error: PipelineError| {
                    error.stage = Some(index + 1);
                    error
                };
                let stage = match item {
                    serde_json::Value::String(text) => parse_stage(text).map_err(with_stage)?,
                    other => match value_to_bson(other).map_err(with_stage)? {
                        Bson::Document(document) => document,
                        _ => {
                            return Err(with_stage(payload_error("A stage must be an object.")))
                        }
                    },
                };
                stages.push(stage);
            }
            (stages, Vec::new())
        }
        _ => return Err(payload_error("pipeline must be a string or an array")),
    };
    validate_stages(&stages, &positions)?;
    let mode = match payload.get("mode").and_then(serde_json::Value::as_str) {
        None | Some("full") => RunMode::Full,
        Some("preview") => RunMode::Preview,
        Some(other) => return Err(payload_error(format!("unknown aggregate mode '{other}'"))),
    };
    let stage_index = match payload.get("stage_index") {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => {
            let index = value
                .as_u64()
                .and_then(|index| usize::try_from(index).ok())
                .ok_or_else(|| payload_error("stage_index must be an unsigned integer"))?;
            if mode != RunMode::Preview {
                return Err(payload_error("stage_index is only valid for mode \"preview\""));
            }
            if index >= stages.len() {
                return Err(payload_error(format!(
                    "stage_index {index} is out of range for {} stage(s)",
                    stages.len()
                )));
            }
            Some(index)
        }
    };
    let sample_size = match payload.get("sample_size") {
        None | Some(serde_json::Value::Null) => DEFAULT_SAMPLE_SIZE,
        Some(value) => clamp_sample_size(
            value
                .as_u64()
                .ok_or_else(|| payload_error("sample_size must be an unsigned integer"))?,
        ),
    };
    let max_time_ms = match payload.get("max_time_ms") {
        None | Some(serde_json::Value::Null) => Some(match mode {
            RunMode::Preview => DEFAULT_PREVIEW_MAX_TIME_MS,
            RunMode::Full => DEFAULT_RUN_MAX_TIME_MS,
        }),
        Some(value) => {
            let ms = value
                .as_u64()
                .ok_or_else(|| payload_error("max_time_ms must be an unsigned integer"))?;
            (ms > 0).then_some(ms)
        }
    };
    let allow_disk_use = payload
        .get("allow_disk_use")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let collation = match payload.get("collation") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) if text.trim().is_empty() => None,
        Some(serde_json::Value::String(text)) => Some(
            parse_stage(text).map_err(|error| payload_error(format!("collation: {error}")))?,
        ),
        Some(other) => match value_to_bson(other)? {
            Bson::Document(document) => Some(document),
            _ => return Err(payload_error("collation must be an object")),
        },
    };
    if let Some(collation) = &collation {
        if collation.get_str("locale").is_err() {
            return Err(payload_error("collation requires a string \"locale\""));
        }
    }
    let confirm_write = payload
        .get("confirm_write")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(AggregateRequest {
        stages,
        positions,
        mode,
        stage_index,
        sample_size,
        max_time_ms,
        allow_disk_use,
        collation,
        confirm_write,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;
    use serde_json::json;

    fn stages(text: &str) -> Vec<Document> {
        parse_pipeline(text).unwrap().stages
    }

    // ---- validate -----------------------------------------------------

    #[test]
    fn validate_accepts_single_key_dollar_stages() {
        assert!(parse_and_validate("[{ $match: { a: 1 } }, { $limit: 5 }]").is_ok());
        assert!(parse_and_validate("[]").is_ok());
    }

    #[test]
    fn validate_rejects_bad_stages_with_positions() {
        for (text, needle, stage, line) in [
            ("[{}]", "empty", 1, 1),
            ("[{ $match: {}, $limit: 1 }]", "2 keys", 1, 1),
            ("[{ a: 1 }]", "must start with '$'", 1, 1),
            ("[\n{ $match: {} },\n{ match: {} }]", "must start with '$'", 2, 3),
            ("[{ $out: 'x' }, { $limit: 1 }]", "must be the last stage", 1, 1),
            ("[{ $merge: { into: 'x' } }, { $match: {} }]", "must be the last stage", 1, 1),
        ] {
            let error = parse_and_validate(text).expect_err(text);
            assert_eq!(error.stage, Some(stage), "{text}: {error:?}");
            assert_eq!(error.line, line, "{text}: {error:?}");
            assert!(error.message.contains(needle), "{text}: {error:?}");
        }
    }

    // ---- preview (fix 2 + 3) ------------------------------------------

    #[test]
    fn preview_always_appends_limit_even_when_pipeline_has_one() {
        // Old behaviour: any existing $limit suppressed the cap.
        let pipeline = stages("[{ $match: {} }, { $limit: 1000000 }, { $sort: { a: 1 } }]");
        let preview = preview_pipeline(&pipeline, None, 20);
        assert_eq!(preview.last().unwrap(), &doc! { "$limit": 20_i64 });
        assert_eq!(preview.len(), 4);
        assert_eq!(preview[1], doc! { "$limit": 1_000_000 }, "user stage kept");
    }

    #[test]
    fn preview_truncates_at_stage_and_clamps_sample() {
        let pipeline = stages("[{ $match: {} }, { $sort: { a: 1 } }, { $group: { _id: '$a' } }]");
        let preview = preview_pipeline(&pipeline, Some(1), 5);
        assert_eq!(
            preview,
            vec![
                doc! { "$match": {} },
                doc! { "$sort": { "a": 1 } },
                doc! { "$limit": 5_i64 }
            ]
        );
        assert_eq!(
            preview_pipeline(&pipeline, Some(0), 0).last().unwrap(),
            &doc! { "$limit": 1_i64 }
        );
        assert_eq!(
            preview_pipeline(&pipeline, Some(99), 5_000_000).last().unwrap(),
            &doc! { "$limit": MAX_SAMPLE_SIZE as i64 }
        );
        assert_eq!(preview_pipeline(&[], None, 7), vec![doc! { "$limit": 7_i64 }]);
    }

    #[test]
    fn preview_never_contains_a_write_stage() {
        let pipeline = stages("[{ $match: {} }, { $out: 'copy' }]");
        for through in [None, Some(0), Some(1)] {
            let preview = preview_pipeline(&pipeline, through, 20);
            assert!(
                preview.iter().all(|stage| stage_operator(stage) != Some("$out")),
                "{through:?}: {preview:?}"
            );
            assert_eq!(preview.last().unwrap(), &doc! { "$limit": 20_i64 });
        }
        let merge = stages("[{ $merge: { into: { db: 'a', coll: 'b' } } }]");
        assert_eq!(preview_pipeline(&merge, None, 3), vec![doc! { "$limit": 3_i64 }]);
    }

    // ---- full run -----------------------------------------------------

    #[test]
    fn full_pages_with_skip_and_one_extra_row() {
        let pipeline = stages("[{ $match: { a: 1 } }]");
        let first = full_pipeline(&pipeline, 0, 50);
        assert_eq!(
            first.pipeline,
            vec![doc! { "$match": { "a": 1 } }, doc! { "$limit": 51_i64 }]
        );
        assert!(first.write.is_none());
        let third = full_pipeline(&pipeline, 100, 50);
        assert_eq!(
            third.pipeline[1..],
            [doc! { "$skip": 100_i64 }, doc! { "$limit": 51_i64 }]
        );
        assert_eq!(third.page_limit, 50);
    }

    #[test]
    fn full_run_of_a_write_pipeline_gets_no_skip_or_limit_and_is_flagged() {
        let pipeline = stages("[{ $match: {} }, { $merge: { into: 'dst' } }]");
        let plan = full_pipeline(&pipeline, 40, 20);
        assert_eq!(plan.pipeline, pipeline, "nothing may follow a write stage");
        let write = plan.write.expect("write detected");
        assert_eq!((write.operator, write.index, write.target.as_str()), ("$merge", 1, "dst"));
        let out = full_pipeline(&stages("[{ $out: { db: 'x', coll: 'y' } }]"), 0, 20);
        assert_eq!(out.write.unwrap().target, "x.y");
    }

    // ---- explain (fix 3) ----------------------------------------------

    #[test]
    fn explain_strips_write_stages_only_when_the_server_would_run_them() {
        let pipeline = stages("[{ $match: {} }, { $out: 'copy' }]");
        assert_eq!(explain_pipeline(&pipeline, ExplainVerbosity::QueryPlanner), pipeline);
        for verbosity in [ExplainVerbosity::ExecutionStats, ExplainVerbosity::AllPlansExecution] {
            assert_eq!(
                explain_pipeline(&pipeline, verbosity),
                vec![doc! { "$match": {} }],
                "{verbosity:?}"
            );
        }
    }

    #[test]
    fn explain_command_shape() {
        let command = explain_command(
            "orders",
            vec![doc! { "$match": {} }],
            ExplainVerbosity::ExecutionStats,
            Some(5000),
            true,
            Some(&doc! { "locale": "en" }),
        );
        assert_eq!(
            command,
            doc! {
                "explain": {
                    "aggregate": "orders",
                    "pipeline": [{ "$match": {} }],
                    "cursor": {},
                    "maxTimeMS": 5000_i64,
                    "allowDiskUse": true,
                    "collation": { "locale": "en" },
                },
                "verbosity": "executionStats",
            }
        );
        let plain = explain_command("c", vec![], ExplainVerbosity::QueryPlanner, None, false, None);
        assert_eq!(plain.get_str("verbosity").unwrap(), "queryPlanner");
        assert!(!plain.get_document("explain").unwrap().contains_key("maxTimeMS"));
    }

    #[test]
    fn verbosity_round_trips() {
        for verbosity in [
            ExplainVerbosity::QueryPlanner,
            ExplainVerbosity::ExecutionStats,
            ExplainVerbosity::AllPlansExecution,
        ] {
            assert_eq!(ExplainVerbosity::parse(verbosity.as_str()), Some(verbosity));
        }
        assert_eq!(ExplainVerbosity::parse("nope"), None);
    }

    // ---- editability --------------------------------------------------

    #[test]
    fn editability_follows_the_safe_stage_rule_and_needs_id() {
        let editable = |text: &str, id: bool| editability(&stages(text), id);
        assert!(editable("[{ $match: {} }, { $sort: { a: 1 } }, { $skip: 1 }, { $limit: 2 }]", true).editable);
        assert!(editable("[]", true).editable);
        let derived = editable("[{ $match: {} }, { $group: { _id: '$a' } }]", true);
        assert!(!derived.editable);
        assert_eq!(derived.reason.as_deref(), Some("Stage $group produces derived output."));
        let no_id = editable("[{ $match: {} }]", false);
        assert_eq!(no_id.reason.as_deref(), Some("Output has no _id field."));
        assert!(!editability(&[doc! {}], true).editable, "empty stage is not single-key");
        assert!(!editable("[{ $project: { a: 1 } }]", true).editable);
    }

    // ---- export code --------------------------------------------------

    #[test]
    fn mongosh_code_uses_shell_syntax() {
        let code = mongosh_code(
            "orders",
            &stages("[{ $match: { _id: ObjectId('507f191e810c19729de860ea') } }]"),
        );
        assert_eq!(
            code,
            "db.orders.aggregate([\n  { $match: { _id: ObjectId(\"507f191e810c19729de860ea\") } }\n])"
        );
        assert!(mongosh_code("my-coll", &[]).starts_with("db.getCollection(\"my-coll\").aggregate("));
        assert!(mongosh_code("find", &[]).starts_with("db.getCollection(\"find\")"));
    }

    // ---- request payload ----------------------------------------------

    #[test]
    fn payload_defaults_and_text_pipeline() {
        let request = parse_request_payload(&json!({ "pipeline": "[{ $match: {} }]" })).unwrap();
        assert_eq!(request.mode, RunMode::Full);
        assert_eq!(request.sample_size, DEFAULT_SAMPLE_SIZE);
        assert_eq!(request.max_time_ms, Some(DEFAULT_RUN_MAX_TIME_MS));
        assert!(!request.confirm_write && !request.allow_disk_use);
        let preview = parse_request_payload(
            &json!({ "pipeline": "[{ $match: {} }]", "mode": "preview", "sample_size": 7 }),
        )
        .unwrap();
        assert_eq!(preview.max_time_ms, Some(DEFAULT_PREVIEW_MAX_TIME_MS));
        assert_eq!(preview.sample_size, 7);
        let unlimited =
            parse_request_payload(&json!({ "pipeline": "[]", "max_time_ms": 0 })).unwrap();
        assert_eq!(unlimited.max_time_ms, None);
    }

    #[test]
    fn payload_array_of_stage_texts_or_objects_maps_integers_to_int32() {
        let request = parse_request_payload(&json!({
            "pipeline": ["{ $match: { a: ObjectId('507f191e810c19729de860ea') } }", { "$limit": 5 }],
        }))
        .unwrap();
        assert!(matches!(
            request.stages[0].get_document("$match").unwrap().get("a"),
            Some(Bson::ObjectId(_))
        ));
        assert_eq!(request.stages[1].get("$limit"), Some(&Bson::Int32(5)));
    }

    #[test]
    fn payload_errors_are_structured() {
        let error = parse_request_payload(&json!({ "pipeline": "[{ $match: { a: } }]" })).unwrap_err();
        assert_eq!((error.stage, error.line), (Some(1), 1));
        let error = parse_request_payload(&json!({ "pipeline": ["{ $match: {} }", "{ nope: 1 }"] })).unwrap_err();
        assert_eq!(error.stage, Some(2));
        for payload in [
            json!({}),
            json!({ "pipeline": 5 }),
            json!({ "pipeline": "[]", "mode": "bogus" }),
            json!({ "pipeline": "[]", "mode": "full", "stage_index": 0 }),
            json!({ "pipeline": "[{ $limit: 1 }]", "mode": "preview", "stage_index": 1 }),
            json!({ "pipeline": "[]", "sample_size": "x" }),
            json!({ "pipeline": "[]", "collation": { "strength": 2 } }),
            json!({ "pipeline": "[]", "collation": "{ locale" }),
        ] {
            assert!(parse_request_payload(&payload).is_err(), "{payload}");
        }
    }

    #[test]
    fn payload_collation_text_and_object() {
        let from_text = parse_request_payload(
            &json!({ "pipeline": "[]", "collation": "{ locale: 'en', strength: 2 }" }),
        )
        .unwrap();
        assert_eq!(from_text.collation, Some(doc! { "locale": "en", "strength": 2 }));
        let from_object = parse_request_payload(
            &json!({ "pipeline": "[]", "collation": { "locale": "fr" } }),
        )
        .unwrap();
        assert_eq!(from_object.collation, Some(doc! { "locale": "fr" }));
        let blank = parse_request_payload(&json!({ "pipeline": "[]", "collation": "  " })).unwrap();
        assert_eq!(blank.collation, None);
    }

    #[test]
    fn error_envelope_json_is_built_with_serde_never_string_escaping() {
        // The old FFI formatted `{"valid":false,"error":"…"}` with manual
        // escaping; quotes, backslashes and newlines in the message must
        // survive. serde_json is the only encoder.
        let nasty = "bad \"quote\" back\\slash\nnew\tline \u{1}";
        let error = PipelineError {
            stage: Some(3),
            line: 4,
            col: 5,
            message: nasty.into(),
        };
        let text = serde_json::to_string(&json!({ "valid": false, "error": error })).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["error"]["message"], nasty);
        assert_eq!(back["error"]["stage"], 3);
        assert_eq!(back["error"]["line"], 4);
        assert_eq!(back["error"]["col"], 5);
    }
}
