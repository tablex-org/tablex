// Binary cells in the grid and the value panel: the compact cell form, the hex dump, the text decode,
// the image preview of real PNG / JPEG bytes, the size cap, and Save to / Load from a file through
// the platform prompts (stubbed) down to a real SQLite database.

const GRADIENT_PNG: &[u8] = include_bytes!("fixtures/gradient.png");
const GRADIENT_JPEG: &[u8] = include_bytes!("fixtures/gradient.jpg");
const HELLO_BYTES: &[u8] = b"Hello, TableX!\x00\x01\xfe\xff";

/// A query tab whose `data` column holds: 0 a PNG, 1 a JPEG, 2 `HELLO_BYTES`, 3 NULL, 4 a value one
/// byte over the preview cap, 5 UTF-8 text with accents, 6 a megabyte of a counting pattern.
fn binary_fixture_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    let big = vec![0xab_u8; crate::binary_value::PREVIEW_CAP_BYTES + 1];
    let megabyte: Vec<u8> = (0..1_048_576u32).map(|n| (n % 251) as u8).collect();
    let values: Vec<Option<&[u8]>> = vec![
        Some(GRADIENT_PNG),
        Some(GRADIENT_JPEG),
        Some(HELLO_BYTES),
        None,
        Some(&big),
        Some("héllo wörld".as_bytes()),
        Some(&megabyte),
    ];
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("data", arrow_schema::DataType::Binary, true),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from_iter_values(0..values.len() as i64))
                as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::BinaryArray::from(values)) as arrow_array::ArrayRef,
        ],
    )
    .unwrap();
    app.update(visual, |this, cx| {
        let tab = this.workspace.open_ran_query_tab("blobs");
        let runtime = this.workspace.runtime_mut(tab).unwrap();
        runtime.grid = GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![
            std::sync::Arc::new(batch),
        ])));
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    (app, visual)
}

/// Selects `row` of the `data` column and shows the Details panel on it.
fn show_binary_detail(
    app: &gpui::Entity<AppRoot>,
    visual: &mut gpui::VisualTestContext,
    row: usize,
) {
    app.update(visual, |this, cx| {
        let tab = this.active_id();
        let position = CellPosition { row, column: 1 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(visual);
}

fn selected_binary(app: &gpui::Entity<AppRoot>, visual: &gpui::VisualTestContext) -> crate::cell_detail::BinaryDetail {
    app.read_with(visual, |this, _| {
        this.selected_cell_detail(this.active_id())
            .and_then(|detail| detail.binary.clone())
            .expect("the selected cell is binary")
    })
}

#[gpui::test]
fn a_binary_cell_reads_as_a_short_hex_prefix_and_size_in_the_grid(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    app.read_with(visual, |this, _| {
        let grid = this.active_grid();
        let text = |row| grid.cell_text(CellPosition { row, column: 1 }).unwrap().to_string();
        assert_eq!(text(2), "0x48656c6c6f2c205461626c6558210001feff", "a short value is whole");
        let png = text(0);
        assert!(png.starts_with("0x89504e470d0a1a0a"), "{png}");
        assert!(png.ends_with(&format!("… ({})", crate::binary_value::format_size(GRADIENT_PNG.len()))), "{png}");
        assert!(png.len() < 100, "a cell never formats the whole value: {png}");
        // Copy and the Value panel use the whole value.
        let full = grid.full_cell_text(CellPosition { row: 0, column: 1 }).unwrap();
        assert_eq!(full, crate::binary_value::to_hex_literal(GRADIENT_PNG));
        let huge = text(4);
        assert!(huge.ends_with("… (16.0 MB)"), "{huge}");
    });
}

#[gpui::test]
fn the_hex_view_shows_offset_hex_and_ascii_rows_of_the_selected_value(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 2);
    assert!(visual.debug_bounds("cell-detail-binary-hex").is_some(), "no image: opens on the hex dump");
    assert!(visual.debug_bounds("cell-detail-binary-mode-image").is_none(), "only a picture offers Image");
    // 18 bytes at the side panel's 8 bytes per row: three rows.
    for row in 0..3 {
        let selector: &'static str = Box::leak(format!("cell-detail-binary-hex-row-{row}").into_boxed_str());
        assert!(visual.debug_bounds(selector).is_some(), "{selector}");
    }
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-3").is_none());
    let binary = selected_binary(&app, visual);
    assert_eq!(binary.len, HELLO_BYTES.len());
    assert!(binary.image.is_none() && binary.picture.is_none());
    assert!(visual.debug_bounds("cell-detail-binary-info").is_some());
    // The length row counts bytes, not hex digits.
    app.read_with(visual, |this, _| {
        assert_eq!(this.selected_cell_detail(this.active_id()).unwrap().length, HELLO_BYTES.len());
    });

    // Copy puts every byte on the clipboard as 0x hex.
    click_selector(visual, "cell-detail-copy-value");
    assert_eq!(
        visual.read_from_clipboard().and_then(|item| item.text()).as_deref(),
        Some("0x48656c6c6f2c205461626c6558210001feff")
    );
}

#[gpui::test]
fn a_megabyte_value_paints_only_the_visible_hex_rows(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 6);
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-0").is_some());
    let painted = (0..200)
        .filter(|row| {
            let selector: &'static str = Box::leak(format!("cell-detail-binary-hex-row-{row}").into_boxed_str());
            visual.debug_bounds(selector).is_some()
        })
        .count();
    assert!(painted > 3 && painted < 80, "{painted} of 131072 rows painted");
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-100000").is_none());
    // Scrolling the list moves the window of rows, the value is not copied again.
    app.update(visual, |this, _| {
        this.av().detail_ui.binary.hex_scroll.scroll_to_item(100_000, gpui::ScrollStrategy::Top);
    });
    settle(visual);
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-100000").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-0").is_none());
}

#[gpui::test]
fn the_text_view_decodes_utf8_or_latin1_by_choice(cx: &mut TestAppContext) {
    use crate::binary_value::TextEncoding;
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 5);
    click_selector(visual, "cell-detail-binary-mode-text");
    assert!(visual.debug_bounds("cell-detail-binary-text").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-hex").is_none());
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().detail_ui.binary.encoding, TextEncoding::Utf8);
        let detail = this.selected_cell_detail(this.active_id()).unwrap();
        let bytes = this.cell_detail_bytes(this.active_id(), detail).unwrap();
        assert_eq!(
            crate::binary_value::decode_text(bytes, TextEncoding::Utf8).0,
            "héllo wörld"
        );
    });
    click_selector(visual, "cell-detail-binary-encoding-latin1");
    app.read_with(visual, |this, _| {
        assert_eq!(this.av().detail_ui.binary.encoding, TextEncoding::Latin1);
        let detail = this.selected_cell_detail(this.active_id()).unwrap();
        let bytes = this.cell_detail_bytes(this.active_id(), detail).unwrap();
        assert_eq!(
            crate::binary_value::decode_text(bytes, TextEncoding::Latin1).0,
            "hÃ©llo wÃ¶rld"
        );
    });
    // Another cell opens on its own default view again.
    show_binary_detail(&app, visual, 2);
    assert!(visual.debug_bounds("cell-detail-binary-hex").is_some());
}

#[gpui::test]
fn png_and_jpeg_values_are_detected_sized_and_rendered_as_images(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    for (row, kind, bytes) in [
        (0, crate::binary_value::ImageKind::Png, GRADIENT_PNG),
        (1, crate::binary_value::ImageKind::Jpeg, GRADIENT_JPEG),
    ] {
        show_binary_detail(&app, visual, row);
        let binary = selected_binary(&app, visual);
        assert_eq!(binary.image, Some(kind));
        assert_eq!(binary.dimensions, Some((96, 64)), "{kind:?}");
        assert_eq!(binary.len, bytes.len());
        assert!(visual.debug_bounds("cell-detail-binary-image").is_some(), "{kind:?} opens on the image");
        assert!(visual.debug_bounds("cell-detail-binary-hex").is_none());
        // The same bytes GPUI draws decode to the size the header names.
        let picture = binary.picture.clone().expect("a picture");
        let decoded = visual.update(|_, cx| picture.to_image_data(cx.svg_renderer())).expect("GPUI decodes it");
        assert_eq!(decoded.size(0), gpui::size(gpui::DevicePixels(96), gpui::DevicePixels(64)), "{kind:?}");

        // Fit scales into the panel; 100% paints the actual pixels.
        let fit = visual.debug_bounds("cell-detail-binary-image").unwrap();
        click_selector(visual, "cell-detail-binary-zoom-actual");
        let actual = visual.debug_bounds("cell-detail-binary-image").unwrap();
        assert_eq!(actual.size, gpui::size(gpui::px(96.0), gpui::px(64.0)), "{kind:?} at 100%");
        assert!(fit.size.width >= actual.size.width, "fit fills the box, {:?} vs {:?}", fit.size, actual.size);
        click_selector(visual, "cell-detail-binary-zoom-fit");
        // The hex dump of the picture is one click away.
        click_selector(visual, "cell-detail-binary-mode-hex");
        assert!(visual.debug_bounds("cell-detail-binary-hex").is_some());
        assert!(visual.debug_bounds("cell-detail-binary-image").is_none());
        click_selector(visual, "cell-detail-binary-mode-image");
        assert!(visual.debug_bounds("cell-detail-binary-image").is_some());
        app.update(visual, |this, _| this.av_mut().detail_ui.binary.zoom = Default::default());
    }
}

#[gpui::test]
fn a_value_over_the_cap_is_not_decoded_but_saves_whole(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 4);
    let binary = selected_binary(&app, visual);
    assert_eq!(binary.len, crate::binary_value::PREVIEW_CAP_BYTES + 1);
    assert!(!binary.previewable());
    assert!(binary.picture.is_none());
    assert!(visual.debug_bounds("cell-detail-binary-too-large").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-hex").is_none());
    app.read_with(visual, |this, _| {
        let detail = this.selected_cell_detail(this.active_id()).unwrap();
        // The panel did not build the 32 MB hex text of the value.
        assert!(detail.value.len() < 200, "{}", detail.value.len());
        assert_eq!(detail.length, crate::binary_value::PREVIEW_CAP_BYTES + 1);
    });

    let path = std::env::temp_dir().join(format!("tablex-blob-{}.bin", uuid::Uuid::new_v4()));
    click_selector(visual, "cell-detail-binary-save");
    assert!(visual.did_prompt_for_new_path(), "Save asks where to write");
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    visual.run_until_parked();
    wait_for_file_len(visual, &path, crate::binary_value::PREVIEW_CAP_BYTES + 1);
    assert!(std::fs::read(&path).unwrap().iter().all(|byte| *byte == 0xab));
    std::fs::remove_file(path).unwrap();
}

/// Waits (the write runs on a background thread) until `path` holds `len` bytes.
fn wait_for_file_len(visual: &mut gpui::VisualTestContext, path: &std::path::Path, len: usize) {
    wait_until(visual, "the file to be written", |_| {
        std::fs::metadata(path).is_ok_and(|meta| meta.len() as usize == len)
    });
}

#[gpui::test]
fn save_to_file_writes_the_image_bytes_through_the_prompt_and_cancel_writes_nothing(
    cx: &mut TestAppContext,
) {
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 0);
    let path = std::env::temp_dir().join(format!("tablex-png-{}.png", uuid::Uuid::new_v4()));
    click_selector(visual, "cell-detail-binary-save");
    assert!(visual.did_prompt_for_new_path());
    // Cancel: the prompt answers nothing.
    visual.simulate_new_path_selection(|_| None);
    settle(visual);
    assert!(!path.exists());
    app.read_with(visual, |this, _| assert!(this.av().detail_ui.error.is_none()));

    click_selector(visual, "cell-detail-binary-save");
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    wait_for_file_len(visual, &path, GRADIENT_PNG.len());
    assert_eq!(std::fs::read(&path).unwrap(), GRADIENT_PNG);
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn a_read_only_result_offers_save_but_not_load(cx: &mut TestAppContext) {
    let (app, visual) = binary_fixture_app(cx);
    show_binary_detail(&app, visual, 2);
    assert!(visual.debug_bounds("cell-detail-binary-save").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-load").is_none(), "a query result has no key to write by");
    assert!(visual.debug_bounds("cell-detail-edit").is_none());
    // NULL has no bytes: the plain NULL value box.
    show_binary_detail(&app, visual, 3);
    assert!(visual.debug_bounds("cell-detail-binary-modes").is_none());
    app.read_with(visual, |this, _| {
        assert!(this.selected_cell_detail(this.active_id()).unwrap().binary.is_none());
    });
}

const BLOB_SEED: &str = "CREATE TABLE files(id INTEGER PRIMARY KEY, name TEXT, data BLOB); \
     INSERT INTO files(name, data) VALUES ('a', x'00010203'), ('b', NULL);";

#[gpui::test]
fn loading_a_file_into_a_blob_cell_stages_it_and_apply_writes_the_bytes(cx: &mut TestAppContext) {
    let db = SqliteDb::new(BLOB_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("files");
    rig.wait_structure();
    let dir = std::env::temp_dir().join(format!("tablex-blob-load-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("picture.png");
    std::fs::write(&source, GRADIENT_PNG).unwrap();

    // The BLOB column arrives as bytes: the cell shows them as hex, the panel offers Hex/Text.
    rig.app.update(rig.visual, |this, cx| {
        let tab = this.active_id();
        let position = CellPosition { row: 0, column: 2 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(rig.visual);
    let before = selected_binary(&rig.app, rig.visual);
    assert_eq!(before.len, 4);
    assert!(rig.visual.debug_bounds("cell-detail-binary-hex-row-0").is_some());

    rig.click("cell-detail-binary-load");
    assert!(rig.visual.did_prompt_for_paths(), "Load asks which file");
    rig.visual.simulate_path_prompt_response(|options| {
        assert!(options.files && !options.multiple);
        Some(vec![source.clone()])
    });
    let (app, tab) = (rig.app.clone(), rig.tab);
    wait_until(rig.visual, "the file to be staged", |visual| {
        app.read_with(visual, |this, _| {
            this.workspace.runtime(tab).unwrap().pending_edits.len() == 1
        })
    });
    settle(rig.visual);
    assert_eq!(rig.staged(), (1, 0));
    assert_eq!(rig.detail_error(), None);
    // The staged value is what the panel shows now: an image of the loaded file.
    let staged = selected_binary(&rig.app, rig.visual);
    assert_eq!(staged.len, GRADIENT_PNG.len());
    assert_eq!(staged.image, Some(crate::binary_value::ImageKind::Png));
    assert!(rig.visual.debug_bounds("cell-detail-binary-image").is_some());
    // The grid cell lays out a short form of the staged hex, never megabytes of it.
    rig.app.read_with(rig.visual, |this, _| {
        let cell = this.workspace.runtime(rig.tab).unwrap().pending_cell_text(CellPosition { row: 0, column: 2 }).unwrap().len();
        assert_eq!(cell, 2 + 2 * GRADIENT_PNG.len());
    });
    assert_eq!(db.sql("SELECT hex(data) FROM files WHERE id = 1"), "00010203", "nothing is written before Apply");

    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(rig.staged(), (0, 0));
    assert_eq!(
        db.sql("SELECT hex(data) FROM files WHERE id = 1"),
        crate::binary_value::to_hex_literal(GRADIENT_PNG)[2..].to_uppercase()
    );
    assert_eq!(db.sql("SELECT typeof(data) FROM files WHERE id = 1"), "blob");
    // Another row, left alone.
    assert_eq!(db.sql("SELECT count(*) FROM files WHERE data IS NULL"), "1");

    // A file over the cap is refused with its reason and stages nothing.
    let big = dir.join("big.bin");
    std::fs::write(&big, vec![1u8; crate::binary_value::PREVIEW_CAP_BYTES + 1]).unwrap();
    rig.wait_not_running("the reload after Apply");
    rig.app.update(rig.visual, |this, cx| {
        let tab = this.active_id();
        let position = CellPosition { row: 1, column: 2 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(rig.visual);
    assert!(
        rig.visual.debug_bounds("cell-detail-binary-load").is_some(),
        "a NULL cell of a BLOB column can be given a value from a file"
    );
    assert!(rig.visual.debug_bounds("cell-detail-binary-modes").is_none(), "but has no bytes to view");
    rig.click("cell-detail-binary-load");
    rig.visual.simulate_path_prompt_response(|_| Some(vec![big.clone()]));
    wait_until(rig.visual, "the refusal", |visual| app.read_with(visual, |this, _| this.av().detail_ui.error.is_some()));
    assert!(rig.detail_error().unwrap().contains("larger than 16.0 MB"), "{:?}", rig.detail_error());
    assert_eq!(rig.staged(), (0, 0));
    std::fs::remove_dir_all(&dir).unwrap();
}

// --- live servers: the binary type of each SQL driver reaches the panel as bytes and a loaded file
// --- is written back byte for byte --------------------------------------------------------------

const LIVE_BLOB_TABLE: &str = "tablex_p5_blobs";

/// A table with a binary column on a live server: the stored bytes are what the panel reads, a PNG
/// loaded through the panel is staged, applied, and read back identical, and the record view shows it.
fn live_binary_roundtrip(
    cx: &mut TestAppContext,
    kind: ConnectionKind,
    driver: (&'static str, &'static str),
    env: &str,
    schema: Option<&str>,
    (create, insert): (&str, &str),
) {
    let Ok(uri) = std::env::var(env) else {
        eprintln!("skipped: set {env} to a dedicated tablex-test-* container URI");
        return;
    };
    let mut rig = SqlRig::new(cx, kind, driver, &uri);
    let _cleanup = DropScratchTable(rig.session.clone(), LIVE_BLOB_TABLE);
    exec_confirmed(&rig.session, &format!("DROP TABLE IF EXISTS {LIVE_BLOB_TABLE}"));
    exec_confirmed(&rig.session, create);
    exec_confirmed(&rig.session, insert);
    rig.visual.simulate_resize(gpui::size(gpui::px(1400.0), gpui::px(850.0)));
    rig.open_table_in("tablex", schema, LIVE_BLOB_TABLE);
    rig.rerun_table("", "id");
    rig.wait_structure();
    let (app, tab) = (rig.app.clone(), rig.tab);
    // The stored bytes, whatever the driver's wire form (Arrow binary, or PostgreSQL's `\x` text).
    let bytes_of = |rig: &mut SqlRig<'_>, row: usize| -> Option<Vec<u8>> {
        rig.app.update(rig.visual, |this, cx| {
            let position = CellPosition { row, column: 1 };
            this.select_grid_cell(tab, position);
            this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
        });
        settle(rig.visual);
        rig.app.read_with(rig.visual, |this, _| {
            let detail = this.selected_cell_detail(tab)?;
            this.cell_detail_bytes(tab, detail).map(<[u8]>::to_vec)
        })
    };
    assert_eq!(bytes_of(&mut rig, 0), Some(vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x01]));
    assert_eq!(bytes_of(&mut rig, 1), None, "NULL");
    // The panel and the record view read the same bytes.
    rig.click("record-view-toggle");
    rig.app.update(rig.visual, |this, cx| {
        let position = CellPosition { row: 0, column: 1 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(rig.visual);
    assert!(rig.visual.debug_bounds("cell-detail-binary-hex-row-0").is_some());
    assert!(rig.visual.debug_bounds("record-value-1").is_some());

    // Load a PNG into row 2's NULL, Apply, read it back.
    let dir = std::env::temp_dir().join(format!("tablex-live-blob-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("picture.png");
    std::fs::write(&source, GRADIENT_PNG).unwrap();
    rig.app.update(rig.visual, |this, cx| {
        let position = CellPosition { row: 1, column: 1 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
        this.load_cell_binary(tab, cx);
    });
    rig.visual.simulate_path_prompt_response(|_| Some(vec![source.clone()]));
    wait_until(rig.visual, "the file to be staged", |visual| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().pending_edits.len() == 1)
    });
    assert_eq!(rig.detail_error(), None);
    rig.apply();
    rig.wait_apply_finished();
    assert!(rig.log().is_empty(), "{:?}", rig.log());
    rig.wait_not_running("the reload after Apply");
    assert_eq!(bytes_of(&mut rig, 1).as_deref(), Some(GRADIENT_PNG), "the loaded file is what the server stores");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_PG_URI (container tablex-test-pg)"]
fn live_postgresql_bytea_reaches_the_panel_and_a_file_loads_back(cx: &mut TestAppContext) {
    live_binary_roundtrip(
        cx,
        ConnectionKind::Postgres,
        ("dbengine-postgres", "abi-v2"),
        "TABLEX_TEST_PG_URI",
        Some("public"),
        (
            &format!("CREATE TABLE {LIVE_BLOB_TABLE} (id integer PRIMARY KEY, data bytea)"),
            &format!("INSERT INTO {LIVE_BLOB_TABLE} VALUES (1, '\\xdeadbeef0001'::bytea), (2, NULL)"),
        ),
    );
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MYSQL_URI (container tablex-test-mysql)"]
fn live_mysql_blob_reaches_the_panel_and_a_file_loads_back(cx: &mut TestAppContext) {
    live_binary_roundtrip(
        cx,
        ConnectionKind::Mysql,
        ("dbengine-mysql", "abi-v2"),
        "TABLEX_TEST_MYSQL_URI",
        None,
        (
            &format!("CREATE TABLE {LIVE_BLOB_TABLE} (id int PRIMARY KEY, data longblob)"),
            &format!("INSERT INTO {LIVE_BLOB_TABLE} VALUES (1, X'DEADBEEF0001'), (2, NULL)"),
        ),
    );
}

#[gpui::test]
#[ignore = "needs TABLEX_TEST_MARIADB_URI (container tablex-test-mariadb)"]
fn live_mariadb_blob_reaches_the_panel_and_a_file_loads_back(cx: &mut TestAppContext) {
    live_binary_roundtrip(
        cx,
        ConnectionKind::Mariadb,
        ("dbengine-mariadb", "abi-v2"),
        "TABLEX_TEST_MARIADB_URI",
        None,
        (
            &format!("CREATE TABLE {LIVE_BLOB_TABLE} (id int PRIMARY KEY, data longblob)"),
            &format!("INSERT INTO {LIVE_BLOB_TABLE} VALUES (1, X'DEADBEEF0001'), (2, NULL)"),
        ),
    );
}

// --- review round: text that is not valid UTF-8, large PostgreSQL bytea, pins during Load, Tab ---

/// A query tab whose column `data` is `bytea` text (`\x…`, as the PostgreSQL driver delivers it):
/// 0 a short value, 1 a megabyte, 2 one byte over the preview cap, 3 NULL.
fn bytea_text_app(
    cx: &mut TestAppContext,
) -> (gpui::Entity<AppRoot>, &mut gpui::VisualTestContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual
        .cx
        .read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    let hex = |byte: &str, count: usize| format!("\\x{}", byte.repeat(count));
    let values = vec![
        Some("\\xdeadbeef".to_string()),
        Some(hex("ab", 1 << 20)),
        Some(hex("cd", crate::binary_value::PREVIEW_CAP_BYTES + 1)),
        None,
    ];
    let metadata = std::collections::HashMap::from([(
        crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
        "bytea".to_string(),
    )]);
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("data", arrow_schema::DataType::Utf8, true).with_metadata(metadata),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from_iter_values(0..4)) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::StringArray::from(values)) as arrow_array::ArrayRef,
        ],
    )
    .unwrap();
    app.update(visual, |this, cx| {
        let tab = this.workspace.open_ran_query_tab("bytea");
        this.workspace.runtime_mut(tab).unwrap().grid =
            GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![std::sync::Arc::new(batch)])));
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    (app, visual)
}

#[gpui::test]
fn a_large_postgres_bytea_is_measured_not_decoded_or_copied_when_selected(cx: &mut TestAppContext) {
    let (app, visual) = bytea_text_app(cx);
    // A short value reads as bytes: hex rows, 4 bytes, copy gives the server's own text back.
    show_binary_detail(&app, visual, 0);
    let binary = selected_binary(&app, visual);
    assert_eq!((binary.len, binary.form), (4, crate::binary_value::HexForm::PgBytea));
    assert!(visual.debug_bounds("cell-detail-binary-hex-row-0").is_some());
    click_selector(visual, "cell-detail-copy-value");
    assert_eq!(visual.read_from_clipboard().and_then(|item| item.text()).as_deref(), Some("\\xdeadbeef"));

    // A megabyte: decoded once into the panel's own bytes, the 2 MB text is not copied into the detail.
    show_binary_detail(&app, visual, 1);
    let binary = selected_binary(&app, visual);
    assert_eq!(binary.len, 1 << 20);
    assert_eq!(binary.owned.as_ref().map(|bytes| bytes.len()), Some(1 << 20));
    app.read_with(visual, |this, _| {
        let detail = this.selected_cell_detail(this.active_id()).unwrap();
        assert!(detail.value.len() < 200, "{}: the whole text was copied", detail.value.len());
        assert_eq!(detail.length, 1 << 20);
    });

    // One byte over the cap: only measured. Nothing decoded, nothing copied, no preview ...
    show_binary_detail(&app, visual, 2);
    let binary = selected_binary(&app, visual);
    assert_eq!(binary.len, crate::binary_value::PREVIEW_CAP_BYTES + 1);
    assert!(binary.owned.is_none() && binary.picture.is_none() && !binary.previewable());
    app.read_with(visual, |this, _| {
        assert!(this.selected_cell_detail(this.active_id()).unwrap().value.len() < 200);
    });
    assert!(visual.debug_bounds("cell-detail-binary-too-large").is_some());
    // ... and Save decodes it only then, straight from the page.
    let path = std::env::temp_dir().join(format!("tablex-bytea-{}.bin", uuid::Uuid::new_v4()));
    click_selector(visual, "cell-detail-binary-save");
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    wait_for_file_len(visual, &path, crate::binary_value::PREVIEW_CAP_BYTES + 1);
    assert!(std::fs::read(&path).unwrap().iter().all(|byte| *byte == 0xcd));
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn text_that_is_not_utf8_has_bytes_to_view_but_is_never_loaded_from_a_file(cx: &mut TestAppContext) {
    cx.update(|app| {
        gpui_component::init(app);
        theme::init(app);
        actions::bind_keys(app);
    });
    let (root, visual) = cx.add_window_view(|window, cx| {
        let view = cx.new(AppRoot::new_for_test);
        gpui_component::Root::new(view, window, cx)
    });
    let app = visual.cx.read(|cx| root.read(cx).view().clone().downcast::<AppRoot>().unwrap());
    visual.simulate_resize(gpui::size(gpui::px(1200.0), gpui::px(700.0)));
    // A MySQL VARCHAR holding invalid UTF-8 arrives as Arrow binary tagged `(invalid utf-8)`.
    let metadata = std::collections::HashMap::from([(
        crate::grid_arrow::NATIVE_TYPE_METADATA_KEY.to_string(),
        "varchar (invalid utf-8)".to_string(),
    )]);
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![
        arrow_schema::Field::new("id", arrow_schema::DataType::Int64, false),
        arrow_schema::Field::new("data", arrow_schema::DataType::Binary, true).with_metadata(metadata),
    ]));
    let batch = arrow_array::RecordBatch::try_new(
        schema,
        vec![
            std::sync::Arc::new(arrow_array::Int64Array::from_iter_values(0..2)) as arrow_array::ArrayRef,
            std::sync::Arc::new(arrow_array::BinaryArray::from(vec![None, Some(&b"\xff\xfeabc"[..])]))
                as arrow_array::ArrayRef,
        ],
    )
    .unwrap();
    app.update(visual, |this, cx| {
        let tab = this.workspace.open_ran_query_tab("invalid");
        this.workspace.runtime_mut(tab).unwrap().grid =
            GridState::new(GridContentSource::Arrow(ArrowGridSource::new(vec![std::sync::Arc::new(batch)])));
        this.reset_active_view();
        cx.notify();
    });
    settle(visual);
    // The NULL cell: no value from a file (it would be stored as the literal text `0x…`).
    show_binary_detail(&app, visual, 0);
    app.read_with(visual, |this, _| {
        let detail = this.selected_cell_detail(this.active_id()).unwrap();
        assert_eq!(detail.binary_form, None);
    });
    assert!(visual.debug_bounds("cell-detail-binary-load").is_none());
    app.update(visual, |this, cx| this.load_cell_binary(this.active_id(), cx));
    assert!(!visual.did_prompt_for_paths(), "Load asks nothing on such a column");
    // The bytes of the other cell can still be looked at and saved.
    show_binary_detail(&app, visual, 1);
    assert_eq!(selected_binary(&app, visual).len, 5);
    assert!(visual.debug_bounds("cell-detail-binary-hex").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-save").is_some());
    assert!(visual.debug_bounds("cell-detail-binary-load").is_none());
    app.read_with(visual, |this, _| {
        assert_eq!(this.selected_cell_detail(this.active_id()).unwrap().binary_form, None);
    });
    let path = std::env::temp_dir().join(format!("tablex-invalid-{}.bin", uuid::Uuid::new_v4()));
    click_selector(visual, "cell-detail-binary-save");
    visual.simulate_new_path_selection(|_| Some(path.clone()));
    wait_for_file_len(visual, &path, 5);
    assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xfeabc");
    std::fs::remove_file(path).unwrap();
}

#[gpui::test]
fn a_sqlite_text_cell_with_invalid_utf8_is_not_a_load_target(cx: &mut TestAppContext) {
    let (app, visual) = context_test_app(cx, "result-grid", "sample");
    let mixed = crate::grid_arrow::tests::mixed_column();
    let schema = std::sync::Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "m",
        mixed.data_type().clone(),
        true,
    )]));
    let source = ArrowGridSource::new(vec![std::sync::Arc::new(
        arrow_array::RecordBatch::try_new(schema, vec![mixed]).unwrap(),
    )]);
    app.update(visual, |this, _| {
        this.set_active_grid(GridState::new(GridContentSource::Arrow(source.clone())));
        let tab = this.workspace.active_tab().cloned().unwrap();
        let class = |row| this.cell_class(tab.id, row, 0);
        let forms: Vec<_> = (0..crate::grid::GridSource::row_count(&source))
            .filter_map(|row| class(row).map(|class| (class, this.binary_column_form(&tab, row, 0, source.binary_cell(row, 0).is_some()))))
            .collect();
        assert!(forms.contains(&(crate::grid_arrow::CellClass::Blob, Some(crate::binary_value::HexForm::ZeroX))), "{forms:?}");
        assert!(
            forms.contains(&(crate::grid_arrow::CellClass::InvalidUtf8Text, None)),
            "invalid UTF-8 text is not binary data: {forms:?}"
        );
    });
}

#[gpui::test]
fn a_file_loaded_after_a_pin_lands_on_the_column_the_cell_has_now(cx: &mut TestAppContext) {
    let db = SqliteDb::new(BLOB_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("files");
    rig.wait_structure();
    let (app, tab) = (rig.app.clone(), rig.tab);
    let dir = std::env::temp_dir().join(format!("tablex-load-pin-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("picture.png");
    std::fs::write(&source, GRADIENT_PNG).unwrap();
    // Row 2's `data` (display column 2) is NULL; its panel offers Load, and the file dialog is open.
    rig.app.update(rig.visual, |this, cx| {
        let position = CellPosition { row: 1, column: 2 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
    });
    settle(rig.visual);
    rig.click("cell-detail-binary-load");
    assert!(rig.visual.did_prompt_for_paths());
    // The (non-modal) dialog is open: the user pins `data`, which becomes display column 0.
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 2, cx));
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| assert_eq!(this.view(tab).grid_ui.visible_columns, [2, 0, 1]));
    rig.visual.simulate_path_prompt_response(|_| Some(vec![source.clone()]));
    wait_until(rig.visual, "the file to be staged", |visual| {
        app.read_with(visual, |this, _| this.workspace.runtime(tab).unwrap().pending_edits.len() == 1)
    });
    app.read_with(rig.visual, |this, _| {
        let runtime = this.workspace.runtime(tab).unwrap();
        assert_eq!(runtime.pending_edits[0].column, 0, "the staged edit is under the column `data` has now");
        assert!(runtime.pending_cell_text(CellPosition { row: 1, column: 0 }).is_some());
        assert!(runtime.pending_cell_text(CellPosition { row: 1, column: 2 }).is_none());
    });
    rig.apply();
    rig.wait_apply_finished();
    assert_eq!(db.sql("SELECT typeof(data) || ':' || typeof(name) FROM files WHERE id = 2"), "blob:text");
    assert_eq!(db.sql("SELECT name FROM files WHERE id = 2"), "b", "no other column was written");
    std::fs::remove_dir_all(&dir).unwrap();

    // A cell editor open in the panel also holds the columns where they are.
    rig.wait_not_running("the reload after Apply");
    rig.app.update_in(rig.visual, |this, window, cx| {
        let position = CellPosition { row: 0, column: 1 };
        this.select_grid_cell(tab, position);
        this.open_cell_detail_tab_at(tab, CellDetailTab::Details, Some(position), cx);
        this.open_cell_detail_editor(tab, window, cx);
    });
    settle(rig.visual);
    rig.app.read_with(rig.visual, |this, _| assert!(this.view(tab).detail_ui.edit.is_some()));
    let before = rig.app.read_with(rig.visual, |this, _| this.view(tab).grid_ui.visible_columns.clone());
    rig.app.update(rig.visual, |this, cx| this.pin_column(tab, 1, cx));
    rig.app.read_with(rig.visual, |this, _| {
        assert_eq!(this.view(tab).grid_ui.visible_columns, before, "refused while the panel's editor is open");
    });
}

#[gpui::test]
fn tab_inside_a_cell_editor_does_not_switch_the_view(cx: &mut TestAppContext) {
    let db = SqliteDb::new(EDIT_ITEMS_SEED);
    let mut rig = SqlRig::sqlite(cx, &db);
    rig.open_table("items");
    rig.wait_structure();
    rig.visual.update(|window, cx| {
        crate::app::actions::bind_keys(cx);
        window.activate_window();
    });
    let (app, tab) = (rig.app.clone(), rig.tab);
    // In the table.
    rig.double_click_cell(0, 1);
    assert_eq!(rig.editing(), Some(CellPosition { row: 0, column: 1 }));
    rig.visual.simulate_keystrokes("tab");
    settle(rig.visual);
    assert!(!app.read_with(rig.visual, |this, _| this.view(tab).grid_ui.record), "Tab in the grid's editor is the editor's");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    assert_eq!(rig.editing(), None);
    // And in the record view.
    rig.click("record-view-toggle");
    let value = rig.visual.debug_bounds("record-value-1").expect("the name value");
    double_click(rig.visual, value.center());
    assert_eq!(rig.editing(), Some(CellPosition { row: 0, column: 1 }));
    rig.visual.simulate_keystrokes("tab");
    settle(rig.visual);
    assert!(app.read_with(rig.visual, |this, _| this.view(tab).grid_ui.record), "Tab in the record's editor does not leave it");
    rig.visual.simulate_keystrokes("escape");
    settle(rig.visual);
    // With no editor open Tab switches again (the click gives the record the focus).
    click_selector(rig.visual, "record-field-0");
    rig.visual.simulate_keystrokes("tab");
    settle(rig.visual);
    assert!(!app.read_with(rig.visual, |this, _| this.view(tab).grid_ui.record));
}
