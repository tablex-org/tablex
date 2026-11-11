//! Import / Export menu importers for DBX, Navicat, DBeaver and DataGrip:
//! pick files, parse off the UI thread, let the user choose which connections
//! to keep, then save them. Nothing is saved before the choice is confirmed;
//! cancelling at any step leaves the stores untouched.

use super::*;
use crate::connection_import::{
    dbx, store::Batch, ImportError, ImportPreview, ImportSource, ImportSummary,
    ImportedConnection, Loaded,
};
use gpui_component::dialog::DialogFooter;
use gpui_component::scroll::ScrollableMask;
use std::sync::Arc;

/// The selection dialog's state: which of the found connections to import.
pub(super) struct ImportSelection {
    source: ImportSource,
    connections: Vec<ImportedConnection>,
    checked: Vec<bool>,
    unsupported: Vec<String>,
    note: Option<&'static str>,
}

impl ImportSelection {
    fn new(preview: ImportPreview) -> Self {
        let unsupported = preview.unsupported_labels();
        let note = preview.note();
        Self {
            source: preview.source,
            checked: preview
                .connections
                .iter()
                .map(|connection| connection.kind.supported().is_some())
                .collect(),
            connections: preview.connections,
            unsupported,
            note,
        }
    }

    /// Each connection's tunnel as the list shows it (tests).
    #[cfg(test)]
    pub(super) fn tunnel_labels(&self) -> Vec<Option<String>> {
        self.connections.iter().map(ImportedConnection::tunnel_label).collect()
    }

    /// Importable connections that come with an SSH tunnel.
    pub(super) fn tunnel_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|connection| connection.kind.supported().is_some() && connection.has_route())
            .count()
    }

    pub(super) fn selected_count(&self) -> usize {
        self.checked.iter().filter(|checked| **checked).count()
    }

    pub(super) fn supported_count(&self) -> usize {
        self.connections
            .iter()
            .filter(|connection| connection.kind.supported().is_some())
            .count()
    }

    /// Unsupported rows stay unchecked.
    fn set(&mut self, index: usize, checked: bool) {
        if self.connections[index].kind.supported().is_some() {
            self.checked[index] = checked;
        }
    }

    fn set_all(&mut self, checked: bool) {
        for index in 0..self.connections.len() {
            self.set(index, checked);
        }
    }

    #[cfg(test)]
    pub(super) fn names(&self) -> Vec<&str> {
        self.connections.iter().map(|connection| connection.name.as_str()).collect()
    }

    #[cfg(test)]
    pub(super) fn is_checked(&self, index: usize) -> bool {
        self.checked[index]
    }

    fn selected(&self) -> Vec<ImportedConnection> {
        self.connections
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|(connection, _)| connection.clone())
            .collect()
    }
}

const ROW_HEIGHT: f32 = 44.0;

impl AppRoot {
    pub(super) fn import_foreign_connections(
        &mut self,
        source: ImportSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: source.picks_folders(),
            multiple: source.picks_folders(),
            prompt: Some(source.picker_prompt().into()),
        });
        self.sidebar.notice = Some(source.picker_hint());
        cx.notify();
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let paths = match prompt.await {
                Ok(Ok(Some(paths))) => paths,
                other => {
                    let picker_failed = !matches!(other, Ok(Ok(None)));
                    let _ = this.update(cx, |this, cx| {
                        this.sidebar.notice =
                            picker_failed.then(|| "Could not open the file picker".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || crate::connection_import::load(source, &paths)).await
                })
                .await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    this.finish_import_load(source, loaded, window, cx)
                });
            });
        })
        .detach();
    }

    fn import_failed(&mut self, source: ImportSource, error: &ImportError, cx: &mut Context<Self>) {
        let message = format!("{} import failed: {error}", source.title());
        self.report_error(Area::Connect, None, &message);
        self.sidebar.notice = Some(message);
        cx.notify();
    }

    pub(super) fn finish_import_load(
        &mut self,
        source: ImportSource,
        loaded: Result<Loaded, ImportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match loaded {
            Ok(Loaded::Ready(preview)) if preview.connections.is_empty() => {
                self.sidebar.notice =
                    Some(format!("No connections found in the selected {} files", source.title()));
                cx.notify();
            }
            Ok(Loaded::Ready(preview)) => self.open_import_selection(preview, window, cx),
            Ok(Loaded::NeedsPassphrase(file)) => {
                self.open_import_passphrase_dialog(Arc::new(file), false, window, cx)
            }
            Err(error) => self.import_failed(source, &error, cx),
        }
    }

    fn open_import_passphrase_dialog(
        &mut self,
        file: Arc<Vec<u8>>,
        wrong: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.notice = None;
        let input = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Passphrase"));
        let root = cx.entity().downgrade();
        let handle = window.window_handle();
        cx.notify();
        window.open_dialog(cx, move |dialog, _, _| {
            let input = input.clone();
            let file = file.clone();
            let root = root.clone();
            let decrypt_input = input.clone();
            dialog
                .w(px(420.0))
                .title("DBX export is encrypted")
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child("Enter the passphrase chosen when the file was exported."),
                        )
                        .child(
                            div()
                                .debug_selector(|| "import-passphrase-input".into())
                                .child(Input::new(&input).mask_toggle()),
                        )
                        .when(wrong, |this| {
                            this.child(
                                div()
                                    .debug_selector(|| "import-passphrase-error".into())
                                    .text_size(px(12.0))
                                    .text_color(theme::text_muted())
                                    .child(ImportError::WrongPassphrase.to_string()),
                            )
                        }),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("import-passphrase-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "import-passphrase-cancel".into())
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("import-passphrase-ok")
                                .label("Decrypt")
                                .compact()
                                .primary()
                                .debug_selector(|| "import-passphrase-ok".into())
                                .on_click(move |_, window, cx| {
                                    let passphrase = decrypt_input.read(cx).value().to_string();
                                    window.close_dialog(cx);
                                    let file = file.clone();
                                    let _ = root.update(cx, |this, cx| {
                                        this.decrypt_dbx_import(file, passphrase, handle, cx)
                                    });
                                }),
                        ),
                )
        });
    }

    fn decrypt_dbx_import(
        &mut self,
        file: Arc<Vec<u8>>,
        passphrase: String,
        handle: gpui::AnyWindowHandle,
        cx: &mut Context<Self>,
    ) {
        let background = {
            let file = file.clone();
            cx.background_executor().spawn(async move {
                smol::unblock(move || dbx::parse(&file, Some(&passphrase))).await
            })
        };
        cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = handle.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| match result {
                    Ok(connections) if connections.is_empty() => {
                        this.sidebar.notice =
                            Some("No connections found in the selected DBX Config file".into());
                        cx.notify();
                    }
                    Ok(connections) => this.open_import_selection(
                        ImportPreview {
                            source: ImportSource::Dbx,
                            connections,
                        },
                        window,
                        cx,
                    ),
                    Err(ImportError::WrongPassphrase) => {
                        this.open_import_passphrase_dialog(file, true, window, cx)
                    }
                    Err(error) => this.import_failed(ImportSource::Dbx, &error, cx),
                });
            });
        })
        .detach();
    }

    /// `--ui-scenario import-selection`: the dialog over a sample Navicat export.
    pub(super) fn open_scenario_import_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sample = include_bytes!("../connection_import/fixtures/navicat.ncx");
        let connections = crate::connection_import::navicat::parse(sample).unwrap_or_default();
        let source = ImportSource::Navicat;
        self.open_import_selection(ImportPreview { source, connections }, window, cx);
    }

    pub(super) fn open_import_selection(
        &mut self,
        preview: ImportPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar.notice = None;
        let state = cx.new(|_| ImportSelection::new(preview));
        self.import_selection = Some(state.clone());
        let root = cx.entity().downgrade();
        let close_root = root.clone();
        // Where the list is scrolled to; the dialog is rebuilt on every render.
        let list_scroll = UniformListScrollHandle::new();
        cx.notify();
        window.open_dialog(cx, move |dialog, _, cx| {
            let view = state.read(cx);
            let source = view.source;
            let total = view.connections.len();
            let selected = view.selected_count();
            let supported = view.supported_count();
            let skipped = view.unsupported.join(", ");
            let tunnels = view.tunnel_count();
            let note = view.note;

            let list_state = state.clone();
            let list = uniform_list("import-selection-list", total, move |range, _, cx| {
                range
                    .map(|index| {
                        let view = list_state.read(cx);
                        let connection = &view.connections[index];
                        let supported = connection.kind.supported().is_some();
                        let checked = view.checked[index];
                        let detail = if supported {
                            format!("{} · {}", connection.kind.label(), connection.summary())
                        } else {
                            format!("{} is not supported", connection.kind.label())
                        };
                        let name = connection.name.clone();
                        let tunnel = connection.tunnel_label().filter(|_| supported);
                        let row_state = list_state.clone();
                        let toggle_state = list_state.clone();
                        div()
                            .id(("import-row", index))
                            .h(px(ROW_HEIGHT))
                            .w_full()
                            .flex()
                            .flex_col()
                            .justify_center()
                            .px(px(8.0))
                            .debug_selector(move || format!("import-row-{index}"))
                            .when(supported, |row| {
                                row.on_click(move |_, _, cx| {
                                    toggle_state.update(cx, |state, cx| {
                                        let next = !state.checked[index];
                                        state.set(index, next);
                                        cx.notify();
                                    });
                                })
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        Checkbox::new(("import-check", index))
                                            .checked(checked)
                                            .disabled(!supported)
                                            .label(name)
                                            .on_click(move |requested: &bool, _, cx| {
                                                let requested = *requested;
                                                row_state.update(cx, |state, cx| {
                                                    state.set(index, requested);
                                                    cx.notify();
                                                });
                                            }),
                                    )
                                    // The bastion is part of what is being imported: on the
                                    // name line, where the list's ellipsis never cuts it.
                                    .children(tunnel.map(|label| {
                                        div()
                                            .debug_selector(move || format!("import-row-tunnel-{index}"))
                                            .ml_auto()
                                            .flex_shrink_0()
                                            .text_size(px(11.0))
                                            .text_color(theme::text())
                                            .child(label)
                                    })),
                            )
                            .child(
                                div()
                                    .pl(px(24.0))
                                    .text_size(px(11.0))
                                    .text_color(theme::text_muted())
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .child(detail),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
            .h(px(260.0))
            .w_full()
            .track_scroll(&list_scroll);

            let all_state = state.clone();
            let none_state = state.clone();
            let import_state = state.clone();
            let import_root = root.clone();
            let cancel_root = root.clone();
            dialog
                .w(px(520.0))
                .title(format!("Import from {}", source.title()))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(
                            div()
                                .debug_selector(|| "import-selection-summary".into())
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(format!("{supported} of {total} connections can be imported.")),
                        )
                        .when(tunnels > 0, |this| {
                            this.child(
                                div()
                                    .debug_selector(|| "import-selection-tunnels".into())
                                    .text_size(px(12.0))
                                    .text_color(theme::text())
                                    .child(format!(
                                        "{tunnels} of these connections use an SSH tunnel or a proxy (shown on their rows). It is switched on only where nothing is missing; check every hop before you connect.",
                                    )),
                            )
                        })
                        .when(!skipped.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_muted())
                                    .child(format!("Not supported by TableX: {skipped}")),
                            )
                        })
                        .when_some(note, |this, note| {
                            this.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_muted())
                                    .child(note),
                            )
                        })
                        .child(
                            div()
                                .h(px(260.0))
                                .w_full()
                                .flex_shrink_0()
                                .border_1()
                                .border_color(theme::border())
                                .debug_selector(|| "import-selection-list".into())
                                .relative()
                                .child(list)
                                // The body of the dialog is a `Scrollable`, whose `ScrollBounce`
                                // takes every wheel event over it while the dialog is taller than
                                // the window. The mask scrolls the list first and hands the wheel
                                // back to the dialog at the list's end.
                                .child(ScrollableMask::new(gpui::Axis::Vertical, &list_scroll)),
                        ),
                )
                .footer(
                    DialogFooter::new()
                        .child(
                            Button::new("import-select-all")
                                .label("Select all")
                                .compact()
                                .ghost()
                                .debug_selector(|| "import-select-all".into())
                                .on_click(move |_, _, cx| {
                                    all_state.update(cx, |state, cx| {
                                        state.set_all(true);
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new("import-select-none")
                                .label("Select none")
                                .compact()
                                .ghost()
                                .debug_selector(|| "import-select-none".into())
                                .on_click(move |_, _, cx| {
                                    none_state.update(cx, |state, cx| {
                                        state.set_all(false);
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new("import-cancel")
                                .label("Cancel")
                                .compact()
                                .ghost()
                                .debug_selector(|| "import-cancel".into())
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = cancel_root.update(cx, |this, cx| {
                                        this.import_selection = None;
                                        cx.notify();
                                    });
                                }),
                        )
                        .child(
                            Button::new("import-confirm")
                                .label(format!("Import {selected}"))
                                .compact()
                                .primary()
                                .disabled(selected == 0)
                                .debug_selector(|| "import-confirm".into())
                                .on_click(move |_, window, cx| {
                                    let view = import_state.read(cx);
                                    let chosen = view.selected();
                                    let skipped = view.unsupported.clone();
                                    if chosen.is_empty() {
                                        return;
                                    }
                                    window.close_dialog(cx);
                                    let _ = import_root.update(cx, |this, cx| {
                                        this.import_selection = None;
                                        this.commit_connection_import(source, chosen, skipped, cx)
                                    });
                                }),
                        ),
                )
                .on_close({
                    let close_root = close_root.clone();
                    move |_, _, cx| {
                        let _ = close_root.update(cx, |this, cx| {
                            this.import_selection = None;
                            cx.notify();
                        });
                    }
                })
        });
    }

    fn commit_connection_import(
        &mut self,
        source: ImportSource,
        selected: Vec<ImportedConnection>,
        skipped_unsupported: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        let mut batch = Batch::build(&selected, current_time());
        let invalid = batch.invalid;
        let already_present = batch.skip_existing(&self.saved_connections);
        if batch.records.is_empty() {
            self.sidebar.notice = Some(if already_present > 0 && invalid == 0 {
                format!(
                    "The selected connection{} {} already saved, nothing imported",
                    if already_present == 1 { "" } else { "s" },
                    if already_present == 1 { "is" } else { "are" },
                )
            } else {
                "None of the selected connections could be imported".into()
            });
            cx.notify();
            return;
        }
        let records = batch.records.clone();
        let store = self.import_store.clone();
        let background = cx.background_executor().spawn(async move {
            smol::unblock(move || store.save(batch.records, batch.secrets)).await
        });
        cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(outcome) => {
                        this.sidebar.notice = None;
                        this.apply_imported_connections(&records, cx);
                        let summary = ImportSummary {
                            source,
                            imported: records.len(),
                            skipped_unsupported,
                            invalid,
                            passwords_saved: outcome.passwords_saved,
                            passwords_failed: outcome.passwords_failed,
                            already_present,
                        };
                        if this.sidebar.notice.is_none() {
                            this.sidebar.notice = Some(summary.message());
                        }
                    }
                    Err(error) => {
                        let message = format!("{error}; no connections were changed");
                        this.report_error(Area::Connect, None, &message);
                        this.sidebar.notice = Some(message);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// A UI scenario has no saved-connection database to reload.
    fn apply_imported_connections(
        &mut self,
        records: &[engine_storage::connection::SavedConnectionRecord],
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_none() {
            self.refresh_saved_connections(cx);
            return;
        }
        let mut saved = self.saved_connections.clone();
        saved.extend(records.iter().map(SavedConnection::from_record));
        self.apply_saved_connections(saved, cx);
    }
}
