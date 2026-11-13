// Informational timing of the Mongo Tree view with ~2,000 expanded rows. Run on demand:
//
//   cargo test -j 4 -p tablex-gpui mongo_tree_render_cost -- --ignored --nocapture --test-threads=1
//
// Debug profile, so only the before/after ratio means anything. No asserts.

#[gpui::test]
#[ignore]
fn mongo_tree_render_cost(cx: &mut TestAppContext) {
    let (app, visual, tab_id) = tree_app(cx);
    seed_big_tree(&app, visual, tab_id);
    let rows = app.read_with(visual, |this, _| {
        let inspector = &this.workspace.runtime(tab_id).unwrap().mongo.inspector;
        crate::mongo_result::flatten_tree(&inspector.snapshot.as_ref().unwrap().document, &inspector.expanded_paths).len()
    });
    for _ in 0..3 {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    const FRAMES: u32 = 30;
    let allocs = crate::grid_bench::allocation_count();
    let start = std::time::Instant::now();
    for _ in 0..FRAMES {
        app.update(visual, |_, cx| cx.notify());
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let still = start.elapsed() / FRAMES;
    let still_allocs = (crate::grid_bench::allocation_count() - allocs) / u64::from(FRAMES);
    // Scrolling: every step brings a different window of rows into view.
    let steps = 200usize;
    let allocs = crate::grid_bench::allocation_count();
    let start = std::time::Instant::now();
    for step in 0..steps {
        app.update(visual, |this, cx| {
            this.view(tab_id).mongo_detail_scroll.scroll_to_item((step * 10) % rows, gpui::ScrollStrategy::Top);
            cx.notify();
        });
        visual.update(|window, _| window.refresh());
        visual.run_until_parked();
    }
    let scroll = start.elapsed() / steps as u32;
    let scroll_allocs = (crate::grid_bench::allocation_count() - allocs) / steps as u64;
    eprintln!("MONGO_TREE rows={rows} redraw={still:?} ({still_allocs} allocs) scroll_step={scroll:?} ({scroll_allocs} allocs)");
}
