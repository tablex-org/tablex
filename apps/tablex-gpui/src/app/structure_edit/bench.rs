//! On-demand micro benchmarks of the structure editor's pure core (no window): the row builder of
//! the Properties Columns grid, the checks every edit reruns (`plan_against`, `name_clashes`,
//! `stale`, `warnings`), `Draft::push` folding and the review-model cache. Measurements only.
//!
//! ```text
//! cargo test --release -p tablex-gpui structure_edit::bench -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Also the shared fixture of the other structure-editor benchmarks: a described table of `C`
//! columns and a draft of `K` mixed changes ([`mixed_changes`]).

use std::sync::Arc;

use super::{live, DraftEdit, EditContext, EditScope, EditSession};
use crate::app::results::properties_edit::build_rows;
use crate::catalog::{CatalogColumn, CatalogConstraint, CatalogConstraintKind, CatalogForeignKey, CatalogIndex, TableProperties};
use crate::connections::ConnectionKind;
use crate::perf_bench::{bench, record_stats};
use crate::sql_ddl::draft::Draft;
use crate::sql_ddl::schema::{name_clashes, stale, LiveSchema};
use crate::sql_ddl::{dialect_for, plan_against, warnings, Change, ColumnSpec, TableId};

pub(in crate::app) fn table() -> TableId {
    TableId { schema: Some("public".into()), name: "orders".into() }
}

/// A table of `columns` columns named `col0`... (`col0` is the primary key), with a mix of types,
/// defaults and comments like a real one.
pub(in crate::app) fn described(columns: usize) -> TableProperties {
    described_with(columns, 0, 0)
}

/// [`described`] with `indexes` single-column indexes and `foreign_keys` foreign keys.
pub(in crate::app) fn described_with(columns: usize, indexes: usize, foreign_keys: usize) -> TableProperties {
    const TYPES: [&str; 5] = ["text", "integer", "varchar(64)", "timestamp", "boolean"];
    let column = |index: usize| CatalogColumn {
        name: format!("col{index}"),
        data_type: if index == 0 { "integer" } else { TYPES[index % TYPES.len()] }.into(),
        nullable: index != 0,
        primary_key: index == 0,
        default_expression: (index.is_multiple_of(5) && index != 0).then(|| "'x'".to_owned()),
        comment: index.is_multiple_of(7).then(|| format!("comment of column {index}")),
        ..CatalogColumn::default()
    };
    TableProperties {
        columns: (0..columns).map(column).collect(),
        keys: Some(vec![CatalogConstraint {
            name: "orders_pkey".into(),
            kind: CatalogConstraintKind::PrimaryKey,
            columns: vec!["col0".into()],
            definition: None,
        }]),
        foreign_keys: Some(
            (0..foreign_keys)
                .map(|i| CatalogForeignKey {
                    name: format!("fk{i}"),
                    table: "orders".into(),
                    columns: vec![format!("col{}", i + 1)],
                    referenced_table: format!("other{i}"),
                    referenced_columns: vec!["id".into()],
                    on_update: None,
                    on_delete: None,
                    definition: None,
                })
                .collect(),
        ),
        indexes: Some(
            (0..indexes)
                .map(|i| CatalogIndex {
                    name: format!("idx{i}"),
                    columns: vec![format!("col{}", i + 1)],
                    unique: false,
                    primary: false,
                    method: Some("btree".into()),
                    definition: None,
                })
                .collect(),
        ),
        ..TableProperties::default()
    }
}

/// `count` changes that do not fold into each other: change `i` is about the existing column
/// `col{i + 1}` while there are some (rename, alter, add, drop in turn), and adds a new column after.
pub(in crate::app) fn mixed_changes(properties: &TableProperties, count: usize) -> Vec<Change> {
    let table = table();
    (0..count)
        .map(|i| {
            let existing = properties.columns.get(i + 1);
            let add = |name: String| Change::AddColumn {
                table: table.clone(),
                column: ColumnSpec { name, data_type: "integer".into(), nullable: true, ..ColumnSpec::default() },
            };
            match (existing, i % 4) {
                (Some(column), 0) => Change::RenameColumn { table: table.clone(), from: column.name.clone(), to: format!("r{i}") },
                (Some(column), 1) => {
                    let from = live::column_spec(column);
                    let to = ColumnSpec { data_type: "varchar(80)".into(), ..from.clone() };
                    Change::AlterColumn { table: table.clone(), from, to }
                }
                (Some(column), 3) => Change::DropColumn { table: table.clone(), column: column.name.clone() },
                _ => add(format!("new{i}")),
            }
        })
        .collect()
}

/// The draft [`mixed_changes`] make when pushed one by one (the real folding path).
pub(in crate::app) fn mixed_draft(properties: &TableProperties, count: usize) -> Draft {
    let mut draft = Draft::default();
    for change in mixed_changes(properties, count) {
        draft.push(change);
    }
    assert_eq!(draft.len(), count, "the mixed changes do not fold into each other");
    draft
}

/// The index (into the columns) of an existing column that no change of `mixed_changes(_, count)` touches.
pub(in crate::app) fn untouched_column(columns: usize, count: usize) -> usize {
    (1..columns)
        .rev()
        .find(|&column| column > count || (column - 1) % 4 == 2)
        .expect("a table with an untouched column")
}

fn live_schema(properties: &TableProperties) -> LiveSchema {
    let mut schema = LiveSchema::new(false);
    schema.present(table(), live::live_table(properties));
    schema
}

fn context() -> EditContext {
    EditContext {
        scope: EditScope { kind: ConnectionKind::Postgres, database: "warehouse".into(), schema: Some("public".into()) },
        subject: "Bench · warehouse".into(),
        confirm_execute: false,
        disabled: None,
        sql_mode: None,
        catalog: None,
        catalog_error: None,
    }
}

const COLUMNS: [usize; 3] = [50, 500, 2000];
const CHANGES: [usize; 3] = [0, 20, 200];
const BIG_CHANGES: [usize; 3] = [20, 200, 2000];

#[test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_build_rows_bench() {
    eprintln!("build_rows: the Columns grid rows of the edit mode, C columns x K pending changes");
    for columns in COLUMNS {
        let properties = described(columns);
        let live_table = live::live_table(&properties);
        for changes in CHANGES {
            let draft = mixed_draft(&properties, changes);
            let table = table();
            let stats = bench(|| build_rows(&properties, &live_table, &draft, &table));
            eprintln!("  C={columns:<5} K={changes:<4} median {:>9.1} us  min {:>9.1}  max {:>9.1}  {} allocs  {} B", stats.median_us, stats.min_us, stats.max_us, stats.allocations, stats.bytes);
            record_stats("build_rows", &format!("C={columns} K={changes}"), &stats, &[("columns", columns as f64), ("changes", changes as f64)]);
        }
    }
}

/// A named check of the DDL bench: it returns how many entries it produced.
type Check<'a> = (&'a str, Box<dyn FnMut() -> usize + 'a>);

#[test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_ddl_checks_bench() {
    eprintln!("plan_against / name_clashes / stale / warnings on a table of 500 columns, K pending changes");
    let properties = described(500);
    let live = live_schema(&properties);
    let dialect = dialect_for(ConnectionKind::Postgres).expect("Postgres has a dialect");
    for changes in BIG_CHANGES {
        let draft = mixed_draft(&properties, changes);
        let list = &draft.changes;
        let checks: [Check<'_>; 4] = [
            ("plan_against", Box::new(|| plan_against(dialect, list, &live).entries.len())),
            ("name_clashes", Box::new(|| name_clashes(list, &live, dialect.name_rules()).len())),
            ("stale", Box::new(|| stale(list, &live).len())),
            ("warnings", Box::new(|| warnings(dialect, list).len())),
        ];
        for (name, work) in checks {
            let stats = bench(work);
            eprintln!("  {name:<13} K={changes:<5} median {:>10.1} us  min {:>10.1}  max {:>10.1}  {} allocs  {} B", stats.median_us, stats.min_us, stats.max_us, stats.allocations, stats.bytes);
            record_stats(name, &format!("C=500 K={changes}"), &stats, &[("columns", 500.0), ("changes", changes as f64)]);
        }
    }
}

#[test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_draft_push_bench() {
    eprintln!("Draft::push folding: K sequential edits (total and per edit), table of 500 columns");
    let properties = described(500);
    for count in BIG_CHANGES {
        let changes = mixed_changes(&properties, count);
        // Distinct columns: every push scans the pending changes for what it folds into.
        let stats = bench(|| {
            let mut draft = Draft::default();
            for change in changes.iter().cloned() {
                draft.push(change);
            }
            draft.len()
        });
        eprintln!("  distinct   K={count:<5} total {:>11.1} us  per edit {:>8.2} us  {} allocs  {} B", stats.median_us, stats.median_us / count as f64, stats.allocations, stats.bytes);
        record_stats("draft_push", &format!("distinct K={count}"), &stats, &[("changes", count as f64), ("per_edit_us", stats.median_us / count as f64)]);

        // One column edited K times (a user retyping a cell): all fold into one change.
        let first = live::column_spec(&properties.columns[1]);
        let retypes: Vec<Change> = (0..count)
            .map(|i| {
                let from = ColumnSpec { data_type: format!("varchar({})", i + 1), ..first.clone() };
                let to = ColumnSpec { data_type: format!("varchar({})", i + 2), ..first.clone() };
                Change::AlterColumn { table: table(), from, to }
            })
            .collect();
        let stats = bench(|| {
            let mut draft = mixed_draft(&properties, 20);
            for change in retypes.iter().cloned() {
                draft.push(change);
            }
            draft.len()
        });
        eprintln!("  same cell  K={count:<5} total {:>11.1} us  per edit {:>8.2} us  {} allocs  {} B (incl. a 20-change base)", stats.median_us, stats.median_us / count as f64, stats.allocations, stats.bytes);
        record_stats("draft_push", &format!("same-cell K={count}"), &stats, &[("changes", count as f64), ("per_edit_us", stats.median_us / count as f64)]);

        // What the editor runs per edit: copy the draft, push, compare, record the history step.
        let stats = bench(|| {
            let mut session: EditSession<DraftEdit> = EditSession::default();
            for change in changes.iter().cloned() {
                if let Some(after) = session.edit(|draft| draft.push(change)) {
                    session.set_draft(after);
                }
            }
            session.pending_count()
        });
        eprintln!("  session    K={count:<5} total {:>11.1} us  per edit {:>8.2} us  {} allocs  {} B (EditSession::edit: clone + push + compare + history)", stats.median_us, stats.median_us / count as f64, stats.allocations, stats.bytes);
        record_stats("session_edit", &format!("K={count}"), &stats, &[("changes", count as f64), ("per_edit_us", stats.median_us / count as f64)]);
    }
}

#[test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_review_cache_bench() {
    eprintln!("review model (the Changes dialog's per-render read): cache hit vs miss, table of 500 columns");
    let properties = described(500);
    let live = live_schema(&properties);
    let context = context();
    for count in BIG_CHANGES {
        let mut session: EditSession<DraftEdit> = EditSession::default();
        let draft = Arc::new(mixed_draft(&properties, count));
        session.set_draft(draft);
        session.set_live(live.clone());
        session.stale = vec![None; count];
        // Prime the cache, then read it: the model clones the change list and the stale list each time.
        std::hint::black_box(session.review_model(&context));
        let hit = bench(|| session.review_model(&context).planned.entries.len());
        // A new catalog revision invalidates the cache: the plan and the warnings are recomputed.
        let clone = bench(|| matches!(live.clone().get(&table()), crate::sql_ddl::schema::Lookup::Absent));
        let miss = bench(|| {
            session.set_live(live.clone());
            session.review_model(&context).planned.entries.len()
        });
        eprintln!("  K={count:<5} hit {:>9.1} us ({} allocs, {} B)   miss {:>11.1} us ({} allocs, {} B), of which cloning the catalog {:>8.1} us", hit.median_us, hit.allocations, hit.bytes, miss.median_us, miss.allocations, miss.bytes, clone.median_us);
        record_stats("review_cache", &format!("hit K={count}"), &hit, &[("changes", count as f64)]);
        record_stats("review_cache", &format!("miss K={count}"), &miss, &[("changes", count as f64), ("catalog_clone_us", clone.median_us)]);
    }
}

#[test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_undo_redo_bench() {
    use crate::perf_bench::{measure, Cost};
    eprintln!("undo / redo of a draft of K changes (a snapshot is turned back into a draft: deep copy, O(K)); first edit after a draft was installed from outside");
    let properties = described(500);
    let median = |mut costs: Vec<Cost>| {
        costs.sort_by_key(|cost| cost.elapsed);
        costs[costs.len() / 2]
    };
    for count in BIG_CHANGES {
        let draft = Arc::new(mixed_draft(&properties, count));
        let mut session: EditSession<DraftEdit> = EditSession::default();
        session.set_draft(Arc::clone(&draft));
        // The first edit of a draft the session did not make: its snapshot is built (every change copied once).
        let untouched = untouched_column(500, count);
        let from = live::column_spec(&properties.columns[untouched]);
        let to = ColumnSpec { data_type: "varchar(77)".into(), ..from.clone() };
        let edit = Change::AlterColumn { table: table(), from, to };
        let mut first = Vec::new();
        for _ in 0..15 {
            let mut fresh: EditSession<DraftEdit> = EditSession::default();
            fresh.set_draft(Arc::clone(&draft));
            let (after, cost) = measure(|| fresh.edit(|d| d.push(edit.clone())));
            std::hint::black_box(after);
            first.push(cost);
        }
        let first = median(first);
        let after = session.edit(|d| d.push(edit.clone())).expect("a new alteration");
        session.set_draft(after);
        // Undo and redo, alternately, a step of a draft of K changes.
        let (mut undo, mut redo) = (Vec::new(), Vec::new());
        for _ in 0..15 {
            let (_, cost) = measure(|| {
                let step = session.undo().expect("a step to undo");
                let draft = session.restore(&step.before);
                session.set_draft(draft);
            });
            undo.push(cost);
            let (_, cost) = measure(|| {
                let step = session.redo().expect("a step to redo");
                let draft = session.restore(&step.after);
                session.set_draft(draft);
            });
            redo.push(cost);
        }
        let (undo, redo) = (median(undo), median(redo));
        eprintln!(
            "  K={count:<5} first edit {:>9.1} us ({} allocs, {} B)   undo {:>9.1} us ({} allocs, {} B)   redo {:>9.1} us ({} allocs, {} B)",
            first.elapsed.as_secs_f64() * 1e6, first.allocations, first.bytes,
            undo.elapsed.as_secs_f64() * 1e6, undo.allocations, undo.bytes,
            redo.elapsed.as_secs_f64() * 1e6, redo.allocations, redo.bytes,
        );
        for (name, cost) in [("first edit", first), ("undo", undo), ("redo", redo)] {
            crate::perf_bench::record(
                "undo_redo",
                &format!("{name} K={count}"),
                &[("median_us", cost.elapsed.as_secs_f64() * 1e6), ("allocs", cost.allocations as f64), ("bytes", cost.bytes as f64), ("changes", count as f64)],
            );
        }
    }
}
