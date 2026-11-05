#[cfg(test)]
mod tests {
    use super::*;

    use super::super::copy::{CopyCell, CopyData};

    #[test]
    fn offscreen_selection_bounds_require_every_cell_or_explicit_error() {
        let bounds = SnapshotBounds::new(200, 8, 160, 26).unwrap();
        assert_eq!(bounds.width, 1280);
        assert_eq!(bounds.height, 5200);
        assert!(SnapshotBounds::new(10_000, 8, 160, 26).is_err());
        assert!(SnapshotBounds::new(0, 8, 160, 26).is_err());
        let mut layout = crate::grid::ColumnLayout::new(2);
        layout.set_width(0, 100.0);
        layout.set_width(1, 200.0);
        let range = crate::grid::CellRange {
            start: crate::grid::CellPosition { row: 0, column: 0 },
            end: crate::grid::CellPosition {
                row: 199,
                column: 1,
            },
        };
        let selected = SnapshotBounds::from_layout(range, &layout).unwrap();
        assert_eq!(selected.width, 300);
        assert_eq!(selected.height, 5200);
        layout.widths[1] = f32::NAN;
        assert!(SnapshotBounds::from_layout(range, &layout).is_err());
    }

    #[test]
    fn selected_dataset_renders_escaped_unicode_svg_to_decodable_png() {
        let selected = CopyData {
            columns: vec!["<&>".into(), "Name".into()],
            rows: vec![
                vec![
                    CopyCell::new("first & <script>", "text", false),
                    CopyCell::new("雪", "text", false),
                ],
                vec![
                    CopyCell::new("second", "text", false),
                    CopyCell::new("a\nb", "text", false),
                ],
            ],
            table: String::new(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: super::super::copy::SqlTarget::POSTGRES,
        };
        let plan = SnapshotTable::new(&selected, &[100, 100]).unwrap();
        let svg = plan.to_svg().unwrap();
        assert!(svg.contains("&lt;&amp;&gt;"));
        assert!(svg.contains("first &amp; &lt;script&gt;"));
        assert!(svg.contains("雪"));
        assert!(!svg.contains("<script>"));
        let png = plan.render_png().unwrap();
        let image = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(
            (image.width(), image.height()),
            (plan.bounds.width, plan.bounds.height)
        );
        assert!(image.pixels().any(|pixel| pixel.0[0] < 100));
        let changed = CopyData {
            columns: vec!["<&>".into(), "Name".into()],
            rows: vec![
                vec![
                    CopyCell::new("other & <script>", "text", false),
                    CopyCell::new("雪", "text", false),
                ],
                vec![
                    CopyCell::new("second", "text", false),
                    CopyCell::new("a\nb", "text", false),
                ],
            ],
            table: String::new(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: super::super::copy::SqlTarget::POSTGRES,
        };
        assert_ne!(
            png,
            SnapshotTable::new(&changed, &[100, 100])
                .unwrap()
                .render_png()
                .unwrap(),
            "text glyphs must affect the pixels"
        );
    }

    #[test]
    fn snapshot_preflight_rejects_large_values_before_pixmap_allocation() {
        let selected = CopyData {
            columns: vec!["x".into()],
            rows: vec![vec![CopyCell::new(&"a".repeat(10_000), "text", false)]],
            table: String::new(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: super::super::copy::SqlTarget::POSTGRES,
        };
        assert!(SnapshotTable::new(&selected, &[100]).is_err());
    }

    #[test]
    fn snapshot_rejects_invalid_xml_controls_without_rendering() {
        let selected = CopyData {
            columns: vec!["name".into()],
            rows: vec![vec![CopyCell::new("bad\u{1}value", "text", false)]],
            table: String::new(),
            schema: None,
            key_column: None,
            keys: vec![],
            target: super::super::copy::SqlTarget::POSTGRES,
        };
        let plan = SnapshotTable::new(&selected, &[160]).unwrap();
        assert!(plan.to_svg().is_err());
        assert!(plan.render_png().is_err());
    }

    #[test]
    fn atomic_png_publish_never_overwrites_existing_destination() {
        let path =
            std::env::temp_dir().join(format!("tablex-snapshot-{}.png", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"existing").unwrap();
        assert!(write_png_new(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
        std::fs::remove_file(&path).unwrap();
        write_png_new(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        std::fs::remove_file(path).unwrap();
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct SnapshotBounds {
    pub width: u32,
    pub height: u32,
}

impl SnapshotBounds {
    pub(in crate::app) fn from_layout(
        range: crate::grid::CellRange,
        layout: &crate::grid::ColumnLayout,
    ) -> Result<Self, String> {
        let range = range.normalized();
        let columns = layout
            .widths
            .get(range.start.column..=range.end.column)
            .ok_or("Selection extends beyond grid columns")?;
        if columns
            .iter()
            .any(|width| !width.is_finite() || *width <= 0.0)
        {
            return Err("Snapshot column width is invalid".into());
        }
        let width = columns
            .iter()
            .map(|width| width.ceil() as u32)
            .try_fold(0u32, |total, width| {
                total.checked_add(width).ok_or("Snapshot width overflow")
            })?;
        Self::new(
            range
                .end
                .row
                .checked_sub(range.start.row)
                .and_then(|n| n.checked_add(1))
                .ok_or("Snapshot row count overflow")?,
            1,
            width,
            crate::theme::ROW_HEIGHT as u32,
        )
    }

    pub(in crate::app) fn new(
        rows: usize,
        columns: usize,
        column_width: u32,
        row_height: u32,
    ) -> Result<Self, String> {
        let width = u32::try_from(columns)
            .ok()
            .and_then(|count| count.checked_mul(column_width))
            .ok_or("Snapshot width overflow")?;
        let height = u32::try_from(rows)
            .ok()
            .and_then(|count| count.checked_mul(row_height))
            .ok_or("Snapshot height overflow")?;
        let pixels = u64::from(width)
            .checked_mul(u64::from(height))
            .ok_or("Snapshot pixel count overflow")?;
        if width == 0 || height == 0 || width > 8192 || height > 8192 || pixels > 16_777_216 {
            return Err("Selected cells exceed the 8192px / 16M-pixel snapshot limit; nothing was truncated".into());
        }
        Ok(Self { width, height })
    }
}

impl super::super::AppRoot {
    pub(in crate::app) fn open_selected_snapshot_preview(
        &mut self,
        tab_id: u64,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let generation = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.query.generation());
        let table = match self.prepare_selected_snapshot(tab_id) {
            Ok(table) => table,
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error);
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let bounds = table.bounds;
        let handle = window.window_handle();
        let root = cx.entity().downgrade();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { table.render_png() })
                .await;
            let png = match result {
                Ok(png) => Arc::new(png),
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report_error(crate::app::Area::Export, None, &error);
                        this.view_mut(tab_id).detail_ui.error = Some(error);
                        this.notify_tab(tab_id, cx);
                    });
                    return;
                }
            };
            let current = this
                .update(cx, |this, _| {
                    this.workspace
                        .runtime(tab_id)
                        .is_some_and(|runtime| Some(runtime.query.generation()) == generation)
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let _ = cx.update_window(handle, move |_, window, cx| {
                let image = Arc::new(Image::from_bytes(ImageFormat::Png, png.as_ref().clone()));
                window.open_alert_dialog(cx, move |dialog, _, _| {
                    let root = root.clone();
                    let png = png.clone();
                    dialog
                        .title("Screenshot Selected Cells")
                        .child(
                            img(image.clone())
                                .debug_selector(|| "snapshot-preview".into())
                                .w(px(bounds.width.min(640) as f32))
                                .h(px(bounds.height.min(420) as f32))
                                .object_fit(ObjectFit::Contain),
                        )
                        .button_props(
                            gpui_component::dialog::DialogButtonProps::default()
                                .ok_text("Save PNG"),
                        )
                        .confirm()
                        .on_ok(move |_, _, app| {
                            let _ = root.update(app, |this, cx| {
                                this.save_rendered_snapshot(png.clone(), tab_id, generation, cx)
                            });
                            true
                        })
                });
            });
        })
        .detach();
    }

    fn save_rendered_snapshot(
        &mut self,
        png: Arc<Vec<u8>>,
        tab_id: u64,
        generation: Option<u64>,
        cx: &mut gpui::Context<Self>,
    ) {
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let prompt = cx.prompt_for_new_path(&directory, Some("selected-cells.png"));
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => return,
            };
            let current = this
                .update(cx, |this, _| {
                    this.workspace
                        .runtime(tab_id)
                        .is_some_and(|runtime| Some(runtime.query.generation()) == generation)
                })
                .unwrap_or(false);
            if !current {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
                    {
                        return Err("Choose a .png destination".into());
                    }
                    write_png_new(&path, &png)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let error = result.err();
                if let Some(error) = &error {
                    this.report_error(crate::app::Area::Export, None, error.clone());
                }
                this.view_mut(tab_id).detail_ui.error = error;
                this.notify_tab(tab_id, cx);
            });
        })
        .detach();
    }

    pub(in crate::app) fn snapshot_selected_bounds(
        &self,
        tab_id: u64,
    ) -> Result<SnapshotBounds, String> {
        let view = self.view(tab_id);
        let range = view
            .grid_ui
            .selection
            .borrow()
            .range
            .ok_or("Select cells to capture")?
            .normalized();
        if range.end.row >= self.grid(tab_id).row_count()
            || range.end.column >= self.display_column_count(tab_id)
        {
            return Err("Selected cells are outside the current page".into());
        }
        SnapshotBounds::from_layout(range, &view.grid_ui.layout)
    }

    pub(in crate::app) fn prepare_selected_snapshot(
        &self,
        tab_id: u64,
    ) -> Result<SnapshotTable, String> {
        self.snapshot_selected_bounds(tab_id)?;
        let view = self.view(tab_id);
        let range = view
            .grid_ui
            .selection
            .borrow()
            .range
            .ok_or("Select cells to capture")?
            .normalized();
        let widths = view
            .grid_ui
            .layout
            .widths
            .get(range.start.column..=range.end.column)
            .ok_or("Selection extends beyond grid columns")?
            .iter()
            .map(|width| width.ceil() as u32)
            .collect::<Vec<_>>();
        let data = self.collect_copy_context_range(tab_id, range)?;
        SnapshotTable::new(&data, &widths)
    }

    #[cfg(test)]
    pub(in crate::app) fn save_selected_snapshot(
        &mut self,
        tab_id: u64,
        cx: &mut gpui::Context<Self>,
    ) {
        let generation = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.query.generation());
        let table = match self.prepare_selected_snapshot(tab_id) {
            Ok(table) => table,
            Err(error) => {
                self.view_mut(tab_id).detail_ui.error = Some(error);
                self.notify_tab(tab_id, cx);
                return;
            }
        };
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let prompt = cx.prompt_for_new_path(&directory, Some("selected-cells.png"));
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.view_mut(tab_id).detail_ui.error =
                            Some("Could not open the snapshot save dialog".into());
                        this.notify_tab(tab_id, cx);
                    });
                    return;
                }
            };
            let current = this
                .update(cx, |this, _| {
                    this.workspace
                        .runtime(tab_id)
                        .is_some_and(|runtime| Some(runtime.query.generation()) == generation)
                })
                .unwrap_or(false);
            if !current {
                let _ = this.update(cx, |this, cx| {
                    this.view_mut(tab_id).detail_ui.error = Some(
                        "Result changed before snapshot save; choose the selection again".into(),
                    );
                    this.notify_tab(tab_id, cx);
                });
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    if !path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
                    {
                        return Err("Choose a .png destination".into());
                    }
                    let png = table.render_png()?;
                    write_png_new(&path, &png)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let error = result.err();
                if let Some(error) = &error {
                    this.report_error(crate::app::Area::Export, None, error.clone());
                }
                this.view_mut(tab_id).detail_ui.error = error;
                this.notify_tab(tab_id, cx);
            });
        })
        .detach();
    }
}
use super::copy::CopyData;
use gpui::{
    img, px, AppContext as _, Image, ImageFormat, InteractiveElement as _, ObjectFit,
    ParentElement as _, Styled as _, StyledImage as _,
};
use gpui_component::WindowExt as _;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::{Arc, OnceLock};

const MAX_SVG_BYTES: usize = 16 * 1024 * 1024;
const MAX_PNG_BYTES: usize = 64 * 1024 * 1024;
const ROW_HEIGHT: u32 = 26;

pub(in crate::app) struct SnapshotTable {
    pub bounds: SnapshotBounds,
    widths: Vec<u32>,
    heights: Vec<u32>,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

fn svg_text(value: &str) -> Result<String, String> {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            ch if ch.is_control() && ch != '\n' && ch != '\t' => {
                return Err("Selected value contains an invalid XML control character".into())
            }
            '\u{fffe}' | '\u{ffff}' => {
                return Err("Selected value contains an invalid XML character".into())
            }
            _ => escaped.push(ch),
        }
    }
    Ok(escaped)
}

impl SnapshotTable {
    pub(in crate::app) fn new(data: &CopyData, source_widths: &[u32]) -> Result<Self, String> {
        if data.columns.is_empty()
            || data.rows.is_empty()
            || data.columns.len() != source_widths.len()
            || data.rows.iter().any(|row| row.len() != data.columns.len())
        {
            return Err("Snapshot requires a nonempty rectangular selection".into());
        }
        let mut widths = Vec::with_capacity(data.columns.len());
        let mut input_budget = 0usize;
        for (index, name) in data.columns.iter().enumerate() {
            let longest = std::iter::once(name.as_str())
                .chain(data.rows.iter().map(|row| row[index].display()))
                .flat_map(|value| value.split('\n'))
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(0);
            let needed = u32::try_from(longest)
                .ok()
                .and_then(|count| count.checked_mul(16))
                .and_then(|width| width.checked_add(16))
                .ok_or("Snapshot column width overflow")?;
            widths.push(source_widths[index].max(needed).max(60));
            input_budget = input_budget
                .checked_add(
                    name.len()
                        .checked_mul(6)
                        .ok_or("Snapshot text size overflow")?,
                )
                .ok_or("Snapshot text size overflow")?;
        }
        let mut heights = Vec::with_capacity(data.rows.len());
        for row in &data.rows {
            let lines = row
                .iter()
                .map(|cell| cell.display().split('\n').count())
                .max()
                .unwrap_or(1);
            heights.push(
                u32::try_from(lines)
                    .ok()
                    .and_then(|count| count.checked_mul(18))
                    .and_then(|height| height.checked_add(8))
                    .ok_or("Snapshot row height overflow")?
                    .max(ROW_HEIGHT),
            );
            for cell in row {
                input_budget = input_budget
                    .checked_add(
                        cell.display()
                            .len()
                            .checked_mul(6)
                            .ok_or("Snapshot text size overflow")?,
                    )
                    .ok_or("Snapshot text size overflow")?;
            }
        }
        let cells = data
            .rows
            .len()
            .checked_mul(data.columns.len())
            .ok_or("Snapshot cell count overflow")?;
        let text_nodes = data
            .rows
            .iter()
            .flat_map(|row| row.iter())
            .try_fold(data.columns.len(), |total, cell| {
                total.checked_add(cell.display().split('\n').count())
            })
            .ok_or("Snapshot text node count overflow")?;
        let svg_budget = input_budget
            .checked_add(cells.checked_mul(120).ok_or("Snapshot SVG size overflow")?)
            .and_then(|total| total.checked_add(text_nodes.checked_mul(160)?))
            .and_then(|total| total.checked_add(4096))
            .ok_or("Snapshot SVG size overflow")?;
        if svg_budget > MAX_SVG_BYTES {
            return Err(
                "Selected text exceeds the 16 MiB SVG budget; nothing was truncated".into(),
            );
        }
        let width = widths.iter().try_fold(0u32, |total, width| {
            total.checked_add(*width).ok_or("Snapshot width overflow")
        })?;
        let height = heights.iter().try_fold(ROW_HEIGHT, |total, height| {
            total.checked_add(*height).ok_or("Snapshot height overflow")
        })?;
        let bounds = SnapshotBounds::new(1, 1, width, height)?;
        Ok(Self {
            bounds,
            widths,
            heights,
            columns: data.columns.clone(),
            rows: data
                .rows
                .iter()
                .map(|row| row.iter().map(|cell| cell.display().to_string()).collect())
                .collect(),
        })
    }

    pub(in crate::app) fn to_svg(&self) -> Result<String, String> {
        let mut svg = String::new();
        write!(svg, "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/>",
            self.bounds.width, self.bounds.height, self.bounds.width, self.bounds.height).map_err(|error| error.to_string())?;
        let mut x = 0u32;
        for (name, width) in self.columns.iter().zip(&self.widths) {
            write!(svg, "<rect x=\"{x}\" y=\"0\" width=\"{width}\" height=\"26\" fill=\"#e8ebef\" stroke=\"#cbd2dc\"/>")
                .map_err(|error| error.to_string())?;
            write!(svg, "<text x=\"{}\" y=\"18\" font-family=\"Arial, sans-serif\" font-size=\"12\" fill=\"#17202a\">{}</text>", x + 8, svg_text(name)?)
                .map_err(|error| error.to_string())?;
            x += width;
        }
        let mut y = ROW_HEIGHT;
        for (row, height) in self.rows.iter().zip(&self.heights) {
            x = 0;
            for (value, width) in row.iter().zip(&self.widths) {
                write!(svg, "<rect x=\"{x}\" y=\"{y}\" width=\"{width}\" height=\"{height}\" fill=\"#fff\" stroke=\"#cbd2dc\"/>")
                    .map_err(|error| error.to_string())?;
                for (line, part) in value.split('\n').enumerate() {
                    let baseline =
                        y + 18 + u32::try_from(line).map_err(|_| "Snapshot line overflow")? * 18;
                    write!(svg, "<text x=\"{}\" y=\"{baseline}\" font-family=\"Arial, sans-serif\" font-size=\"12\" fill=\"#17202a\">{}</text>", x + 8, svg_text(part)?)
                        .map_err(|error| error.to_string())?;
                }
                x += width;
            }
            y += height;
        }
        svg.push_str("</svg>");
        if svg.len() > MAX_SVG_BYTES {
            return Err("Snapshot SVG exceeds the 16 MiB budget".into());
        }
        Ok(svg)
    }

    pub(in crate::app) fn render_png(&self) -> Result<Vec<u8>, String> {
        use resvg::usvg::fontdb;
        // `Arial, sans-serif` only resolves when the machine has Arial or the font fontconfig names for
        // `sans-serif`; a Linux box with other fonts installed (Liberation, DejaVu, ...) would match
        // nothing and drop every glyph. `fallback` is the plain system face the resolver uses then.
        static FONT_DB: OnceLock<(Arc<fontdb::Database>, Option<fontdb::ID>)> = OnceLock::new();
        let (fontdb, fallback) = FONT_DB.get_or_init(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            let fallback = db
                .faces()
                .min_by_key(|face| {
                    (
                        face.style != fontdb::Style::Normal,
                        face.monospaced,
                        face.weight.0.abs_diff(400),
                    )
                })
                .map(|face| face.id);
            (Arc::new(db), fallback)
        });
        let fallback = *fallback;
        let default_select = resvg::usvg::FontResolver::default_font_selector();
        let options = resvg::usvg::Options {
            font_family: "Arial".into(),
            fontdb: fontdb.clone(),
            font_resolver: resvg::usvg::FontResolver {
                select_font: Box::new(move |font, db| default_select(font, db).or(fallback)),
                ..Default::default()
            },
            ..Default::default()
        };
        let svg = self.to_svg()?;
        let tree =
            resvg::usvg::Tree::from_str(&svg, &options).map_err(|error| error.to_string())?;
        let mut pixmap = resvg::tiny_skia::Pixmap::new(self.bounds.width, self.bounds.height)
            .ok_or("Could not allocate bounded snapshot pixmap")?;
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        let png = pixmap.encode_png().map_err(|error| error.to_string())?;
        if png.len() > MAX_PNG_BYTES {
            return Err("Snapshot PNG exceeds 64 MiB".into());
        }
        Ok(png)
    }
}

pub(in crate::app) fn write_png_new(path: &std::path::Path, png: &[u8]) -> Result<(), String> {
    if png.len() > MAX_PNG_BYTES {
        return Err("Snapshot PNG exceeds 64 MiB".into());
    }
    let temp = path.with_extension(format!("png.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(png)?;
        file.sync_all()?;
        std::fs::hard_link(&temp, path)?;
        Ok(())
    })();
    let cleanup = std::fs::remove_file(&temp);
    result.map_err(|error| error.to_string())?;
    cleanup.map_err(|error| error.to_string())?;
    Ok(())
}
