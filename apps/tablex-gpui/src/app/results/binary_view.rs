//! The value panel for a binary cell (BLOB, bytea, BINARY): a Hex dump, the
//! bytes read as Text (UTF-8 or Latin-1) and, for a PNG / JPEG / GIF / WebP /
//! BMP, the Image (fit or 100%). The bytes are read from the page while the
//! panel draws; the hex dump is a virtualized list of dump rows, so a value of
//! megabytes costs the same as one of a few bytes. Save writes the bytes to a
//! file; Load stages a file as the cell's new value (editable cells only).

use super::super::*;
use crate::binary_value::{self, HexForm, ImageKind, TextEncoding};
use crate::cell_detail::BinaryDetail;
use gpui::StyledImage as _;
use gpui_component::button::{ButtonCustomVariant, ButtonGroup};
use std::path::{Path, PathBuf};

const HEX_ROW_HEIGHT: f32 = 16.0;

/// The view of a binary value the panel is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum BinaryMode {
    Hex,
    Text,
    Image,
}

/// How an image is scaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum ImageZoom {
    /// Shrunk to fit the panel.
    #[default]
    Fit,
    /// Actual pixels, scrollable.
    Actual,
}

/// Per-tab state of the binary view (see `DetailUiState::binary`).
pub(in crate::app) struct BinaryViewState {
    /// The view the user picked, for one cell: any other cell opens on its own
    /// default (the image of a picture, else the hex dump).
    pub(in crate::app) chosen: Option<(CellPosition, BinaryMode)>,
    pub(in crate::app) encoding: TextEncoding,
    pub(in crate::app) zoom: ImageZoom,
    pub(in crate::app) hex_scroll: UniformListScrollHandle,
    /// The Text view's decoded text, kept while the cell, encoding and bytes
    /// stay the same (the panel redraws on every selection and scroll).
    text: std::cell::RefCell<Option<DecodedText>>,
}

/// One decode of the Text view, and what it was made from.
struct DecodedText {
    position: CellPosition,
    encoding: TextEncoding,
    len: usize,
    /// Address of the bytes: a staged edit at the same cell and size is another buffer.
    address: usize,
    text: gpui::SharedString,
    truncated: bool,
}

impl Default for BinaryViewState {
    fn default() -> Self {
        Self {
            chosen: None,
            encoding: TextEncoding::default(),
            zoom: ImageZoom::default(),
            hex_scroll: UniformListScrollHandle::new(),
            text: std::cell::RefCell::new(None),
        }
    }
}

impl BinaryViewState {
    /// The first characters of `bytes` as text in the chosen encoding, decoded
    /// again only when the cell, encoding or bytes changed.
    fn decoded(&self, position: CellPosition, bytes: &[u8]) -> (gpui::SharedString, bool) {
        let mut cache = self.text.borrow_mut();
        let current = cache.as_ref().is_some_and(|cached| {
            cached.position == position
                && cached.encoding == self.encoding
                && cached.len == bytes.len()
                && cached.address == bytes.as_ptr() as usize
        });
        if !current {
            let (text, truncated) = binary_value::decode_text(bytes, self.encoding);
            *cache = Some(DecodedText {
                position,
                encoding: self.encoding,
                len: bytes.len(),
                address: bytes.as_ptr() as usize,
                text: text.into(),
                truncated,
            });
        }
        cache
            .as_ref()
            .map(|cached| (cached.text.clone(), cached.truncated))
            .unwrap_or_default()
    }
}

/// Where Save and Copy read a value's bytes from: owned ones, or the page
/// itself (a cheap handle on it, so a large value is never copied up front).
enum ByteSource {
    Owned(Arc<[u8]>),
    Page {
        grid: GridState<GridContentSource>,
        /// Display row, source column.
        position: CellPosition,
        /// The cell holds PostgreSQL `bytea` text, decoded when read.
        pg_text: bool,
    },
}

impl ByteSource {
    /// `read` on the bytes (decoding a PostgreSQL text value first).
    fn with_bytes<R>(&self, read: impl FnOnce(&[u8]) -> R) -> Option<R> {
        match self {
            Self::Owned(bytes) => Some(read(bytes)),
            Self::Page { grid, position, pg_text: false } => Some(read(grid.cell_bytes(*position)?)),
            Self::Page { grid, position, pg_text: true } => {
                let bytes = binary_value::from_hex_form(grid.cell_str(*position)?, HexForm::PgBytea)?;
                Some(read(&bytes))
            }
        }
    }
}

/// `bytes` written to `path` through a temporary file beside it that is then
/// renamed over it: a failed or interrupted write never leaves half a file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), BinaryFileError> {
    let write_error = |source| BinaryFileError::Write {
        path: path.to_path_buf(),
        source,
    };
    let name = path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = std::fs::write(&temporary, bytes).and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(write_error)
}

/// Why a binary value could not be saved to or loaded from a file.
#[derive(Debug)]
pub(in crate::app) enum BinaryFileError {
    /// The file is larger than the value cap ([`binary_value::PREVIEW_CAP_BYTES`]).
    TooLarge { path: PathBuf },
    Read { path: PathBuf, source: std::io::Error },
    Write { path: PathBuf, source: std::io::Error },
    /// The platform could not show the file dialog.
    Dialog,
    /// The value is no longer on the page (the result changed).
    Unreadable,
}

impl std::fmt::Display for BinaryFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { path } => write!(
                f,
                "{} is larger than {}: a binary cell takes a file of at most that size.",
                path.display(),
                binary_value::format_size(binary_value::PREVIEW_CAP_BYTES)
            ),
            Self::Read { path, source } => write!(f, "Could not read {}: {source}", path.display()),
            Self::Write { path, source } => write!(f, "Could not write {}: {source}", path.display()),
            Self::Dialog => f.write_str("Could not open the file dialog"),
            Self::Unreadable => f.write_str("The value is no longer on the page; select the cell again."),
        }
    }
}

impl std::error::Error for BinaryFileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// The file at `path`, at most [`binary_value::PREVIEW_CAP_BYTES`] bytes.
fn read_capped(path: &Path) -> Result<Vec<u8>, BinaryFileError> {
    use std::io::Read as _;
    let read_error = |source| BinaryFileError::Read {
        path: path.to_path_buf(),
        source,
    };
    let cap = binary_value::PREVIEW_CAP_BYTES as u64;
    let mut data = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(cap + 1).read_to_end(&mut data))
        .map_err(read_error)?;
    if data.len() as u64 > cap {
        return Err(BinaryFileError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    Ok(data)
}

/// File name offered when saving: the column, with the extension of an image.
fn suggested_file_name(column: &str, image: Option<ImageKind>) -> String {
    let stem: String = column
        .chars()
        .map(|ch| if ch.is_alphanumeric() || matches!(ch, '-' | '_') { ch } else { '_' })
        .collect();
    let stem = if stem.is_empty() { "value".to_string() } else { stem };
    let extension = match image {
        Some(ImageKind::Png) => "png",
        Some(ImageKind::Jpeg) => "jpg",
        Some(ImageKind::Gif) => "gif",
        Some(ImageKind::Webp) => "webp",
        Some(ImageKind::Bmp) => "bmp",
        None => "bin",
    };
    format!("{stem}.{extension}")
}

impl AppRoot {
    /// The bytes of the selected cell's binary value: a staged edit's, else the page's.
    pub(in crate::app) fn cell_detail_bytes<'a>(
        &'a self,
        tab_id: u64,
        detail: &'a CellDetail,
    ) -> Option<&'a [u8]> {
        let binary = detail.binary.as_ref()?;
        match &binary.owned {
            Some(bytes) => Some(bytes),
            None => self.grid(tab_id).cell_bytes(CellPosition {
                row: detail.position.row,
                column: detail.source_column,
            }),
        }
    }

    /// The view the panel is on for the selected binary cell.
    pub(in crate::app) fn binary_mode(
        &self,
        tab_id: u64,
        detail: &CellDetail,
        binary: &BinaryDetail,
    ) -> BinaryMode {
        let chosen = self
            .view(tab_id)
            .detail_ui
            .binary
            .chosen
            .filter(|(position, _)| *position == detail.position)
            .map(|(_, mode)| mode);
        match chosen {
            Some(BinaryMode::Image) if binary.picture.is_none() => BinaryMode::Hex,
            Some(mode) => mode,
            None if binary.picture.is_some() => BinaryMode::Image,
            None => BinaryMode::Hex,
        }
    }

    pub(in crate::app) fn set_binary_mode(
        &mut self,
        tab_id: u64,
        position: CellPosition,
        mode: BinaryMode,
        cx: &mut Context<Self>,
    ) {
        self.view_mut(tab_id).detail_ui.binary.chosen = Some((position, mode));
        self.notify_tab(tab_id, cx);
    }

    /// The Hex / Text / Image switch of the value header.
    pub(in crate::app) fn render_binary_mode_switch(
        &self,
        tab_id: u64,
        detail: &CellDetail,
        binary: &BinaryDetail,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = self.binary_mode(tab_id, detail, binary);
        let mut options = vec![(BinaryMode::Hex, "Hex"), (BinaryMode::Text, "Text")];
        if binary.picture.is_some() {
            options.push((BinaryMode::Image, "Image"));
        }
        let position = detail.position;
        let root = cx.entity().downgrade();
        ButtonGroup::new(("cell-detail-binary-mode", tab_id))
            .outline()
            .compact()
            .with_size(Size::XSmall)
            .children(options.iter().enumerate().map(|(index, (option, label))| {
                Button::new(("cell-detail-binary-mode-option", index))
                    .debug_selector(move || format!("cell-detail-binary-mode-{}", label.to_lowercase()))
                    .label(*label)
                    .selected(mode == *option)
            }))
            .on_click(move |selected, _, app| {
                if let Some((option, _)) = selected.first().and_then(|index| options.get(*index)) {
                    let option = *option;
                    let _ = root.update(app, |this, cx| {
                        this.set_binary_mode(tab_id, position, option, cx)
                    });
                }
            })
            .into_any_element()
    }

    /// The row under the value box: what the value is, and the encoding (Text)
    /// or zoom (Image) switch.
    pub(in crate::app) fn render_binary_subbar(
        &self,
        tab_id: u64,
        detail: &CellDetail,
        binary: &BinaryDetail,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mode = self.binary_mode(tab_id, detail, binary);
        let mut facts = Vec::new();
        if let Some(kind) = binary.image {
            facts.push(kind.label().to_string());
        }
        if let Some((width, height)) = binary.dimensions {
            facts.push(format!("{width} × {height}"));
        }
        facts.push(binary_value::format_size(binary.len));
        let state = &self.view(tab_id).detail_ui.binary;
        let root = cx.entity().downgrade();
        let switch = match mode {
            BinaryMode::Text => {
                let current = state.encoding;
                let options = [TextEncoding::Utf8, TextEncoding::Latin1];
                Some(
                    ButtonGroup::new(("cell-detail-binary-encoding", tab_id))
                        .outline()
                        .compact()
                        .with_size(Size::XSmall)
                        .children(options.iter().enumerate().map(|(index, option)| {
                            Button::new(("cell-detail-binary-encoding-option", index))
                                .debug_selector(move || {
                                    format!("cell-detail-binary-encoding-{}", option.label().to_lowercase().replace('-', ""))
                                })
                                .label(option.label())
                                .selected(current == *option)
                        }))
                        .on_click(move |selected, _, app| {
                            if let Some(option) = selected.first().and_then(|index| options.get(*index)) {
                                let option = *option;
                                let _ = root.update(app, |this, cx| {
                                    this.view_mut(tab_id).detail_ui.binary.encoding = option;
                                    this.notify_tab(tab_id, cx);
                                });
                            }
                        })
                        .into_any_element(),
                )
            }
            BinaryMode::Image => {
                let current = state.zoom;
                let options = [(ImageZoom::Fit, "Fit"), (ImageZoom::Actual, "100%")];
                Some(
                    ButtonGroup::new(("cell-detail-binary-zoom", tab_id))
                        .outline()
                        .compact()
                        .with_size(Size::XSmall)
                        .children(options.iter().enumerate().map(|(index, (option, label))| {
                            Button::new(("cell-detail-binary-zoom-option", index))
                                .debug_selector(move || format!("cell-detail-binary-zoom-{}", if *option == ImageZoom::Fit { "fit" } else { "actual" }))
                                .label(*label)
                                .selected(current == *option)
                        }))
                        .on_click(move |selected, _, app| {
                            if let Some((option, _)) = selected.first().and_then(|index| options.get(*index)) {
                                let option = *option;
                                let _ = root.update(app, |this, cx| {
                                    this.view_mut(tab_id).detail_ui.binary.zoom = option;
                                    this.notify_tab(tab_id, cx);
                                });
                            }
                        })
                        .into_any_element(),
                )
            }
            BinaryMode::Hex => None,
        };
        div()
            .debug_selector(|| "cell-detail-binary-subbar".into())
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_h(px(22.0))
            .child(
                div()
                    .debug_selector(|| "cell-detail-binary-info".into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.0))
                    .text_color(theme::text_muted())
                    .child(facts.join(" · ")),
            )
            .children(switch.map(|switch| {
                div()
                    .debug_selector(|| "cell-detail-binary-switch".into())
                    .flex_shrink_0()
                    .child(switch)
            }))
            .into_any_element()
    }

    /// The Value box of a binary cell.
    pub(in crate::app) fn render_binary_value(
        &self,
        tab_id: u64,
        detail: &CellDetail,
        binary: &BinaryDetail,
        panel_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !binary.previewable() {
            return div()
                .debug_selector(|| "cell-detail-binary-too-large".into())
                .size_full()
                .p(px(6.0))
                .font_family("Inter")
                .text_size(px(11.0))
                .text_color(theme::text_muted())
                .whitespace_normal()
                .child(format!(
                    "This value is {}: too large to preview (the limit is {}). Save it to a file to inspect it.",
                    binary_value::format_size(binary.len),
                    binary_value::format_size(binary_value::PREVIEW_CAP_BYTES)
                ))
                .into_any_element();
        }
        match self.binary_mode(tab_id, detail, binary) {
            BinaryMode::Hex => self.render_binary_hex(tab_id, binary.len, panel_width, cx),
            BinaryMode::Text => {
                let bytes = self.cell_detail_bytes(tab_id, detail).unwrap_or_default();
                let (text, truncated) = self
                    .view(tab_id)
                    .detail_ui
                    .binary
                    .decoded(detail.position, bytes);
                div()
                    .debug_selector(|| "cell-detail-binary-text".into())
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        div()
                            .p(px(4.0))
                            .whitespace_normal()
                            .text_color(theme::text())
                            .child(text),
                    )
                    .when(truncated, |this| {
                        this.child(
                            div()
                                .p(px(4.0))
                                .font_family("Inter")
                                .text_size(px(10.0))
                                .text_color(theme::text_muted())
                                .child("Only the start of the value is shown as text."),
                        )
                    })
                    .into_any_element()
            }
            BinaryMode::Image => {
                let Some(picture) = binary.picture.clone() else {
                    return div().into_any_element();
                };
                match (self.view(tab_id).detail_ui.binary.zoom, binary.dimensions) {
                    (ImageZoom::Actual, Some((width, height))) => div()
                        .id(("cell-detail-binary-image-scroll", tab_id))
                        .debug_selector(|| "cell-detail-binary-image-actual".into())
                        .size_full()
                        .overflow_scroll()
                        .child(
                            gpui::img(picture)
                                .debug_selector(|| "cell-detail-binary-image".into())
                                .w(px(width as f32))
                                .h(px(height as f32))
                                .flex_shrink_0()
                                .with_fallback(undecodable_image),
                        )
                        .into_any_element(),
                    _ => div()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            gpui::img(picture)
                                .debug_selector(|| "cell-detail-binary-image".into())
                                .size_full()
                                .object_fit(gpui::ObjectFit::Contain)
                                .with_fallback(undecodable_image),
                        )
                        .into_any_element(),
                }
            }
        }
    }

    /// Virtualized hex dump: only the visible dump rows are formatted.
    fn render_binary_hex(
        &self,
        tab_id: u64,
        len: usize,
        panel_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let per_row = binary_value::row_bytes_for_width(panel_width);
        let scroll = self.view(tab_id).detail_ui.binary.hex_scroll.clone();
        let rows = uniform_list(
            ("cell-detail-binary-hex-list", tab_id),
            binary_value::hex_row_count(len, per_row),
            cx.processor(move |this, range: Range<usize>, _window, _cx| {
                let Some(detail) = this.selected_cell_detail(tab_id) else {
                    return Vec::new();
                };
                let bytes = this.cell_detail_bytes(tab_id, detail).unwrap_or_default();
                range
                    .filter_map(|index| {
                        binary_value::hex_row(bytes, index, per_row).map(|row| (index, row))
                    })
                    .map(|(index, row)| {
                        div()
                            .debug_selector(move || format!("cell-detail-binary-hex-row-{index}"))
                            .h(px(HEX_ROW_HEIGHT))
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .whitespace_nowrap()
                            .child(div().text_color(theme::text_faint()).child(row.offset))
                            .child(div().text_color(theme::text()).child(row.hex))
                            .child(div().text_color(theme::text_muted()).child(row.ascii))
                    })
                    .collect()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&scroll);
        div()
            .debug_selector(|| "cell-detail-binary-hex".into())
            .relative()
            .size_full()
            .flex()
            .text_size(px(10.0))
            .child(rows)
            .child(Scrollbar::vertical(&scroll))
            .into_any_element()
    }

    /// Where the selected cell's bytes are read from for Save and Copy.
    fn detail_byte_source(&self, tab_id: u64, detail: &CellDetail) -> Option<ByteSource> {
        let binary = detail.binary.as_ref()?;
        Some(match &binary.owned {
            Some(bytes) => ByteSource::Owned(Arc::clone(bytes)),
            None => ByteSource::Page {
                grid: self.grid(tab_id).clone(),
                position: CellPosition {
                    row: detail.position.row,
                    column: detail.source_column,
                },
                pg_text: binary.form == HexForm::PgBytea,
            },
        })
    }

    /// Copies the selected binary value as hex (`0x…`, or PostgreSQL's `\x…`):
    /// every byte, not the part the panel previews.
    pub(in crate::app) fn copy_cell_detail_binary(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(detail) = self.selected_cell_detail(tab_id) else {
            return;
        };
        let form = detail.binary.as_ref().map(|binary| binary.form).unwrap_or_default();
        let text = self
            .detail_byte_source(tab_id, detail)
            .and_then(|source| source.with_bytes(|bytes| binary_value::to_hex_form(bytes, form)));
        if let Some(text) = text {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
    }

    /// Save to file: the bytes of the selected value, as the user named the file.
    pub(in crate::app) fn save_cell_binary(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(detail) = self.selected_cell_detail(tab_id) else {
            return;
        };
        let Some(source) = self.detail_byte_source(tab_id, detail) else {
            return;
        };
        let name = suggested_file_name(
            &detail.column_name,
            detail.binary.as_ref().and_then(|binary| binary.image),
        );
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(std::env::temp_dir);
        let prompt = cx.prompt_for_new_path(&directory, Some(&name));
        cx.spawn(async move |this, cx| {
            let outcome = match prompt.await {
                Ok(Ok(Some(path))) => {
                    let write = cx
                        .background_executor()
                        .spawn(async move {
                            smol::unblock(move || {
                                source
                                    .with_bytes(|bytes| write_atomic(&path, bytes))
                                    .unwrap_or(Err(BinaryFileError::Unreadable))
                            })
                            .await
                        })
                        .await;
                    write.err()
                }
                Ok(Ok(None)) => return,
                _ => Some(BinaryFileError::Dialog),
            };
            let _ = this.update(cx, |this, cx| {
                this.view_mut(tab_id).detail_ui.error = outcome.map(|error| error.to_string());
                this.notify_tab(tab_id, cx);
            });
        })
        .detach();
    }

    /// Load from file: the chosen file becomes the cell's staged value.
    pub(in crate::app) fn load_cell_binary(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(detail) = self.selected_cell_detail(tab_id).cloned() else {
            return;
        };
        if !detail.editable || detail.binary_form.is_none() || self.refuse_read_only_edit(tab_id, cx) {
            return;
        }
        let generation = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.query.generation());
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Load binary value from file".into()),
        });
        cx.spawn(async move |this, cx| {
            let path = match prompt.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.view_mut(tab_id).detail_ui.error =
                            Some(BinaryFileError::Dialog.to_string());
                        this.notify_tab(tab_id, cx);
                    });
                    return;
                }
            };
            let Some(path) = path else { return };
            let read = cx
                .background_executor()
                .spawn(async move { smol::unblock(move || read_capped(&path)).await })
                .await;
            let _ = this.update(cx, |this, cx| {
                let outcome = read
                    .map_err(|error| error.to_string())
                    .and_then(|bytes| this.stage_loaded_binary(tab_id, generation, &detail, &bytes));
                match outcome {
                    Ok(()) => this.refresh_cell_detail(tab_id),
                    Err(message) => this.view_mut(tab_id).detail_ui.error = Some(message),
                }
                this.notify_tab(tab_id, cx);
            });
        })
        .detach();
    }

    /// Stages `bytes` as the new value of the cell `detail` describes, as an
    /// edit typed as `0x…` would be (Apply writes it, Discard drops it).
    fn stage_loaded_binary(
        &mut self,
        tab_id: u64,
        generation: Option<u64>,
        detail: &CellDetail,
        bytes: &[u8],
    ) -> Result<(), String> {
        let current = self
            .workspace
            .runtime(tab_id)
            .map(|runtime| runtime.query.generation());
        if current != generation {
            return Err("The result changed while the file was read; select the cell again.".into());
        }
        let tab = self
            .workspace
            .tab(tab_id)
            .cloned()
            .ok_or_else(|| "The tab is closed.".to_string())?;
        // The columns may have been re-ordered (a pin) while the file was read:
        // the cell is found again by its source column, never by the old display one.
        let column = {
            let visible = &self.view(tab_id).grid_ui.visible_columns;
            if visible.is_empty() {
                (detail.source_column < self.grid(tab_id).column_count()).then_some(detail.source_column)
            } else {
                visible.iter().position(|source| *source == detail.source_column)
            }
        }
        .ok_or_else(|| "The column is no longer shown; select the cell again.".to_string())?;
        let position = CellPosition {
            row: detail.position.row,
            column,
        };
        let payload = self
            .cell_detail_payload(&tab, detail)
            .ok_or_else(|| "This cell cannot be edited.".to_string())?;
        let original = self
            .grid(tab_id)
            .full_cell_text(CellPosition {
                row: detail.position.row,
                column: detail.source_column,
            })
            .unwrap_or_default();
        self.try_stage_sql_edit(
            tab_id,
            position,
            &payload,
            &original,
            &binary_value::to_hex_form(bytes, detail.binary_form.unwrap_or_default()),
        )
        .map_err(|error| error.to_string())
    }
}

/// Shown where an image GPUI cannot decode would be (the bytes start like a
/// picture but are not one).
fn undecodable_image() -> AnyElement {
    div()
        .debug_selector(|| "cell-detail-binary-image-undecodable".into())
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p(px(8.0))
        .font_family("Inter")
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child("These bytes start like an image but cannot be decoded. Switch to Hex.")
        .into_any_element()
}

/// The chrome-colored icon button of the value header.
pub(in crate::app) fn binary_icon_button(
    id: &'static str,
    tab_id: u64,
    path: &'static str,
    tooltip: &'static str,
    variant: ButtonCustomVariant,
) -> Button {
    Button::new((id, tab_id))
        .icon(Icon::empty().path(path).size(px(13.0)))
        .with_size(Size::XSmall)
        .compact()
        .custom(variant)
        .rounded(px(4.0))
        .tooltip(tooltip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_files_are_named_after_the_column_and_the_image_type() {
        assert_eq!(suggested_file_name("avatar", Some(ImageKind::Png)), "avatar.png");
        assert_eq!(suggested_file_name("a b/c", Some(ImageKind::Jpeg)), "a_b_c.jpg");
        assert_eq!(suggested_file_name("payload", None), "payload.bin");
        assert_eq!(suggested_file_name("", None), "value.bin");
    }

    #[test]
    fn the_text_view_decodes_once_per_cell_encoding_and_bytes() {
        let state = BinaryViewState::default();
        let cell = CellPosition { row: 0, column: 1 };
        let bytes = b"hello, a text long enough to live on the heap and not inline in the string".to_vec();
        let (first, _) = state.decoded(cell, &bytes);
        let (again, _) = state.decoded(cell, &bytes);
        assert_eq!(first.as_ptr(), again.as_ptr(), "the same text is handed out, not decoded again");
        let other = bytes.clone();
        let (rebuilt, _) = state.decoded(cell, &other);
        assert_ne!(first.as_ptr(), rebuilt.as_ptr(), "other bytes at the same cell are decoded");
        let (moved, _) = state.decoded(CellPosition { row: 1, column: 1 }, &other);
        assert_ne!(rebuilt.as_ptr(), moved.as_ptr(), "another cell");
    }

    #[test]
    fn a_saved_file_is_replaced_whole_through_a_temporary_beside_it() {
        let dir = std::env::temp_dir().join(format!("tablex-atomic-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("value.bin");
        std::fs::write(&target, b"old contents that are longer").unwrap();
        write_atomic(&target, &[1, 2, 3]).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), [1, 2, 3]);
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(names, ["value.bin"], "no temporary is left behind");
        // A destination that cannot be written fails with its path and leaves nothing.
        let missing = dir.join("no-such-dir").join("value.bin");
        let error = write_atomic(&missing, &[1]).unwrap_err();
        assert!(error.to_string().contains("no-such-dir"), "{error}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_over_the_cap_is_refused_and_a_missing_one_names_its_path() {
        let dir = std::env::temp_dir().join(format!("tablex-binary-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.bin");
        std::fs::write(&small, [1, 2, 3]).unwrap();
        assert_eq!(read_capped(&small).unwrap(), vec![1, 2, 3]);
        let big = dir.join("big.bin");
        std::fs::write(&big, vec![0u8; binary_value::PREVIEW_CAP_BYTES + 1]).unwrap();
        assert!(matches!(read_capped(&big), Err(BinaryFileError::TooLarge { .. })));
        let missing = dir.join("missing.bin");
        let error = read_capped(&missing).unwrap_err();
        assert!(error.to_string().contains("missing.bin"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
