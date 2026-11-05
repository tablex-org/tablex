use super::*;
use gpui_component::menu::PopupMenu;

impl AppRoot {
    pub(super) fn open_create_selected_group_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(snapshot) = self.selected_sidebar_connection_ids() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Group name"));
        let root = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let root = root.clone();
            let snapshot = snapshot.clone();
            let input = input.clone();
            dialog
                .title(format!("Group {} Selected Connections", snapshot.len()))
                .child(Input::new(&input))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default().ok_text("Create Group"),
                )
                .confirm()
                .on_ok(move |_, _, app| {
                    let name = input.read(app).value().to_string();
                    root.update(app, |this, cx| {
                        if !this.selected_sidebar_snapshot_matches(&snapshot) {
                            this.sidebar.notice =
                                Some("Selection changed; retry creating the group".into());
                            cx.notify();
                            return false;
                        }
                        match this.create_group_from_selected_connections(&name, cx) {
                            Ok(_) => true,
                            Err(error) => {
                                this.report_error(Area::Connect, None, &error);
                                this.sidebar.notice = Some(error);
                                cx.notify();
                                false
                            }
                        }
                    })
                    .unwrap_or(false)
                })
        });
        cx.notify();
    }
}

pub(super) fn append_selected_connection_menu(
    menu: PopupMenu,
    root: gpui::WeakEntity<AppRoot>,
    snapshot: Vec<String>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let count = snapshot.len();
    let groups: Vec<(String, String)> = root
        .update(cx, |this, _| {
            this.navigator
                .preferences()
                .groups
                .iter()
                .map(|group| (group.id.clone(), group.name.clone()))
                .collect()
        })
        .unwrap_or_default();
    let group_root = root.clone();
    let group_ids = snapshot.clone();
    let move_root = root.clone();
    let move_ids = snapshot.clone();
    let hide_root = root.clone();
    let hide_ids = snapshot.clone();
    let delete_root = root.clone();
    let copy_root = root.clone();
    let copy_ids = snapshot.clone();
    let export_root = root.clone();
    let export_ids = snapshot.clone();
    let select_root = root.clone();
    let exit_root = root.clone();
    menu.separator()
        .label(format!("{count} selected connections"))
        .item(
            PopupMenuItem::new("Group selected…").on_click(move |_, window, app| {
                let _ = group_root.update(app, |this, cx| {
                    if this.selected_sidebar_snapshot_matches(&group_ids) {
                        this.open_create_selected_group_dialog(window, cx);
                    }
                });
            }),
        )
        .submenu(
            "Move selected to group",
            window,
            cx,
            move |submenu, _, _| {
                let root = move_root.clone();
                let ids = move_ids.clone();
                let mut built =
                    submenu.item(PopupMenuItem::new("Ungrouped").on_click(move |_, _, app| {
                        let _ = root.update(app, |this, cx| {
                            if this.selected_sidebar_snapshot_matches(&ids) {
                                let _ = this.move_selected_sidebar_connections(None, cx);
                            }
                        });
                    }));
                for (group_id, name) in groups.clone() {
                    let root = move_root.clone();
                    let ids = move_ids.clone();
                    built = built.item(PopupMenuItem::new(name).on_click(move |_, _, app| {
                        let _ = root.update(app, |this, cx| {
                            if this.selected_sidebar_snapshot_matches(&ids) {
                                let _ = this.move_selected_sidebar_connections(Some(&group_id), cx);
                            }
                        });
                    }));
                }
                built
            },
        )
        .item(
            PopupMenuItem::new("Hide selected connections").on_click(move |_, _, app| {
                let _ = hide_root.update(app, |this, cx| {
                    if this.selected_sidebar_snapshot_matches(&hide_ids) {
                        let _ = this.hide_selected_sidebar_connections(true, cx);
                    }
                });
            }),
        )
        .item(
            PopupMenuItem::new(format!("Delete {count} selected connections…")).on_click(
                move |_, window, app| {
                    let _ = delete_root.update(app, |this, cx| {
                        if this.selected_sidebar_snapshot_matches(&snapshot) {
                            this.confirm_delete_selected_connections(window, cx);
                        }
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new(format!("Copy {count} connection configs")).on_click(
                move |_, _, app| {
                    let _ = copy_root.update(app, |this, cx| {
                        if this.selected_sidebar_snapshot_matches(&copy_ids) {
                            let _ = this.copy_selected_sidebar_configs(&copy_ids, cx);
                        }
                    });
                },
            ),
        )
        .item(
            PopupMenuItem::new(format!("Export {count} connection configs…")).on_click(
                move |_, window, app| {
                    let _ = export_root.update(app, |this, cx| {
                        if this.selected_sidebar_snapshot_matches(&export_ids) {
                            this.export_sidebar_config_file(&export_ids, window, cx);
                        }
                    });
                },
            ),
        )
        .separator()
        .item(
            PopupMenuItem::new("Select all visible connections").on_click(move |_, _, app| {
                let _ = select_root.update(app, |this, cx| {
                    this.navigator.select_all_connections();
                    cx.notify();
                });
            }),
        )
        .item(
            PopupMenuItem::new("Exit selection").on_click(move |_, _, app| {
                let _ = exit_root.update(app, |this, cx| {
                    this.navigator.clear_multi_selection();
                    cx.notify();
                });
            }),
        )
}
