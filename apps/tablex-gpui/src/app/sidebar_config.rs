use super::*;
use engine_storage::connection::SavedConnectionRecord;
use std::io::{Read, Write};

impl AppRoot {
    pub(super) fn copy_selected_sidebar_configs_with(
        &mut self,
        snapshot: &[String],
        mut fetch: impl FnMut(&str) -> Result<SavedConnectionRecord, String>,
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        let current = self.selected_sidebar_connection_ids()?;
        if current != snapshot {
            return Err("Selection changed; retry copying connection configurations".into());
        }
        let records = snapshot
            .iter()
            .map(|id| fetch(id))
            .collect::<Result<Vec<_>, _>>()?;
        let data = crate::connections::export_connection_configs(&records)?;
        let json =
            String::from_utf8(data).map_err(|_| "Could not encode connection configuration")?;
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(json));
        self.sidebar.notice = Some(format!(
            "Copied {} redacted connection configurations",
            snapshot.len()
        ));
        cx.notify();
        Ok(snapshot.len())
    }

    pub(super) fn copy_selected_sidebar_configs(
        &mut self,
        snapshot: &[String],
        cx: &mut Context<Self>,
    ) -> Result<usize, String> {
        if self.scenario_ui.service.is_some() {
            return Err("Config export unavailable in a UI scenario".into());
        }
        let result = self.copy_selected_sidebar_configs_with(
            snapshot,
            |id| {
                engine_storage::connection::get_connection(
                    &crate::connections::storage_connection_id(id),
                )
                .map_err(|error| format!("Could not read saved connection: {error}"))?
                .ok_or_else(|| "Saved connection not found".into())
            },
            cx,
        );
        if let Err(error) = &result {
            self.report_error(Area::Export, None, error);
            self.sidebar.notice = Some(error.clone());
            cx.notify();
        }
        result
    }

    pub(super) fn sidebar_single_config_copy_target(&self) -> Option<String> {
        if self.navigator.selected_ids().len() != 1 {
            return None;
        }
        let NavigatorNodeId::Connection { connection } = self.navigator.selected()? else {
            return None;
        };
        self.navigator
            .action_target(self.navigator.selected()?)
            .map(|_| connection.clone())
    }

    pub(super) fn copy_sidebar_connection_config(
        &mut self,
        connection: &str,
        cx: &mut Context<Self>,
    ) {
        let result = (|| -> Result<String, String> {
            if self.scenario_ui.service.is_some() {
                return Err("Config export unavailable in a UI scenario".into());
            }
            if !self
                .saved_connections
                .iter()
                .any(|saved| saved.matches_id(connection))
            {
                return Err("Saved connection not found".into());
            }
            let record = engine_storage::connection::get_connection(
                &crate::connections::storage_connection_id(connection),
            )
            .map_err(|error| format!("Could not read saved connection: {error}"))?
            .ok_or("Saved connection not found")?;
            let bytes = crate::connections::export_connection_configs(&[record])?;
            String::from_utf8(bytes).map_err(|_| "Could not encode connection configuration".into())
        })();
        match result {
            Ok(json) => {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(json));
                self.sidebar.notice = Some(
                    "Copied redacted connection configuration; passwords were excluded".into(),
                );
            }
            Err(error) => {
                self.report_error(Area::Export, None, &error);
                self.sidebar.notice = Some(error);
            }
        }
        cx.notify();
    }

    pub(super) fn paste_sidebar_connection_config(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let data = cx.read_from_clipboard().and_then(|item| item.text());
        let Some(data) = data else {
            self.sidebar.notice = Some("Clipboard has no connection configuration".into());
            cx.notify();
            return;
        };
        self.preview_sidebar_config_import(data.as_bytes(), window, cx);
    }

    fn preview_sidebar_config_import(
        &mut self,
        bytes: &[u8],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let records = match crate::connections::stage_connection_configs(bytes, current_time()) {
            Ok(records) => records,
            Err(error) => {
                self.report_error(Area::Connect, None, &error);
                self.sidebar.notice = Some(error);
                cx.notify();
                return;
            }
        };
        let mut summary = records
            .iter()
            .take(5)
            .map(|record| format!("{} ({})", record.name, record.db_type))
            .collect::<Vec<_>>()
            .join(", ");
        if records.len() > 5 {
            summary.push_str(&format!(", and {} more", records.len() - 5));
        }
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let records = records.clone();
            let summary = summary.clone();
            dialog.title("Import Connection Configurations")
                .child(format!("Import {} saved connections: {summary}. Credentials are omitted; SSH/proxy are disabled until reconfigured.", records.len()))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Import"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| this.commit_sidebar_config_import(records.clone(), cx));
                    true
                })
        });
        cx.notify();
    }

    fn commit_sidebar_config_import(
        &mut self,
        records: Vec<SavedConnectionRecord>,
        cx: &mut Context<Self>,
    ) {
        let count = records.len();
        let background = cx.background_executor().spawn(async move {
            smol::unblock(move || engine_storage::connection::upsert_connections_batch(&records))
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = background.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.sidebar.notice = None;
                        this.refresh_saved_connections(cx);
                        if this.sidebar.notice.is_none() {
                            this.sidebar.notice = Some(format!("Imported {count} connections; re-enter passwords before connecting"));
                        }
                    }
                    Err(error) => {
                        this.report_error(Area::Connect, None, format!("Connection import failed; no connections were changed: {error}"));
                        this.sidebar.notice = Some(format!("Connection import failed; no connections were changed: {error}"));
                    }
                }
                cx.notify();
            });
        }).detach();
    }

    pub(super) fn import_sidebar_config_file(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let prompt = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import connection configurations".into()),
        });
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let selection = prompt.await;
            let path = match selection {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.sidebar.notice =
                            Some("Could not open connection configuration picker".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let Some(path) = path else {
                return;
            };
            let data = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || -> Result<Vec<u8>, String> {
                        let mut file = std::fs::File::open(path)
                            .map_err(|error| format!("Could not read configuration: {error}"))?;
                        let mut data = Vec::new();
                        Read::by_ref(&mut file)
                            .take(1_048_577)
                            .read_to_end(&mut data)
                            .map_err(|error| format!("Could not read configuration: {error}"))?;
                        Ok(data)
                    })
                    .await
                })
                .await;
            match data {
                Ok(data) => {
                    let _ = handle.update(cx, |_, window, cx| {
                        let _ = this.update(cx, |this, cx| {
                            this.preview_sidebar_config_import(&data, window, cx)
                        });
                    });
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| {
                        this.report_error(Area::Connect, None, &error);
                        this.sidebar.notice = Some(error);
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn export_sidebar_config_file(
        &mut self,
        ids: &[String],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.scenario_ui.service.is_some() {
            return;
        }
        let live = if ids.len() > 1 {
            self.selected_sidebar_connection_ids().ok() == Some(ids.to_vec())
        } else {
            ids.first().is_some_and(|id| {
                self.navigator
                    .action_target(&NavigatorNodeId::Connection {
                        connection: id.clone(),
                    })
                    .is_some()
            })
        };
        if !live {
            self.sidebar.notice =
                Some("Selection changed; retry exporting connection configurations".into());
            cx.notify();
            return;
        }
        let result = (|| -> Result<Vec<SavedConnectionRecord>, String> {
            if ids.is_empty() {
                return Err("Select connections before exporting configuration".into());
            }
            ids.iter()
                .map(|id| {
                    if !self
                        .saved_connections
                        .iter()
                        .any(|saved| saved.matches_id(id))
                    {
                        return Err("Saved connection not found".into());
                    }
                    engine_storage::connection::get_connection(
                        &crate::connections::storage_connection_id(id),
                    )
                    .map_err(|error| format!("Could not read saved connection: {error}"))?
                    .ok_or_else(|| "Saved connection not found".into())
                })
                .collect()
        })();
        let records = match result {
            Ok(records) => records,
            Err(error) => {
                self.report_error(Area::Export, None, &error);
                self.sidebar.notice = Some(error);
                cx.notify();
                return;
            }
        };
        let json = match crate::connections::export_connection_configs(&records) {
            Ok(json) => json,
            Err(error) => {
                self.report_error(Area::Export, None, &error);
                self.sidebar.notice = Some(error);
                cx.notify();
                return;
            }
        };
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let json = json.clone();
            let count = records.len();
            dialog.title("Export Connection Configurations")
                .child(format!("Export {count} connections without credentials? Existing files will not be replaced."))
                .button_props(gpui_component::dialog::DialogButtonProps::default().ok_text("Choose File"))
                .confirm()
                .on_ok(move |_, _, cx| {
                    let _ = root.update(cx, |this, cx| this.choose_sidebar_config_export(json.clone(), cx));
                    true
                })
        });
    }

    fn choose_sidebar_config_export(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let directory = crate::platform::save_dialog_dir();
        let prompt = cx.prompt_for_new_path(&directory, Some("tablex-connections.json"));
        cx.spawn(async move |this, cx| {
            let selection = prompt.await;
            let path = match selection {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                _ => {
                    let _ = this.update(cx, |this, cx| {
                        this.sidebar.notice =
                            Some("Could not open connection export picker".into());
                        cx.notify();
                    });
                    return;
                }
            };
            let result = cx
                .background_executor()
                .spawn(async move {
                    smol::unblock(move || -> std::io::Result<()> {
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)?;
                        let result = file.write_all(&bytes).and_then(|_| file.sync_all());
                        if let Err(error) = result {
                            return match std::fs::remove_file(&path) {
                                Ok(()) => Err(error),
                                Err(cleanup) => Err(std::io::Error::new(
                                    error.kind(),
                                    format!(
                                        "{error}; could not remove incomplete export: {cleanup}"
                                    ),
                                )),
                            };
                        }
                        Ok(())
                    })
                    .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = &result {
                    this.report_error(Area::Export, None, format!("Could not export connection configuration: {error}"));
                }
                this.sidebar.notice = Some(match result {
                    Ok(()) => "Exported redacted connection configuration".into(),
                    Err(error) => format!("Could not export connection configuration: {error}"),
                });
                cx.notify();
            });
        })
        .detach();
    }
}
