use bson::{doc, Bson, Document};


/// A complete execution plan — the unified intermediate representation.
/// Level 1 (db.coll.find()) compiles into this.
/// Level 2 (pipeline.run([...])) parses directly into this.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelinePlan {
    pub stages: Vec<PipelineStage>,
}

/// Each stage maps 1:1 to a MongoDB aggregation pipeline stage,
/// except `Join` which is orchestrated by the Federated Aggregate Orchestrator.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineStage {
    SourceMongo {
        db: String,
        collection: String,
    },
    Match {
        filter: Document,
    },
    Project {
        spec: Document,
    },
    Sort {
        spec: Document,
    },
    Limit {
        count: i64,
    },
    Skip {
        count: i64,
    },
    LookupMongo {
        from: String,
        local_field: String,
        foreign_field: String,
        as_field: String,
    },
    Group {
        group_key: Bson,
        accumulators: Document,
    },
    Raw {
        doc: Document,
    },
    Join {
        foreign: AggregateSource,
        local_field: String,
        foreign_field: String,
        as_field: String,
        pipeline: Vec<Document>,
        one: bool,
    },
}

/// Source for an aggregate or join — same connection, different database.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateSource {
    pub db: String,
    pub collection: String,
}

/// A named pipeline template defined with pipeline.define().
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineDefinition {
    pub name: String,
    pub default_params: Document,
    pub stages: Vec<PipelineStage>,
}

/// Compiled execution plan — either a single MongoDB aggregate
/// or a federated plan with cross-DB joins.
#[derive(Debug)]
pub enum CompiledPlan {
    Single {
        db: String,
        collection: String,
        stages: Vec<Document>,
    },
    Federated {
        source: AggregateSource,
        pre_join_stages: Vec<Document>,
        join: JoinNode,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct JoinNode {
    pub foreign: AggregateSource,
    pub local_field: String,
    pub foreign_field: String,
    pub as_field: String,
    pub pipeline: Vec<Document>,
    pub one: bool,
}

/// What `into_aggregation` returns: database, collection, the stages MongoDB runs, and the
/// cross-database `Join` stages with their positions.
pub type Aggregation = (String, String, Vec<Document>, Vec<(usize, PipelineStage)>);

impl PipelinePlan {
    /// Compile PipelinePlan into a CompiledPlan.
    /// - If no Join stages → Single (pure MongoDB aggregate)
    /// - If Join stage found → Federated (orchestrated execution)
    pub fn compile(self) -> Result<CompiledPlan, String> {
        if self.stages.is_empty() {
            return Err("PipelinePlan has no stages".into());
        }

        let first = &self.stages[0];
        let (db, collection) = match first {
            PipelineStage::SourceMongo { db, collection } => (db.clone(), collection.clone()),
            _ => return Err("First stage must be SourceMongo".into()),
        };

        let join_pos = self
            .stages
            .iter()
            .position(|s| matches!(s, PipelineStage::Join { .. }));

        match join_pos {
            None => {
                let mut stages = Vec::new();
                for stage in &self.stages[1..] {
                    stages.push(stage.to_aggregation_doc()?);
                }
                Ok(CompiledPlan::Single {
                    db,
                    collection,
                    stages,
                })
            }
            Some(pos) => {
                let mut pre_join_stages = Vec::new();
                for stage in &self.stages[1..pos] {
                    pre_join_stages.push(stage.to_aggregation_doc()?);
                }

                let join_stage = &self.stages[pos];
                let join_node = match join_stage {
                    PipelineStage::Join {
                        foreign,
                        local_field,
                        foreign_field,
                        as_field,
                        pipeline,
                        one,
                    } => JoinNode {
                        foreign: foreign.clone(),
                        local_field: local_field.clone(),
                        foreign_field: foreign_field.clone(),
                        as_field: as_field.clone(),
                        pipeline: pipeline.clone(),
                        one: *one,
                    },
                    _ => unreachable!(),
                };

                // MVP: ignore stages after $join
                let after_join = &self.stages[pos + 1..];
                if !after_join.is_empty() {
                    let ignored: Vec<String> =
                        after_join.iter().map(|s| format!("{:?}", s)).collect();
                    eprintln!(
                        "[pipeline] Warning: {} stage(s) after $join are ignored in MVP: {:?}",
                        after_join.len(),
                        ignored
                    );
                }

                Ok(CompiledPlan::Federated {
                    source: AggregateSource { db, collection },
                    pre_join_stages,
                    join: join_node,
                })
            }
        }
    }

    /// Legacy method for backward compat with single-DB pipelines.
    pub fn into_aggregation(self) -> Result<Aggregation, String> {
        if self.stages.is_empty() {
            return Err("PipelinePlan has no stages".into());
        }

        let first = &self.stages[0];
        let (db, collection) = match first {
            PipelineStage::SourceMongo { db, collection } => (db.clone(), collection.clone()),
            _ => return Err("First stage must be SourceMongo".into()),
        };

        let mut pipeline = Vec::new();
        let mut cross_db = Vec::new();
        for (i, stage) in self.stages[1..].iter().enumerate() {
            match stage {
                PipelineStage::Join { .. } => {
                    cross_db.push((i, stage.clone()));
                }
                _ => {
                    pipeline.push(stage.to_aggregation_doc()?);
                }
            }
        }

        Ok((db, collection, pipeline, cross_db))
    }
}

impl PipelineStage {
    fn to_aggregation_doc(&self) -> Result<Document, String> {
        match self {
            PipelineStage::Match { filter } => {
                Ok(doc! { "$match": filter.clone() })
            }
            PipelineStage::Project { spec } => {
                Ok(doc! { "$project": spec.clone() })
            }
            PipelineStage::Sort { spec } => {
                Ok(doc! { "$sort": spec.clone() })
            }
            PipelineStage::Limit { count } => {
                Ok(doc! { "$limit": *count })
            }
            PipelineStage::Skip { count } => {
                Ok(doc! { "$skip": *count })
            }
            PipelineStage::LookupMongo { from, local_field, foreign_field, as_field } => {
                Ok(doc! {
                    "$lookup": {
                        "from": from,
                        "localField": local_field,
                        "foreignField": foreign_field,
                        "as": as_field,
                    }
                })
            }
            PipelineStage::Group { group_key, accumulators } => {
                let mut group = Document::new();
                group.insert("_id", group_key.clone());
                for (k, v) in accumulators.iter() {
                    group.insert(k, v.clone());
                }
                Ok(doc! { "$group": group })
            }
            PipelineStage::Raw { doc } => {
                Ok(doc.clone())
            }
            PipelineStage::Join { .. } => {
                Err("$join stages are handled by the Federated Aggregate Orchestrator and should not appear in aggregation docs".into())
            }
            PipelineStage::SourceMongo { .. } => {
                Err("SourceMongo should not appear in aggregation stages".into())
            }
        }
    }
}

/// Rows a script returns when it sets no `limit`.
pub const MAX_SCRIPT_ROWS: i64 = 10_000;

pub fn ensure_limit(plan: PipelinePlan) -> PipelinePlan {
    let has_limit = plan
        .stages
        .iter()
        .any(|s| matches!(s, PipelineStage::Limit { .. }));
    if has_limit {
        return plan;
    }
    let mut stages = plan.stages;
    stages.push(PipelineStage::Limit {
        count: MAX_SCRIPT_ROWS,
    });
    PipelinePlan { stages }
}

pub fn apply_param_substitution(
    stage: &mut PipelineStage,
    defaults: &Document,
    override_params: &Document,
) {
    match stage {
        PipelineStage::Match { filter } => {
            substitute_doc(filter, defaults, override_params);
        }
        PipelineStage::Group { accumulators, .. } => {
            substitute_doc(accumulators, defaults, override_params);
        }
        PipelineStage::Sort { spec } => {
            substitute_doc(spec, defaults, override_params);
        }
        PipelineStage::Project { spec } => {
            substitute_doc(spec, defaults, override_params);
        }
        _ => {}
    }
}

fn substitute_doc(doc: &mut Document, defaults: &Document, override_params: &Document) {
    let keys: Vec<String> = doc.keys().cloned().collect();
    for key in keys {
        if let Some(val) = doc.get_str(&key).ok().map(|s| s.to_string()) {
            if let Some(param_name) = val.strip_prefix("$$") {
                if let Some(replacement) = override_params
                    .get(param_name)
                    .or_else(|| defaults.get(param_name))
                {
                    let _ = doc.insert(&key, replacement.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_compile_single() {
        let plan = PipelinePlan {
            stages: vec![
                PipelineStage::SourceMongo {
                    db: "testdb".into(),
                    collection: "users".into(),
                },
                PipelineStage::Match {
                    filter: doc! { "name": "Huy" },
                },
                PipelineStage::Sort {
                    spec: doc! { "age": 1 },
                },
                PipelineStage::Limit { count: 10 },
            ],
        };
        let compiled = plan.compile().unwrap();
        match compiled {
            CompiledPlan::Single {
                db,
                collection,
                stages,
            } => {
                assert_eq!(db, "testdb");
                assert_eq!(collection, "users");
                assert_eq!(stages.len(), 3);
                assert!(stages[0].contains_key("$match"));
            }
            _ => panic!("Expected Single"),
        }
    }

    #[test]
    fn pipeline_compile_federated() {
        let plan = PipelinePlan {
            stages: vec![
                PipelineStage::SourceMongo {
                    db: "merchant-service".into(),
                    collection: "merchants".into(),
                },
                PipelineStage::Match {
                    filter: doc! { "status": "active" },
                },
                PipelineStage::Join {
                    foreign: AggregateSource {
                        db: "wallet-service".into(),
                        collection: "wallets".into(),
                    },
                    local_field: "_id".into(),
                    foreign_field: "profileId".into(),
                    as_field: "wallets".into(),
                    pipeline: vec![doc! { "$match": { "currency": "VND" } }],
                    one: false,
                },
            ],
        };
        let compiled = plan.compile().unwrap();
        match compiled {
            CompiledPlan::Federated {
                source,
                pre_join_stages,
                join,
            } => {
                assert_eq!(source.db, "merchant-service");
                assert_eq!(source.collection, "merchants");
                assert_eq!(pre_join_stages.len(), 1);
                assert!(pre_join_stages[0].contains_key("$match"));
                assert_eq!(join.foreign.db, "wallet-service");
                assert_eq!(join.foreign.collection, "wallets");
                assert_eq!(join.local_field, "_id");
                assert_eq!(join.foreign_field, "profileId");
                assert_eq!(join.as_field, "wallets");
                assert_eq!(join.pipeline.len(), 1);
                assert!(!join.one);
            }
            _ => panic!("Expected Federated"),
        }
    }

    #[test]
    fn pipeline_lookup_same_db() {
        let plan = PipelinePlan {
            stages: vec![
                PipelineStage::SourceMongo {
                    db: "mydb".into(),
                    collection: "users".into(),
                },
                PipelineStage::LookupMongo {
                    from: "orders".into(),
                    local_field: "_id".into(),
                    foreign_field: "userId".into(),
                    as_field: "userOrders".into(),
                },
            ],
        };
        let compiled = plan.compile().unwrap();
        match compiled {
            CompiledPlan::Single { stages, .. } => {
                assert_eq!(stages.len(), 1);
                let lookup = stages[0].get_document("$lookup").unwrap();
                assert_eq!(lookup.get_str("from").unwrap(), "orders");
            }
            _ => panic!("Expected Single"),
        }
    }

    #[test]
    fn pipeline_source_missing_fails() {
        let plan = PipelinePlan {
            stages: vec![PipelineStage::Match { filter: doc! {} }],
        };
        assert!(plan.compile().is_err());
    }

    #[test]
    fn pipeline_empty_fails() {
        let plan = PipelinePlan { stages: vec![] };
        assert!(plan.compile().is_err());
    }

    #[test]
    fn pipeline_group_compiles() {
        let plan = PipelinePlan {
            stages: vec![
                PipelineStage::SourceMongo {
                    db: "db".into(),
                    collection: "users".into(),
                },
                PipelineStage::Group {
                    group_key: Bson::Null,
                    accumulators: doc! { "count": { "$sum": 1 } },
                },
            ],
        };
        let compiled = plan.compile().unwrap();
        match compiled {
            CompiledPlan::Single { stages, .. } => {
                assert_eq!(stages.len(), 1);
                assert!(stages[0].contains_key("$group"));
            }
            _ => panic!("Expected Single"),
        }
    }

    #[test]
    fn pipeline_raw_stage_passes_through() {
        let plan = PipelinePlan {
            stages: vec![
                PipelineStage::SourceMongo {
                    db: "db".into(),
                    collection: "orders".into(),
                },
                PipelineStage::Raw {
                    doc: doc! { "$unwind": "$items" },
                },
            ],
        };
        let compiled = plan.compile().unwrap();
        match compiled {
            CompiledPlan::Single { stages, .. } => {
                assert_eq!(stages.len(), 1);
                assert_eq!(stages[0].get_str("$unwind").unwrap(), "$items");
            }
            _ => panic!("Expected Single"),
        }
    }

}
