//! Aggregate tab model: the pipeline draft a tab edits, kept in two
//! synchronized forms (Text and Stages), the options that travel with it,
//! the `mongo_aggregate` request payloads and the saved-pipeline records.
//!
//! Pure data and functions (no GPUI), so the sync rules are unit-tested.
//! Pipeline syntax, validation, preview truncation and the write-stage rules
//! come from the shared engine module (`mongo_aggregate`, the same file the
//! driver compiles), so what the editor accepts is exactly what the driver
//! runs.
//!
//! # Text <-> Stages
//!
//! * `stages` is the full list, disabled stages included; `text` is the
//!   pipeline of the **enabled** stages only (what runs, what is saved to
//!   `pipeline_json`, what Export Code prints).
//! * Disabled stages persist in [`AggregateOptions::disabled`] with their
//!   absolute position, so the saved `pipeline_json` stays a valid pipeline
//!   for any other reader.
//! * Editing the text replaces the enabled stages; disabled stages are
//!   re-inserted at their old positions ([`AggregateDraft::apply_text`]).
//! * A stage is `{ <operator>: <body> }`; Stages mode edits the body only.
use crate::mongo_aggregate as engine;
use bson::Bson;
use serde::{Deserialize, Serialize};

pub const DEFAULT_SAMPLE_SIZE: u64 = engine::DEFAULT_SAMPLE_SIZE;
pub const MAX_SAMPLE_SIZE: u64 = engine::MAX_SAMPLE_SIZE;
/// Debounce between the last edit and the preview request.
pub const PREVIEW_DEBOUNCE_MS: u64 = 400;
/// Saved-aggregate `kind` written by this editor (the old app's value).
pub const SAVED_KIND: &str = "aggregate";

/// Operators offered by the stage `Select`, with the body a freshly chosen
/// operator starts from (each is valid input for the shared parser).
pub const COMMON_STAGES: &[(&str, &str)] = &[
    ("$match", "{}"),
    ("$group", "{ _id: \"$field\", count: { $sum: 1 } }"),
    ("$project", "{ _id: 1 }"),
    ("$sort", "{ _id: 1 }"),
    ("$limit", "20"),
    ("$skip", "0"),
    ("$unwind", "\"$field\""),
    ("$lookup", "{ from: \"collection\", localField: \"field\", foreignField: \"field\", as: \"result\" }"),
    ("$addFields", "{ newField: \"$field\" }"),
    ("$set", "{ newField: \"$field\" }"),
    ("$unset", "\"field\""),
    ("$count", "\"count\""),
    ("$sample", "{ size: 10 }"),
    ("$replaceRoot", "{ newRoot: \"$field\" }"),
    ("$replaceWith", "\"$field\""),
    ("$sortByCount", "\"$field\""),
    ("$bucket", "{ groupBy: \"$field\", boundaries: [0, 100], default: \"Other\" }"),
    ("$bucketAuto", "{ groupBy: \"$field\", buckets: 5 }"),
    ("$facet", "{ outputField: [{ $match: {} }] }"),
    ("$unionWith", "\"collection\""),
    ("$graphLookup", "{ from: \"collection\", startWith: \"$field\", connectFromField: \"field\", connectToField: \"field\", as: \"result\" }"),
    ("$geoNear", "{ near: { type: \"Point\", coordinates: [0, 0] }, distanceField: \"distance\" }"),
    ("$redact", "\"$$KEEP\""),
    ("$setWindowFields", "{ sortBy: { field: 1 }, output: { total: { $sum: \"$field\" } } }"),
    ("$search", "{ text: { query: \"text\", path: \"field\" } }"),
    ("$merge", "{ into: \"collection\" }"),
    ("$out", "\"collection\""),
];

/// Body a new stage of `operator` starts with (`{}` for unknown operators).
pub fn stage_template(operator: &str) -> &'static str {
    COMMON_STAGES
        .iter()
        .find(|(name, _)| *name == operator)
        .map(|(_, body)| *body)
        .unwrap_or("{}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditorMode {
    #[default]
    Text,
    Stages,
}

/// One stage of the Stages editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDraft {
    /// Stable identity (editors, previews and drags key on it, not on the index).
    pub id: u64,
    pub operator: String,
    pub body: String,
    pub enabled: bool,
}

impl StageDraft {
    /// `{ $match: { a: 1 } }`.
    pub fn text(&self) -> String {
        format!("{{ {}: {} }}", self.operator, self.body.trim())
    }
}

/// A disabled stage as persisted (absolute position in the full list).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DisabledStage {
    pub at: usize,
    pub operator: String,
    pub body: String,
}

/// Settings that travel with a pipeline (`options_json` of a saved
/// aggregate, and the tab's session state).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AggregateOptions {
    pub sample_size: u64,
    /// `maxTimeMS`; `0` = the driver default per mode.
    pub max_time_ms: u64,
    pub allow_disk_use: bool,
    /// Collation as text (`{ locale: "en", strength: 2 }`); empty = none.
    pub collation: String,
    pub disabled: Vec<DisabledStage>,
    /// The pipeline as the user wrote it (mongosh syntax). Only present in
    /// saved rows: `pipeline_json` there is strict relaxed Extended JSON for
    /// older readers, and loading prefers this text when it is there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_text: Option<String>,
}

impl Default for AggregateOptions {
    fn default() -> Self {
        Self {
            sample_size: DEFAULT_SAMPLE_SIZE,
            max_time_ms: 0,
            allow_disk_use: false,
            collation: String::new(),
            disabled: Vec::new(),
            shell_text: None,
        }
    }
}

impl AggregateOptions {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// Tolerant reader: unknown / missing fields use defaults, garbage yields
    /// the defaults (a saved pipeline must always open).
    pub fn from_json(text: Option<&str>) -> Self {
        text.and_then(|text| serde_json::from_str(text).ok())
            .map(|options: Self| Self {
                sample_size: options.sample_size.clamp(1, MAX_SAMPLE_SIZE),
                ..options
            })
            .unwrap_or_default()
    }

    /// Validates the free-text fields the Options popover edits.
    pub fn validate(&self) -> Result<(), String> {
        if self.sample_size == 0 || self.sample_size > MAX_SAMPLE_SIZE {
            return Err(format!("Sample size must be between 1 and {MAX_SAMPLE_SIZE}."));
        }
        if !self.collation.trim().is_empty() {
            let collation = engine::parse_stage(&self.collation)
                .map_err(|error| format!("Collation: {error}"))?;
            if collation.get_str("locale").is_err() {
                return Err("Collation needs a string \"locale\".".into());
            }
        }
        Ok(())
    }
}

/// Parse state of the whole draft, shown inline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DraftCheck {
    /// Text mode: the first syntax / structure error (with line and column).
    pub text_error: Option<engine::PipelineError>,
    /// Stages mode: error message per stage id (body syntax, operator).
    pub stage_errors: Vec<(u64, String)>,
}

impl DraftCheck {
    pub fn is_valid(&self) -> bool {
        self.text_error.is_none() && self.stage_errors.is_empty()
    }

    pub fn stage_error(&self, id: u64) -> Option<&str> {
        self.stage_errors
            .iter()
            .find(|(stage, _)| *stage == id)
            .map(|(_, message)| message.as_str())
    }
}

// Test hook: how many times `AggregateDraft::derive` ran on this thread.
#[cfg(test)]
thread_local! {
    static DERIVE_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn derive_runs() -> usize {
    DERIVE_RUNS.with(std::cell::Cell::get)
}

/// Cached analysis of one draft revision (see [`AggregateDraft::derive`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub revision: u64,
    pub check: DraftCheck,
    pub disabled_count: usize,
    /// Edit-safety of the pipeline alone; combine with "output has `_id`".
    pub editability: engine::Editability,
    pub final_write: Option<engine::WriteStage>,
}

/// Everything a tab persists about its pipeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateDraft {
    pub mode: EditorMode,
    /// The Preview toggle: previews run only while it is on.
    pub preview: bool,
    /// Pipeline of the enabled stages as text.
    pub text: String,
    pub stages: Vec<StageDraft>,
    pub options: AggregateOptions,
    /// `None` = "Untitled".
    pub name: Option<String>,
    pub saved_id: Option<String>,
    pub dirty: bool,
    /// Bumped by every change that can alter what the pipeline means (text,
    /// stages, mode, options). Caches (`Derived`) key on it.
    pub revision: u64,
    next_stage_id: u64,
}

impl Default for AggregateDraft {
    fn default() -> Self {
        Self::new()
    }
}

impl AggregateDraft {
    /// A new, empty pipeline.
    pub fn new() -> Self {
        Self {
            mode: EditorMode::Text,
            preview: true,
            text: "[]".into(),
            stages: Vec::new(),
            options: AggregateOptions::default(),
            name: None,
            saved_id: None,
            dirty: false,
            revision: 1,
            next_stage_id: 1,
        }
    }

    pub fn title(&self) -> &str {
        self.name.as_deref().unwrap_or("Untitled")
    }

    /// Marks a content change (callers that edit `options` directly use it).
    pub fn touch(&mut self) {
        self.revision += 1;
        self.dirty = true;
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_stage_id;
        self.next_stage_id += 1;
        id
    }

    // ---- text <-> stages --------------------------------------------------

    /// Splits parsed stages into drafts (`None` if a stage is not a
    /// single-key document, which the Stages editor cannot show).
    fn drafts_from_docs(&self, docs: &[bson::Document]) -> Option<Vec<(String, String)>> {
        docs.iter()
            .map(|stage| {
                let operator = engine::stage_operator(stage)?;
                let body = engine::format_value(stage.get(operator)?);
                Some((operator.to_string(), body))
            })
            .collect()
    }

    /// Applies edited pipeline text. Returns the parse error when the text is
    /// invalid (the text is kept either way so the user can keep typing; the
    /// stage list only follows valid text).
    pub fn apply_text(&mut self, text: &str) -> Result<(), engine::PipelineError> {
        self.text = text.to_string();
        self.touch();
        let parsed = engine::parse_and_validate(text)?;
        let Some(split) = self.drafts_from_docs(&parsed.stages) else {
            return Ok(());
        };
        self.merge_enabled(split);
        Ok(())
    }

    /// Replaces the enabled stages with `incoming`, reusing ids positionally
    /// and re-inserting disabled stages at their old absolute positions.
    fn merge_enabled(&mut self, incoming: Vec<(String, String)>) {
        let old_enabled: Vec<StageDraft> = self
            .stages
            .iter()
            .filter(|stage| stage.enabled)
            .cloned()
            .collect();
        let disabled: Vec<(usize, StageDraft)> = self
            .stages
            .iter()
            .enumerate()
            .filter(|(_, stage)| !stage.enabled)
            .map(|(index, stage)| (index, stage.clone()))
            .collect();
        let mut merged: Vec<StageDraft> = incoming
            .into_iter()
            .enumerate()
            .map(|(index, (operator, body))| StageDraft {
                id: old_enabled
                    .get(index)
                    .map(|stage| stage.id)
                    .unwrap_or_else(|| self.alloc_id()),
                operator,
                body,
                enabled: true,
            })
            .collect();
        for (at, stage) in disabled {
            merged.insert(at.min(merged.len()), stage);
        }
        self.stages = merged;
    }

    /// Regenerates `text` from the enabled stages (Stages mode edits).
    pub fn sync_text_from_stages(&mut self) {
        self.text = generate_text(&self.stages);
    }

    /// Switches editor mode. Text -> Stages needs a valid pipeline whose
    /// stages are single-key documents.
    pub fn set_mode(&mut self, mode: EditorMode) -> Result<(), String> {
        if mode == self.mode {
            return Ok(());
        }
        match mode {
            EditorMode::Stages => {
                let parsed = engine::parse_and_validate(&self.text)
                    .map_err(|error| format!("Fix the pipeline before switching to Stages: {error}"))?;
                let Some(split) = self.drafts_from_docs(&parsed.stages) else {
                    return Err("Every stage must be a single-operator object to use Stages.".into());
                };
                self.merge_enabled(split);
            }
            EditorMode::Text => self.sync_text_from_stages(),
        }
        self.mode = mode;
        self.revision += 1;
        Ok(())
    }

    // ---- stage operations -------------------------------------------------

    fn changed(&mut self) {
        self.touch();
        self.sync_text_from_stages();
    }

    pub fn stage_index(&self, id: u64) -> Option<usize> {
        self.stages.iter().position(|stage| stage.id == id)
    }

    /// Adds a stage after `after` (or at the end / start). Returns its id.
    pub fn add_stage_after(&mut self, after: Option<u64>, operator: &str) -> u64 {
        let id = self.alloc_id();
        let stage = StageDraft {
            id,
            operator: operator.to_string(),
            body: stage_template(operator).to_string(),
            enabled: true,
        };
        let at = after
            .and_then(|after| self.stage_index(after))
            .map_or(self.stages.len(), |index| index + 1);
        self.stages.insert(at, stage);
        self.changed();
        id
    }

    pub fn delete_stage(&mut self, id: u64) -> bool {
        let Some(index) = self.stage_index(id) else {
            return false;
        };
        self.stages.remove(index);
        self.changed();
        true
    }

    pub fn set_stage_enabled(&mut self, id: u64, enabled: bool) -> bool {
        let Some(index) = self.stage_index(id) else {
            return false;
        };
        if self.stages[index].enabled == enabled {
            return false;
        }
        self.stages[index].enabled = enabled;
        self.changed();
        true
    }

    pub fn set_stage_body(&mut self, id: u64, body: &str) -> bool {
        let Some(index) = self.stage_index(id) else {
            return false;
        };
        if self.stages[index].body == body {
            return false;
        }
        self.stages[index].body = body.to_string();
        self.changed();
        true
    }

    /// Changes the operator; the body restarts from the operator's template
    /// (`reset_body`) or is kept (renaming a custom operator).
    pub fn set_stage_operator(&mut self, id: u64, operator: &str, reset_body: bool) -> bool {
        let Some(index) = self.stage_index(id) else {
            return false;
        };
        if self.stages[index].operator == operator {
            return false;
        }
        self.stages[index].operator = operator.to_string();
        if reset_body {
            self.stages[index].body = stage_template(operator).to_string();
        }
        self.changed();
        true
    }

    /// Moves stage `id` to the position currently held by `target`.
    pub fn move_stage(&mut self, id: u64, target: u64) -> bool {
        let (Some(from), Some(to)) = (self.stage_index(id), self.stage_index(target)) else {
            return false;
        };
        if from == to {
            return false;
        }
        let stage = self.stages.remove(from);
        self.stages.insert(to, stage);
        self.changed();
        true
    }

    // ---- derived values ---------------------------------------------------

    pub fn enabled_stages(&self) -> impl Iterator<Item = &StageDraft> {
        self.stages.iter().filter(|stage| stage.enabled)
    }

    pub fn disabled_count(&self) -> usize {
        self.stages.iter().filter(|stage| !stage.enabled).count()
    }

    /// Index of stage `id` among the enabled stages (what the driver's
    /// `stage_index` counts), `None` for disabled / unknown stages.
    pub fn enabled_index(&self, id: u64) -> Option<usize> {
        self.enabled_stages().position(|stage| stage.id == id)
    }

    /// The pipeline text requests carry: the user's own text in Text mode,
    /// generated from the enabled stages in Stages mode.
    pub fn pipeline_text(&self) -> String {
        match self.mode {
            EditorMode::Text => self.text.clone(),
            EditorMode::Stages => generate_text(&self.stages),
        }
    }

    /// Syntax / structure check of the current mode's source.
    pub fn check(&self) -> DraftCheck {
        self.analyze().0
    }

    /// Everything the view needs about the current content, computed in ONE
    /// parse pass. Callers cache it per [`Self::revision`]; rendering must
    /// never call this.
    pub fn derive(&self) -> Derived {
        #[cfg(test)]
        DERIVE_RUNS.with(|runs| runs.set(runs.get() + 1));
        let (check, stages) = self.analyze();
        Derived {
            revision: self.revision,
            disabled_count: self.disabled_count(),
            // `_id` presence is a property of the result and is added at render.
            editability: match &stages {
                Some(stages) => engine::editability(stages, true),
                None => engine::Editability {
                    editable: false,
                    reason: Some("Pipeline cannot be parsed for edit safety.".into()),
                },
            },
            final_write: stages.as_deref().and_then(engine::final_write_stage),
            check,
        }
    }

    /// The check plus the parsed enabled stages when the content is valid.
    fn analyze(&self) -> (DraftCheck, Option<Vec<bson::Document>>) {
        match self.mode {
            EditorMode::Text => match engine::parse_and_validate(&self.text) {
                Ok(parsed) => (DraftCheck::default(), Some(parsed.stages)),
                Err(error) => (
                    DraftCheck {
                        text_error: Some(error),
                        stage_errors: Vec::new(),
                    },
                    None,
                ),
            },
            EditorMode::Stages => self.check_stages(),
        }
    }

    fn check_stages(&self) -> (DraftCheck, Option<Vec<bson::Document>>) {
        let mut stage_errors = Vec::new();
        let mut docs = Vec::new();
        let mut ids = Vec::new();
        for stage in self.enabled_stages() {
            if !stage.operator.starts_with('$') {
                stage_errors.push((
                    stage.id,
                    format!("Operator '{}' must start with '$'.", stage.operator),
                ));
                continue;
            }
            match engine::parse_value(&stage.body) {
                Ok(value) => {
                    let mut doc = bson::Document::new();
                    doc.insert(stage.operator.clone(), value);
                    docs.push(doc);
                    ids.push(stage.id);
                }
                Err(error) => stage_errors.push((
                    stage.id,
                    format!("Line {}, col {}: {}", error.line, error.col, error.message),
                )),
            }
        }
        if stage_errors.is_empty() {
            if let Err(error) = engine::validate_stages(&docs, &[]) {
                if let Some(id) = error.stage.and_then(|stage| ids.get(stage - 1)) {
                    stage_errors.push((*id, error.message));
                }
            }
        }
        let valid = stage_errors.is_empty();
        (
            DraftCheck {
                text_error: None,
                stage_errors,
            },
            valid.then_some(docs),
        )
    }

    /// Parsed stages of the enabled pipeline (valid drafts only).
    pub fn parsed_stages(&self) -> Result<Vec<bson::Document>, engine::PipelineError> {
        engine::parse_and_validate(&self.pipeline_text()).map(|parsed| parsed.stages)
    }

    /// The final `$out` / `$merge`, if the enabled pipeline ends in one.
    pub fn final_write(&self) -> Option<engine::WriteStage> {
        self.parsed_stages()
            .ok()
            .and_then(|stages| engine::final_write_stage(&stages))
    }

    #[cfg(test)]
    /// Whether Run output may be edited in place (see `engine::editability`).
    pub fn editability(&self, output_has_id: bool) -> engine::Editability {
        match self.parsed_stages() {
            Ok(stages) => engine::editability(&stages, output_has_id),
            Err(_) => engine::Editability {
                editable: false,
                reason: Some("Pipeline cannot be parsed for edit safety.".into()),
            },
        }
    }

    /// `db.coll.aggregate([...])` for Export Code.
    pub fn mongosh_code(&self, collection: &str) -> Result<String, engine::PipelineError> {
        self.parsed_stages()
            .map(|stages| engine::mongosh_code(collection, &stages))
    }

    /// Pretty text of the enabled pipeline (Format button), key order kept.
    pub fn formatted_text(&self) -> Result<String, engine::PipelineError> {
        self.parsed_stages()
            .map(|stages| engine::format_pipeline(&stages))
    }

    // ---- request payloads -------------------------------------------------

    fn base_payload(&self) -> serde_json::Value {
        let mut payload = serde_json::json!({
            "pipeline": self.pipeline_text(),
            "sample_size": self.options.sample_size,
            "allow_disk_use": self.options.allow_disk_use,
        });
        if self.options.max_time_ms > 0 {
            payload["max_time_ms"] = self.options.max_time_ms.into();
        }
        if !self.options.collation.trim().is_empty() {
            payload["collation"] = self.options.collation.clone().into();
        }
        payload
    }

    /// `mongo_aggregate` preview payload (`through` = enabled-stage index for
    /// a per-stage preview).
    pub fn preview_payload(&self, through: Option<usize>) -> serde_json::Value {
        let mut payload = self.base_payload();
        payload["mode"] = "preview".into();
        if let Some(index) = through {
            payload["stage_index"] = index.into();
        }
        payload
    }

    /// `mongo_aggregate` full-run payload.
    pub fn run_payload(&self, confirm_write: bool) -> serde_json::Value {
        let mut payload = self.base_payload();
        payload["mode"] = "full".into();
        if confirm_write {
            payload["confirm_write"] = true.into();
        }
        payload
    }

    #[cfg(test)]
    /// `mongo_aggregate` payload of Export Data: the whole result through one
    /// cursor. It sends **no** `maxTimeMS` unless the user set one (a long
    /// export must not die at the 60 s a Run page defaults to).
    pub fn export_payload(&self) -> serde_json::Value {
        let mut payload = self.base_payload();
        payload["mode"] = "full".into();
        if self.options.max_time_ms == 0 {
            payload["max_time_ms"] = 0.into();
        }
        payload
    }

    /// `mongo_aggregate_explain` payload.
    pub fn explain_payload(&self, verbosity: engine::ExplainVerbosity) -> serde_json::Value {
        let mut payload = self.base_payload();
        payload["verbosity"] = verbosity.as_str().into();
        payload
    }

    // ---- persistence ------------------------------------------------------

    /// Options as stored: the editor settings plus the disabled stages.
    pub fn stored_options(&self) -> AggregateOptions {
        AggregateOptions {
            disabled: self
                .stages
                .iter()
                .enumerate()
                .filter(|(_, stage)| !stage.enabled)
                .map(|(at, stage)| DisabledStage {
                    at,
                    operator: stage.operator.clone(),
                    body: stage.body.clone(),
                })
                .collect(),
            ..self.options.clone()
        }
    }

    /// Rebuilds a draft from stored text + options (saved pipeline or session).
    pub fn restore(
        text: &str,
        options: AggregateOptions,
        mode: EditorMode,
        preview: bool,
        name: Option<String>,
        saved_id: Option<String>,
    ) -> Self {
        let mut draft = Self::new();
        draft.preview = preview;
        draft.name = name;
        draft.saved_id = saved_id;
        draft.text = text.to_string();
        let disabled = options.disabled.clone();
        draft.options = AggregateOptions {
            disabled: Vec::new(),
            shell_text: None,
            ..options
        };
        if let Ok(parsed) = engine::parse_and_validate(text) {
            if let Some(split) = draft.drafts_from_docs(&parsed.stages) {
                draft.merge_enabled(split);
                for stage in disabled {
                    let id = draft.alloc_id();
                    let at = stage.at.min(draft.stages.len());
                    draft.stages.insert(
                        at,
                        StageDraft {
                            id,
                            operator: stage.operator,
                            body: stage.body,
                            enabled: false,
                        },
                    );
                }
            }
        }
        // Stages mode needs a stage list; an unparsable text stays in Text.
        draft.mode = if mode == EditorMode::Stages && engine::parse_and_validate(text).is_ok() {
            EditorMode::Stages
        } else {
            EditorMode::Text
        };
        draft.dirty = false;
        draft
    }

    /// Record for the saved-aggregates table.
    #[allow(clippy::too_many_arguments)] // plain row fields; mirrors the storage API
    pub fn to_saved(
        &self,
        id: &str,
        connection: &str,
        database: &str,
        collection: &str,
        name: &str,
        created_at: f64,
        updated_at: f64,
    ) -> engine_storage::aggregate::SavedAggregate {
        engine_storage::aggregate::SavedAggregate {
            id: id.into(),
            conn_uuid: connection.into(),
            database: database.into(),
            collection: collection.into(),
            name: name.into(),
            kind: SAVED_KIND.into(),
            // The effective pipeline as strict relaxed Extended JSON, which
            // any reader of this table (the previous app's json5 reader
            // included) can parse; disabled stages and the user's own text
            // live in `options_json`.
            pipeline_json: match self.parsed_stages() {
                Ok(stages) => engine::format_ejson_pipeline(&stages),
                Err(_) => self.pipeline_text(),
            },
            created_at,
            updated_at,
            options_json: Some(
                AggregateOptions {
                    shell_text: Some(self.pipeline_text()),
                    ..self.stored_options()
                }
                .to_json(),
            ),
        }
    }

    pub fn from_saved(saved: &engine_storage::aggregate::SavedAggregate) -> Self {
        let mut options = AggregateOptions::from_json(saved.options_json.as_deref());
        // Prefer what the user wrote; rows from older versions only have
        // `pipeline_json`.
        let text = options
            .shell_text
            .take()
            .unwrap_or_else(|| saved.pipeline_json.clone());
        Self::restore(
            &text,
            options,
            EditorMode::Text,
            true,
            Some(saved.name.clone()),
            Some(saved.id.clone()),
        )
    }
}

/// `[ {stage}, ... ]` of the enabled stages.
pub fn generate_text(stages: &[StageDraft]) -> String {
    let enabled: Vec<String> = stages
        .iter()
        .filter(|stage| stage.enabled)
        .map(StageDraft::text)
        .collect();
    if enabled.is_empty() {
        return "[]".into();
    }
    format!("[\n  {}\n]", enabled.join(",\n  "))
}

/// Per-document text of a preview, plus the `serde_json` form the tree view
/// flattens. Decoded once when a preview arrives (never per frame).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PreviewDocs {
    pub docs: Vec<serde_json::Value>,
}

impl PreviewDocs {
    pub fn from_documents(documents: &[bson::Document]) -> Self {
        Self {
            docs: documents
                .iter()
                .map(|document| Bson::Document(document.clone()).into_canonical_extjson())
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// All documents as one JSON array value (tree / JSON views).
    pub fn as_array(&self) -> serde_json::Value {
        serde_json::Value::Array(self.docs.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(text: &str) -> AggregateDraft {
        let mut draft = AggregateDraft::new();
        draft.apply_text(text).expect("valid pipeline");
        draft
    }

    fn operators(draft: &AggregateDraft) -> Vec<(&str, bool)> {
        draft
            .stages
            .iter()
            .map(|stage| (stage.operator.as_str(), stage.enabled))
            .collect()
    }

    #[test]
    fn every_template_is_valid_input_for_the_shared_parser() {
        for (operator, body) in COMMON_STAGES {
            assert!(operator.starts_with('$'));
            engine::parse_value(body).unwrap_or_else(|error| panic!("{operator}: {error}"));
        }
        let mut seen = std::collections::HashSet::new();
        assert!(COMMON_STAGES.iter().all(|(operator, _)| seen.insert(*operator)));
        assert_eq!(stage_template("$nope"), "{}");
    }

    #[test]
    fn text_edits_follow_into_stages_and_keep_ids() {
        let mut draft = draft("[{ $match: { a: 1 } }, { $limit: 5 }]");
        assert_eq!(operators(&draft), [("$match", true), ("$limit", true)]);
        let ids: Vec<u64> = draft.stages.iter().map(|stage| stage.id).collect();
        draft
            .apply_text("[{ $match: { a: 2 } }, { $limit: 7 }, { $skip: 1 }]")
            .unwrap();
        assert_eq!(draft.stages[0].id, ids[0]);
        assert_eq!(draft.stages[1].id, ids[1]);
        assert_eq!(draft.stages[2].operator, "$skip");
        assert!(!ids.contains(&draft.stages[2].id));
        assert_eq!(draft.stages[0].body, "{ a: 2 }");
        assert!(draft.dirty);
    }

    #[test]
    fn invalid_text_is_kept_but_the_stage_list_does_not_change() {
        let mut draft = draft("[{ $match: {} }]");
        let before = draft.stages.clone();
        let error = draft.apply_text("[{ $match: {} ").unwrap_err();
        assert!(error.message.contains("Unterminated"));
        assert_eq!(draft.text, "[{ $match: {} ");
        assert_eq!(draft.stages, before);
    }

    #[test]
    fn stage_edits_regenerate_the_text() {
        let mut draft = draft("[{ $match: {} }]");
        let first = draft.stages[0].id;
        let second = draft.add_stage_after(Some(first), "$sort");
        assert_eq!(
            draft.text,
            "[\n  { $match: {} },\n  { $sort: { _id: 1 } }\n]"
        );
        assert!(draft.set_stage_body(second, "{ name: -1 }"));
        assert!(draft.text.contains("{ $sort: { name: -1 } }"));
        assert!(!draft.set_stage_body(second, "{ name: -1 }"), "no-op is not a change");
        assert!(draft.set_stage_operator(second, "$limit", true));
        assert!(draft.text.contains("{ $limit: 20 }"));
        assert!(draft.delete_stage(first));
        assert_eq!(draft.text, "[\n  { $limit: 20 }\n]");
        assert!(draft.delete_stage(second));
        assert_eq!(draft.text, "[]");
        assert!(!draft.delete_stage(999));
    }

    #[test]
    fn disabling_hides_a_stage_from_text_but_keeps_it_in_the_list() {
        let mut draft = draft("[{ $match: {} }, { $sort: { a: 1 } }, { $limit: 3 }]");
        let sort = draft.stages[1].id;
        assert!(draft.set_stage_enabled(sort, false));
        assert_eq!(draft.disabled_count(), 1);
        assert_eq!(draft.text, "[\n  { $match: {} },\n  { $limit: 3 }\n]");
        assert_eq!(draft.enabled_index(draft.stages[2].id), Some(1), "driver stage_index skips disabled");
        assert_eq!(draft.enabled_index(sort), None);
        // Text edit keeps the disabled stage at its old position.
        draft
            .apply_text("[{ $match: { x: 1 } }, { $limit: 9 }, { $skip: 2 }]")
            .unwrap();
        assert_eq!(
            operators(&draft),
            [("$match", true), ("$sort", false), ("$limit", true), ("$skip", true)]
        );
        assert!(draft.set_stage_enabled(sort, true));
        assert!(draft.text.contains("$sort"));
    }

    #[test]
    fn mode_switches_roundtrip_and_refuse_broken_text() {
        let mut draft = draft("[{ $match: { a: 1 } }]");
        draft.set_mode(EditorMode::Stages).unwrap();
        let id = draft.stages[0].id;
        draft.set_stage_body(id, "{ a: 2 }");
        draft.set_mode(EditorMode::Text).unwrap();
        assert!(draft.text.contains("{ $match: { a: 2 } }"));
        draft.apply_text("[{ $match: ").unwrap_err();
        let error = draft.set_mode(EditorMode::Stages).unwrap_err();
        assert!(error.starts_with("Fix the pipeline"), "{error}");
        assert_eq!(draft.mode, EditorMode::Text);
        // A multi-operator stage cannot be shown as a card (the structural
        // rule rejects it first, with the position).
        let mut multi = AggregateDraft::new();
        multi.text = "[{ $match: {}, $limit: 1 }]".into();
        assert!(multi.set_mode(EditorMode::Stages).is_err());
    }

    #[test]
    fn move_stage_reorders_and_regenerates_text() {
        let mut draft = draft("[{ $match: {} }, { $sort: { a: 1 } }, { $limit: 3 }]");
        let (first, last) = (draft.stages[0].id, draft.stages[2].id);
        assert!(draft.move_stage(last, first));
        assert_eq!(operators(&draft), [("$limit", true), ("$match", true), ("$sort", true)]);
        assert!(draft.text.starts_with("[\n  { $limit: 3 }"));
        assert!(!draft.move_stage(first, first));
        assert!(!draft.move_stage(first, 999));
    }

    #[test]
    fn check_reports_text_and_per_stage_errors_with_positions() {
        let mut broken = AggregateDraft::new();
        broken.text = "[\n  { $match: { a: } }\n]".into();
        let check = broken.check();
        let error = check.text_error.expect("text error");
        assert_eq!((error.stage, error.line), (Some(1), 2));
        assert!(draft("[{ $match: {} }]").check().is_valid());

        let mut stages = draft("[{ $match: {} }, { $limit: 2 }]");
        stages.set_mode(EditorMode::Stages).unwrap();
        let (first, second) = (stages.stages[0].id, stages.stages[1].id);
        stages.set_stage_body(second, "{ oops: }");
        let check = stages.check();
        assert!(check.stage_error(first).is_none());
        let message = check.stage_error(second).expect("stage error");
        assert!(message.starts_with("Line 1, col"), "{message}");
        stages.set_stage_body(second, "2");
        stages.set_stage_operator(first, "match", false);
        assert!(stages.check().stage_error(first).unwrap().contains("must start with '$'"));
        // $out in the middle is a structural error attributed to its card.
        stages.set_stage_operator(first, "$out", true);
        assert!(stages.check().stage_error(first).unwrap().contains("last stage"));
    }

    #[test]
    fn payloads_carry_pipeline_options_and_stage_index() {
        let mut draft = draft("[{ $match: {} }, { $sort: { a: 1 } }, { $limit: 3 }]");
        draft.options = AggregateOptions {
            sample_size: 50,
            max_time_ms: 20_000,
            allow_disk_use: true,
            collation: "{ locale: 'en' }".into(),
            disabled: Vec::new(),
            shell_text: None,
        };
        let preview = draft.preview_payload(Some(1));
        assert_eq!(preview["mode"], "preview");
        assert_eq!(preview["stage_index"], 1);
        assert_eq!(preview["sample_size"], 50);
        assert_eq!(preview["max_time_ms"], 20_000);
        assert_eq!(preview["allow_disk_use"], true);
        assert_eq!(preview["collation"], "{ locale: 'en' }");
        assert_eq!(preview["pipeline"], "[{ $match: {} }, { $sort: { a: 1 } }, { $limit: 3 }]");
        let run = draft.run_payload(true);
        assert_eq!((run["mode"].as_str(), run["confirm_write"].as_bool()), (Some("full"), Some(true)));
        assert!(run.get("stage_index").is_none());
        assert!(draft.run_payload(false).get("confirm_write").is_none());
        let explain = draft.explain_payload(engine::ExplainVerbosity::ExecutionStats);
        assert_eq!(explain["verbosity"], "executionStats");
        // The driver accepts every payload we build.
        for payload in [&preview, &run, &explain] {
            engine::parse_request_payload(payload).unwrap_or_else(|error| panic!("{payload}: {error}"));
        }
    }

    #[test]
    fn payload_defaults_omit_unset_options() {
        let draft = draft("[]");
        let payload = draft.run_payload(false);
        assert!(payload.get("max_time_ms").is_none());
        assert!(payload.get("collation").is_none());
    }

    #[test]
    fn stages_mode_requests_use_the_generated_text_without_disabled_stages() {
        let mut draft = draft("[{ $match: {} }, { $sort: { a: 1 } }]");
        draft.set_mode(EditorMode::Stages).unwrap();
        let sort = draft.stages[1].id;
        draft.set_stage_enabled(sort, false);
        let payload = draft.run_payload(false);
        assert_eq!(payload["pipeline"], "[\n  { $match: {} }\n]");
        let request = engine::parse_request_payload(&payload).unwrap();
        assert_eq!(request.stages.len(), 1);
    }

    #[test]
    fn final_write_and_editability_come_from_the_enabled_pipeline() {
        let mut draft = draft("[{ $match: {} }, { $out: 'copy' }]");
        let write = draft.final_write().expect("write stage");
        assert_eq!((write.operator, write.target.as_str()), ("$out", "copy"));
        assert!(!draft.editability(true).editable);
        draft.set_mode(EditorMode::Stages).unwrap();
        let out = draft.stages[1].id;
        draft.set_stage_enabled(out, false);
        assert!(draft.final_write().is_none(), "a disabled $out does not run");
        assert!(draft.editability(true).editable);
        assert!(!draft.editability(false).editable);
    }

    #[test]
    fn export_code_and_format_preserve_key_order() {
        let draft = draft("[{ $sort: { zeta: 1, alpha: -1 } }]");
        let code = draft.mongosh_code("orders").unwrap();
        assert_eq!(code, "db.orders.aggregate([\n  { $sort: { zeta: 1, alpha: -1 } }\n])");
        assert_eq!(
            draft.formatted_text().unwrap(),
            "[\n  { $sort: { zeta: 1, alpha: -1 } }\n]"
        );
        let mut broken = AggregateDraft::new();
        broken.text = "[".into();
        assert!(broken.mongosh_code("c").is_err());
    }

    #[test]
    fn options_json_round_trips_and_tolerates_garbage() {
        let options = AggregateOptions {
            sample_size: 100,
            max_time_ms: 5000,
            allow_disk_use: true,
            collation: "{ locale: 'fr' }".into(),
            disabled: vec![DisabledStage { at: 2, operator: "$sort".into(), body: "{ a: 1 }".into() }],
            shell_text: None,
        };
        assert_eq!(AggregateOptions::from_json(Some(&options.to_json())), options);
        assert_eq!(AggregateOptions::from_json(None), AggregateOptions::default());
        assert_eq!(AggregateOptions::from_json(Some("not json")), AggregateOptions::default());
        assert_eq!(
            AggregateOptions::from_json(Some(r#"{"sample_size": 99999}"#)).sample_size,
            MAX_SAMPLE_SIZE,
            "out-of-range sample size is clamped on load"
        );
        assert_eq!(
            AggregateOptions::from_json(Some(r#"{"future_field": 1}"#)),
            AggregateOptions::default()
        );
    }

    #[test]
    fn options_validation_rejects_bad_values() {
        let mut options = AggregateOptions::default();
        assert!(options.validate().is_ok());
        options.sample_size = 0;
        assert!(options.validate().is_err());
        options.sample_size = MAX_SAMPLE_SIZE + 1;
        assert!(options.validate().is_err());
        options.sample_size = 10;
        options.collation = "{ locale".into();
        assert!(options.validate().unwrap_err().starts_with("Collation"));
        options.collation = "{ strength: 2 }".into();
        assert!(options.validate().unwrap_err().contains("locale"));
        options.collation = "{ locale: 'en' }".into();
        assert!(options.validate().is_ok());
    }

    #[test]
    fn saved_record_round_trips_with_disabled_stages_and_stays_a_valid_pipeline() {
        let mut original = draft("[{ $match: { a: 1 } }, { $sort: { a: 1 } }, { $limit: 3 }]");
        original.options.sample_size = 40;
        original.set_mode(EditorMode::Stages).unwrap();
        let sort = original.stages[1].id;
        original.set_stage_enabled(sort, false);
        original.name = Some("Top orders".into());
        let saved = original.to_saved("id-1", "conn", "db", "orders", "Top orders", 1.0, 2.0);
        assert_eq!(saved.kind, "aggregate");
        assert_eq!(saved.database, "db");
        // Other readers see the effective pipeline only.
        let effective = engine::parse_pipeline(&saved.pipeline_json).unwrap();
        assert_eq!(effective.stages.len(), 2);
        assert!(!saved.pipeline_json.contains("$sort"));

        let restored = AggregateDraft::from_saved(&saved);
        assert_eq!(restored.name.as_deref(), Some("Top orders"));
        assert_eq!(restored.saved_id.as_deref(), Some("id-1"));
        assert_eq!(restored.options.sample_size, 40);
        assert_eq!(
            operators(&restored),
            [("$match", true), ("$sort", false), ("$limit", true)]
        );
        assert_eq!(restored.stages[1].body, "{ a: 1 }");
        assert!(!restored.dirty);
        // Saving the restored draft is stable.
        let again = restored.to_saved("id-1", "conn", "db", "orders", "Top orders", 1.0, 2.0);
        assert_eq!(again.pipeline_json, saved.pipeline_json);
        assert_eq!(again.options_json, saved.options_json);
    }

    #[test]
    fn restore_falls_back_to_text_mode_for_unparsable_text() {
        let restored = AggregateDraft::restore(
            "[{ $match:",
            AggregateOptions::default(),
            EditorMode::Stages,
            true,
            None,
            None,
        );
        assert_eq!(restored.mode, EditorMode::Text);
        assert_eq!(restored.text, "[{ $match:");
        assert!(restored.stages.is_empty());
    }

    #[test]
    fn preview_docs_decode_to_canonical_extended_json() {
        let docs = PreviewDocs::from_documents(&[
            bson::doc! { "_id": bson::oid::ObjectId::parse_str("507f191e810c19729de860ea").unwrap(), "n": 3 },
        ]);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs.docs[0]["_id"]["$oid"], "507f191e810c19729de860ea");
        assert_eq!(docs.as_array().as_array().unwrap().len(), 1);
    }

    #[test]
    fn saved_pipeline_json_is_strict_json_and_shell_text_round_trips_through_options() {
        let original = draft("[{ $match: { _id: ObjectId('507f191e810c19729de860ea'), at: ISODate('2024-01-15T10:30:00Z'), n: NumberLong('7') } }, { $sort: { zeta: 1, alpha: -1 } }]");
        let saved = original.to_saved("id", "c", "d", "coll", "n", 1.0, 1.0);
        // What the previous app's json5 reader gets: plain JSON, no shell syntax.
        let value: serde_json::Value = serde_json::from_str(&saved.pipeline_json).expect("strict JSON");
        assert_eq!(value[0]["$match"]["_id"]["$oid"], "507f191e810c19729de860ea");
        assert!(!saved.pipeline_json.contains("ObjectId("), "{}", saved.pipeline_json);
        assert!(saved.pipeline_json.find("zeta").unwrap() < saved.pipeline_json.find("alpha").unwrap());
        // The user's own text comes back exactly.
        let options = AggregateOptions::from_json(saved.options_json.as_deref());
        assert_eq!(options.shell_text.as_deref(), Some(original.text.as_str()));
        let restored = AggregateDraft::from_saved(&saved);
        assert_eq!(restored.text, original.text);
        assert!(restored.options.shell_text.is_none(), "not kept in the live draft");
    }

    #[test]
    fn rows_written_before_shell_text_existed_still_load_from_pipeline_json() {
        let legacy = engine_storage::aggregate::SavedAggregate {
            id: "old".into(),
            conn_uuid: "c".into(),
            database: "d".into(),
            collection: "coll".into(),
            name: "legacy".into(),
            kind: "aggregate".into(),
            pipeline_json: r#"[{"$match":{"a":1}},{"$limit":3}]"#.into(),
            created_at: 1.0,
            updated_at: 1.0,
            options_json: None,
        };
        let restored = AggregateDraft::from_saved(&legacy);
        assert_eq!(restored.text, legacy.pipeline_json);
        assert_eq!(operators(&restored), [("$match", true), ("$limit", true)]);
        let other = engine_storage::aggregate::SavedAggregate {
            options_json: Some(r#"{"sample_size": 5}"#.into()),
            ..legacy
        };
        assert_eq!(AggregateDraft::from_saved(&other).options.sample_size, 5);
    }

    #[test]
    fn export_payload_sends_no_time_limit_unless_the_user_set_one() {
        let mut draft = draft("[{ $match: {} }]");
        let payload = draft.export_payload();
        assert_eq!(payload["max_time_ms"], 0, "0 = no maxTimeMS");
        assert_eq!(engine::parse_request_payload(&payload).unwrap().max_time_ms, None);
        assert_eq!(
            engine::parse_request_payload(&draft.run_payload(false)).unwrap().max_time_ms,
            Some(engine::DEFAULT_RUN_MAX_TIME_MS),
            "a Run page keeps its default"
        );
        draft.options.max_time_ms = 9000;
        assert_eq!(engine::parse_request_payload(&draft.export_payload()).unwrap().max_time_ms, Some(9000));
    }

    #[test]
    fn revision_changes_with_content_and_derive_matches_the_individual_queries() {
        let mut draft = draft("[{ $match: {} }, { $out: 'copy' }]");
        let first = draft.revision;
        let derived = draft.derive();
        assert_eq!(derived.revision, first);
        assert!(derived.check.is_valid());
        assert_eq!(derived.final_write.as_ref().map(|w| w.target.as_str()), Some("copy"));
        assert_eq!(derived.final_write, draft.final_write());
        assert_eq!(derived.editability, draft.editability(true));
        draft.apply_text("[{ $match: {} }]").unwrap();
        assert!(draft.revision > first);
        let second = draft.revision;
        draft.set_mode(EditorMode::Stages).unwrap();
        assert!(draft.revision > second, "a mode switch is a content change for caches");
        let third = draft.revision;
        draft.options.sample_size = 3;
        draft.touch();
        assert!(draft.revision > third);
        let broken = {
            let mut broken = AggregateDraft::new();
            let _ = broken.apply_text("[{ $match: ");
            broken.derive()
        };
        assert!(broken.check.text_error.is_some() && broken.final_write.is_none() && !broken.editability.editable);
        let mut stages = draft.clone();
        let id = stages.stages[0].id;
        stages.set_stage_body(id, "{ oops: }");
        let derived = stages.derive();
        assert!(derived.check.stage_error(id).is_some() && derived.final_write.is_none());
    }
}
