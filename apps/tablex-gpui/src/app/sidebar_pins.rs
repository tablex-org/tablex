//! Pin / Unpin in the sidebar's row menus: databases, schemas, tables, views and collections keep
//! their pins in the app database (`engine_storage::pinned_nodes`) and in memory in the navigator;
//! connections pin through their saved record. The pins are read once at start and written as they
//! change, both off the UI thread.

use super::*;
use gpui_component::menu::PopupMenu;

/// The connection id the navigator uses (the lowercase form of a UUID) for a stored one.
fn navigator_connection_id(stored: &str) -> String {
    uuid::Uuid::parse_str(stored)
        .map(|id| id.to_string())
        .unwrap_or_else(|_| stored.to_owned())
}

/// The first item of a row's menu, "Pin" or "Unpin", and a separator after it; the menu as it was
/// for a row that cannot be pinned. Reads the state at the moment the menu is built.
pub(super) fn pin_menu_head(
    menu: PopupMenu,
    root: &gpui::WeakEntity<AppRoot>,
    id: &NavigatorNodeId,
    cx: &mut impl gpui::AppContext,
) -> PopupMenu {
    let Ok(Some(pinned)) = root.update(cx, |this, _| {
        this.navigator.is_pinnable(id).then(|| this.navigator.is_pinned(id))
    }) else {
        return menu;
    };
    let (root, id) = (root.clone(), id.clone());
    menu.item(
        PopupMenuItem::new(if pinned { "Unpin" } else { "Pin" })
            .icon(Icon::empty().path("icons/pin.svg").size(px(12.0)))
            .on_click(move |_, _, app| {
                let _ = root.update(app, |this, cx| this.toggle_node_pin(&id, cx));
            }),
    )
    .separator()
}

impl AppRoot {
    /// Pins a row that is not pinned and unpins one that is: the row moves at once, the database
    /// follows, and a write that fails puts the row back and says so.
    pub(super) fn toggle_node_pin(&mut self, id: &NavigatorNodeId, cx: &mut Context<Self>) {
        if let NavigatorNodeId::Connection { connection } = id {
            self.toggle_connection_pin(&connection.clone(), cx);
            return;
        }
        let (Some(connection), Some(key)) = (navigator_ui::row_connection_id(id), id.pin_key()) else {
            return;
        };
        let pinned = !self.navigator.is_pinned(id);
        if !self.navigator.set_node_pinned(id, pinned) {
            return;
        }
        self.navigator_ui.generation = self.navigator.generation();
        self.notify_chrome(cx);
        let Some(db) = self.sidebar.pins_db.clone() else {
            return;
        };
        let storage_id = crate::connections::storage_connection_id(connection);
        let id = id.clone();
        // One write at a time, in the order the pins were made: a quick Pin then Unpin of the
        // same row must not land the other way round.
        let previous = self.sidebar.pin_writes.take();
        self.sidebar.pin_writes = Some(cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let result = smol::unblock(move || {
                if pinned {
                    engine_storage::pinned_nodes::pin(&db, &storage_id, &key)
                } else {
                    engine_storage::pinned_nodes::unpin(&db, &storage_id, &key).map(drop)
                }
            })
            .await;
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.navigator.set_node_pinned(&id, !pinned);
                    this.navigator_ui.generation = this.navigator.generation();
                    let message = format!("Could not {} the row: {error}", if pinned { "pin" } else { "unpin" });
                    this.report_error(Area::Navigator, None, &message);
                    this.sidebar.notice = Some(message);
                    this.notify_chrome(cx);
                }
            });
        }));
    }

    /// Reads the stored pins once, off the UI thread, and applies them.
    pub(super) fn load_node_pins(&mut self, cx: &mut Context<Self>) {
        let Some(db) = self.sidebar.pins_db.clone() else {
            return;
        };
        let task = cx.background_executor().spawn(async move {
            smol::unblock(move || engine_storage::pinned_nodes::list_all(&db)).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(rows) => {
                    let ids = rows.iter().filter_map(|(connection, key)| {
                        NavigatorNodeId::from_pin_key(&navigator_connection_id(connection), key)
                    });
                    this.navigator.set_node_pins(ids);
                    this.navigator_ui.generation = this.navigator.generation();
                    this.notify_chrome(cx);
                }
                Err(error) => {
                    let message = format!("Could not read the pinned rows: {error}");
                    this.report_error(Area::Navigator, None, &message);
                    this.sidebar.notice = Some(message);
                    cx.notify();
                }
            });
        })
        .detach();
    }
}
