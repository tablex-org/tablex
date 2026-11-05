// On-demand measurements of the structure edit mode on GPUI's test platform (no renderer, so the
// numbers are call and allocation counts, not frame times): the memory one edit mode retains and
// gives back, what `rebuild()` costs and how often an edited cell reruns `plan_against`.
//
//   cargo test --release -p tablex-gpui properties_edit_retained_memory properties_rebuild_bench \
//     properties_commit_plan_calls -- --ignored --nocapture --test-threads=1
//
// Each result is also printed as a `PERF {json}` line (see `perf_bench`). No perf asserts.

use crate::app::structure_edit::bench as perf_fixture;
use crate::perf_counters as perf_counts;

/// The Postgres fixture's `orders` tab with Properties open. No clicks: `debug_selector` bounds
/// only exist in debug builds, and these benchmarks run in release.
fn perf_app(cx: &mut TestAppContext) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext, u64) {
    cx.dispatcher.allow_parking();
    let (app, visual, tab_id) = sql_table_app(cx);
    app.update(visual, |this, cx| this.show_columns_presentation(tab_id, cx));
    settle(visual);
    (app, visual, tab_id)
}

/// The `Edit structure` toggle.
fn perf_toggle(app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext, tab_id: u64) {
    visual.update(|window, cx| app.update(cx, |this, cx| this.toggle_properties_edit(tab_id, window, cx)));
    settle(visual);
}

/// Live heap bytes once everything pending has been processed (dropped entities are released by
/// the effect flushes `settle` runs).
fn perf_live(visual: &mut gpui::VisualTestContext) -> i64 {
    settle(visual);
    settle(visual);
    crate::grid_bench::live_bytes() as i64
}

#[gpui::test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_edit_retained_memory(cx: &mut TestAppContext) {
    const COLUMNS: usize = 500;
    const CHANGES: usize = 200;
    const STEPS: usize = 30;
    let properties = perf_fixture::described(COLUMNS);
    let (app, visual, tab_id) = perf_app(cx);
    pe_describe(&app, visual, tab_id, properties.clone());
    let changes = perf_fixture::mixed_changes(&properties, CHANGES);
    let record = |name: &str, bytes: i64| {
        eprintln!("  {name:<46} {bytes:>12} B ({:.1} KiB)", bytes as f64 / 1024.0);
        crate::perf_bench::record("retained", name, &[("bytes", bytes as f64)]);
    };
    eprintln!("retained memory of one structure edit mode, table of {COLUMNS} columns (live heap bytes, counting allocator)");

    // Warm-up cycle: first use fills font, theme and render caches that stay for good.
    perf_toggle(&app, visual, tab_id);
    assert!(pe_edit(&app, visual, tab_id).is_some(), "edit mode started");
    perf_toggle(&app, visual, tab_id);
    let base = perf_live(visual);

    let mut residuals = Vec::new();
    for cycle in 0..3 {
        perf_toggle(&app, visual, tab_id);
        assert!(pe_edit(&app, visual, tab_id).is_some(), "edit mode started");
        let entered = perf_live(visual);
        let edit = pe_edit(&app, visual, tab_id).expect("edit mode");
        edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes.clone(), cx)));
        let with_draft = perf_live(visual);
        if cycle == 0 {
            record("enter edit mode, no changes", entered - base);
            record(&format!("draft of {CHANGES} changes (rows, plan, history step)"), with_draft - entered);
            // Per history step: more single edits on columns the draft does not touch.
            let before = perf_live(visual);
            for step in 0..STEPS {
                let column = CHANGES + 1 + step;
                let from = crate::app::structure_edit::live::column_spec(&properties.columns[column]);
                let to = ColumnSpec { data_type: "varchar(90)".into(), ..from.clone() };
                let change = Change::AlterColumn { table: perf_fixture::table(), from, to };
                edit.update(visual, |edit, cx| assert!(edit.push_for_test(change, cx)));
            }
            let after = perf_live(visual);
            record(&format!("{STEPS} more single edits (total)"), after - before);
            record("per history step (a copy of the draft, K=200..230)", (after - before) / STEPS as i64);
        }
        edit.update(visual, |edit, cx| edit.discard_all(cx));
        let editor = edit.read_with(visual, |edit, _| edit.editor().downgrade());
        perf_toggle(&app, visual, tab_id);
        assert!(pe_edit(&app, visual, tab_id).is_none(), "edit mode left");
        drop(edit);
        let residual = perf_live(visual) - base;
        crate::perf_bench::record("retained", &format!("cycle {} cell editor released after leaving (1 = yes)", cycle + 1), &[("bytes", f64::from(u8::from(editor.upgrade().is_none())))]);
        eprintln!("  cycle {}: the cell editor entity is released after leaving: {}", cycle + 1, editor.upgrade().is_none());
        record(&format!("cycle {} residual after leaving", cycle + 1), residual);
        residuals.push(residual);
    }
    record("drift per enter/leave cycle (last - first)", residuals[2] - residuals[0]);

    // The cycles above are measured against a warm-up without a draft; a draft shows the markers
    // (+ * - !) in the bold face of the font, one more cache that stays for good. Measured against a
    // warm-up that had a draft, a cycle gives back (nearly) everything it took.
    let draft_cycle = |app: &gpui::Entity<AppRoot>, visual: &mut gpui::VisualTestContext| {
        perf_toggle(app, visual, tab_id);
        let edit = pe_edit(app, visual, tab_id).expect("edit mode");
        edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes.clone(), cx)));
        edit.update(visual, |edit, cx| edit.discard_all(cx));
        perf_toggle(app, visual, tab_id);
        drop(edit);
    };
    draft_cycle(&app, visual);
    let warm = perf_live(visual);
    let mut leftovers = Vec::new();
    for _ in 0..2 {
        draft_cycle(&app, visual);
        leftovers.push(perf_live(visual) - warm);
    }
    record("residual of a cycle with a draft, after a warm-up with one", leftovers[0]);
    record("drift of the next such cycle", leftovers[1] - leftovers[0]);

    // Closing the tab with a draft pending: the entity and what it holds go with it.
    perf_toggle(&app, visual, tab_id);
    assert!(pe_edit(&app, visual, tab_id).is_some(), "edit mode started");
    let edit = pe_edit(&app, visual, tab_id).expect("edit mode");
    edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(changes.clone(), cx)));
    let open = perf_live(visual);
    let weak = edit.downgrade();
    drop(edit);
    app.update(visual, |this, cx| this.close_tab(tab_id, cx));
    let closed = perf_live(visual);
    eprintln!("  the edit entity is released after closing the tab: {}", weak.upgrade().is_none());
    crate::perf_bench::record("retained", "edit entity released at tab close (1 = yes)", &[("bytes", f64::from(u8::from(weak.upgrade().is_none())))]);
    record("released by closing the tab (edit mode + K=200)", open - closed);
}

#[gpui::test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_rebuild_bench(cx: &mut TestAppContext) {
    eprintln!("PropertiesEdit::rebuild (rows, staleness, plan, lists, filters) on a table of 500 columns");
    let (app, visual, tab_id) = perf_app(cx);
    for (label, changes, indexes, foreign_keys) in [
        ("F3-F5 C=500 K=0", 0, 0, 0),
        ("F3-F5 C=500 K=50", 50, 0, 0),
        ("F3-F5 C=500 K=200", 200, 0, 0),
        ("F3-F5 C=500 K=50 idx=50 fk=50", 50, 50, 50),
    ] {
        let properties = perf_fixture::described_with(500, indexes, foreign_keys);
        pe_describe(&app, visual, tab_id, properties.clone());
        if pe_edit(&app, visual, tab_id).is_none() {
            perf_toggle(&app, visual, tab_id);
            assert!(pe_edit(&app, visual, tab_id).is_some(), "edit mode started");
        }
        let edit = pe_edit(&app, visual, tab_id).expect("edit mode");
        edit.update(visual, |edit, cx| edit.discard_all(cx));
        if changes > 0 {
            let list = perf_fixture::mixed_changes(&properties, changes);
            edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(list, cx)));
        }
        settle(visual);
        let plans = perf_counts::read(&perf_counts::PLAN_AGAINST);
        let rebuilds = perf_counts::read(&perf_counts::REBUILD);
        let stats = crate::perf_bench::bench(|| edit.update(visual, |edit, _| edit.rebuild_for_test()));
        let calls = (perf_counts::read(&perf_counts::REBUILD) - rebuilds) as f64;
        let plan_calls = (perf_counts::read(&perf_counts::PLAN_AGAINST) - plans) as f64 / calls;
        eprintln!(
            "  {label:<32} median {:>9.1} us  min {:>9.1}  max {:>9.1}  {} allocs  {} B  {plan_calls:.2} plan_against per rebuild",
            stats.median_us, stats.min_us, stats.max_us, stats.allocations, stats.bytes
        );
        crate::perf_bench::record_stats("rebuild", label, &stats, &[("plan_against_per_rebuild", plan_calls)]);
    }
}

#[gpui::test]
#[ignore = "on-demand benchmark; run with --release --ignored --nocapture --test-threads=1"]
fn properties_commit_plan_calls(cx: &mut TestAppContext) {
    use crate::app::results::properties_edit::{Exit, Field};
    const COMMITS: usize = 6;
    eprintln!("plan_against / rebuild calls per committed cell edit (commit + the frames that follow), table of 500 columns, K=50");
    let (app, visual, tab_id) = perf_app(cx);
    let properties = perf_fixture::described_with(500, 50, 50);
    pe_describe(&app, visual, tab_id, properties.clone());
    perf_toggle(&app, visual, tab_id);
    assert!(pe_edit(&app, visual, tab_id).is_some(), "edit mode started");
    let edit = pe_edit(&app, visual, tab_id).expect("edit mode");
    let list = perf_fixture::mixed_changes(&properties, 50);
    edit.update(visual, |edit, cx| assert!(edit.push_all_for_test(list, cx)));
    settle(visual);
    let column = perf_fixture::untouched_column(500, 50);
    let original = format!("col{column}");
    for dialog in [false, true] {
        if dialog {
            visual.update(|window, cx| app.update(cx, |this, cx| this.open_properties_changes(tab_id, window, cx)));
            settle(visual);
            assert!(visual.update(|window, cx| window.has_active_dialog(cx)), "the review dialog is open");
        }
        let (mut plans, mut rebuilds, mut allocations, mut bytes, mut renders) = (0u64, 0u64, 0u64, 0u64, 0u64);
        for index in 0..COMMITS {
            let text = if index % 2 == 0 { format!("{original}_x") } else { original.clone() };
            let started = crate::perf_bench::mark();
            let (p0, r0, f0) = (perf_counts::read(&perf_counts::PLAN_AGAINST), perf_counts::read(&perf_counts::REBUILD), frame_count());
            visual.update(|window, cx| {
                edit.update(cx, |edit, cx| {
                    assert!(edit.begin_edit(column, Field::Name, window, cx));
                    edit.editor().update(cx, |editor, cx| editor.set_value(text, window, cx));
                    edit.commit_edit(Exit::Enter, window, cx);
                })
            });
            settle(visual);
            let cost = started.since();
            plans += perf_counts::read(&perf_counts::PLAN_AGAINST) - p0;
            rebuilds += perf_counts::read(&perf_counts::REBUILD) - r0;
            renders += frame_count() - f0;
            allocations += cost.allocations;
            bytes += cost.bytes;
        }
        let n = COMMITS as f64;
        let state = if dialog { "review dialog open" } else { "review dialog closed" };
        eprintln!(
            "  {state:<22} per commit: {:.2} plan_against  {:.2} rebuild  {:.1} AppRoot renders  {:.0} allocs  {:.1} KiB (commit + settle: the test platform draws 3+ frames)",
            plans as f64 / n,
            rebuilds as f64 / n,
            renders as f64 / n,
            allocations as f64 / n,
            bytes as f64 / n / 1024.0
        );
        crate::perf_bench::record(
            "commit_plan_calls",
            &format!("F3-F5 {state}"),
            &[
                ("commits", n),
                ("plan_against_per_commit", plans as f64 / n),
                ("rebuilds_per_commit", rebuilds as f64 / n),
                ("renders_per_commit", renders as f64 / n),
                ("allocs_per_commit", allocations as f64 / n),
                ("kib_per_commit", bytes as f64 / n / 1024.0),
            ],
        );
    }
}
