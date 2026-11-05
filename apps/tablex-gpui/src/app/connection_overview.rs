//! The Connection tab: an overview of one saved connection. A header (brand
//! icon with its status dot, name, type / route / read-only badges, server and
//! driver version, and the actions), the Connection grid (host, user, database
//! or file, route), and the databases the navigator lists under it.
//!
//! The header and the grid render from the saved connection in every state.
//! The databases section becomes a state panel while the connection is not
//! live (Connect, a spinner, the whole error with Retry / Copy / Edit, or Edit
//! Connection when the password is missing).
//!
//! Cost: `sync_connection_overview` runs once per render of the tab and does
//! hash lookups and two string compares; the grid rows are parsed again only
//! when the URI changes; the database list is read from the navigator when its
//! catalog revision moves and filtered / sorted only when the search text or the
//! order changes (`connection_dashboard::DatabaseList`). The list is a
//! `uniform_list`: only the visible rows are built, as plain `div`s without
//! entities or per-row state. Render never queries a server; the databases are
//! requested once, when the connection is live and nothing asked for them yet.

use super::*;
use crate::connection_dashboard::{
    connection_info, overview_subtitle, DatabaseList, InfoRow, SortOrder,
};
use crate::navigator::NodeLoadState;
use gpui::{ClipboardItem, Entity, SharedString};
use gpui_component::input::InputEvent;
use gpui_component::scroll::ScrollableMask;
use gpui_component::text::TextView;
use gpui_component::tooltip::Tooltip;
use navigator_ui::SidebarGlyph;

const ICON_SIZE: f32 = 28.0;
const MAX_WIDTH: f32 = 880.0;
const ROW_HEIGHT: f32 = 30.0;
/// The list box is at most this many rows tall; the databases past them are reached by scrolling
/// it (a `uniform_list` builds only the rows in view, so the number of databases costs nothing).
const MAX_LIST_ROWS: usize = 16;
const LABEL_WIDTH: f32 = 104.0;
/// `icon_with_status` keys its dot selector and pulse by row index; the page's
/// icon must not collide with a sidebar row's.
const STATUS_INDEX: usize = usize::MAX;
const ROW_GROUP: &str = "overview-db";

/// Per-tab state of a connection overview.
pub(super) struct ConnectionOverview {
    search: Entity<InputState>,
    sort: SortOrder,
    databases: DatabaseList,
    scroll: UniformListScrollHandle,
    /// Where a long failure text is scrolled to.
    error_scroll: gpui::ScrollHandle,
    /// The URI and kind `info` was parsed from.
    info_of: Option<(String, ConnectionKind)>,
    info: Vec<InfoRow>,
}

impl ConnectionOverview {
    #[cfg(test)]
    pub(super) fn databases(&self) -> &DatabaseList {
        &self.databases
    }

    #[cfg(test)]
    pub(super) fn info(&self) -> &[InfoRow] {
        &self.info
    }
}

impl AppRoot {
    /// Runs before a connection tab renders: creates the tab's state, keeps the
    /// Connection grid and the database list current and, for a live
    /// connection whose databases nobody asked for, asks once.
    pub(super) fn sync_connection_overview(
        &mut self,
        tab: &Tab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_id = tab.id;
        if let std::collections::hash_map::Entry::Vacant(slot) =
            self.connection_overviews.entry(tab_id)
        {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder("Filter databases"));
            cx.subscribe(
                &search,
                move |this: &mut Self, _: Entity<InputState>, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.notify_tab(tab_id, cx);
                    }
                },
            )
            .detach();
            slot.insert(ConnectionOverview {
                search,
                sort: SortOrder::default(),
                databases: DatabaseList::default(),
                scroll: UniformListScrollHandle::new(),
                error_scroll: gpui::ScrollHandle::new(),
                info_of: None,
                info: Vec::new(),
            });
        }
        let connection = tab.connection_id.as_str();
        if matches!(
            self.connection_tab_state(connection),
            ConnectionTabState::Connected
        ) {
            let root = NavigatorNodeId::Connection {
                connection: connection.to_string(),
            };
            // `None` unless the node was never asked: loading, loaded and failed stay as they are.
            if let Some(request) = self.navigator.begin_background_load(&root) {
                self.navigator_ui.generation = self.navigator.generation();
                self.ensure_connected_then_load(request, cx);
            }
        }
        let Some(saved) = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection))
        else {
            return;
        };
        let Some(overview) = self.connection_overviews.get_mut(&tab_id) else {
            return;
        };
        if overview
            .info_of
            .as_ref()
            .is_none_or(|(uri, kind)| *uri != saved.uri || *kind != saved.kind)
        {
            overview.info = connection_info(saved);
            overview.info_of = Some((saved.uri.clone(), saved.kind));
        }
        let query = overview.search.read(cx).value();
        let navigator = &self.navigator;
        overview.databases.sync(
            navigator.catalog_revision(),
            &query,
            overview.sort,
            || navigator.connection_databases(connection),
        );
    }

    /// The Connection tab of `tab`.
    pub(super) fn render_connection_tab(
        &self,
        tab: &Tab,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tab_id = tab.id;
        let connection = tab.connection_id.as_str();
        let state = self.connection_tab_state(connection);
        let saved = self
            .saved_connections
            .iter()
            .find(|saved| saved.matches_id(connection));
        let (Some(saved), Some(overview)) = (saved, self.connection_overviews.get(&tab_id)) else {
            return page(connection_ui::connection_state_card(
                "Connection not found",
                "This tab belongs to a deleted saved connection.",
                None,
            ));
        };
        if state == ConnectionTabState::Unsupported {
            return page(connection_ui::connection_state_card(
                "Unsupported connection",
                "This GPUI shell currently supports PostgreSQL, MySQL, MariaDB, SQLite, and MongoDB.",
                None,
            ));
        }
        let content = div()
            .debug_selector(|| "connection-overview".into())
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(self.render_overview_header(saved, &state, tab_id, cx))
            .child(self.render_overview_grid(saved, overview))
            .child(self.render_overview_databases(saved, &state, overview, tab_id, cx));
        page(content)
    }

    fn render_overview_header(
        &self,
        saved: &SavedConnection,
        state: &ConnectionTabState,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let connection = saved.id.to_string();
        let status = self.sessions.status(&connection);
        let effective = self.effective_for(&connection);
        let via = self.via_ssh(&connection);

        let icon = connection_brand_icon(saved.kind);
        let icon = navigator_status::icon_with_status_sized(
            icon,
            &status,
            theme::background(),
            theme::background(),
            STATUS_INDEX,
            ICON_SIZE,
        );
        let (status_text, status_color) = match state {
            ConnectionTabState::Connected => ("Connected", theme::success()),
            ConnectionTabState::Connecting => ("Connecting\u{2026}", theme::warning()),
            ConnectionTabState::Failed(_) => ("Connection failed", theme::danger()),
            ConnectionTabState::PasswordRequired => ("Password required", theme::warning()),
            _ => ("Disconnected", theme::text_muted()),
        };
        let server_version = self.driver_runtime.server_versions.get(&connection).copied();
        let driver_version = self
            .driver_ui
            .records
            .get(saved.kind.driver_id())
            .map(|record| record.version.as_str());
        let subtitle: SharedString =
            overview_subtitle(saved.kind, server_version, driver_version).into();
        let name: SharedString = saved.name.clone().into();

        let badges = div()
            .flex()
            .items_center()
            .flex_shrink_0()
            .children(effective.connection_type.as_ref().map(|ty| {
                connection_policy::type_badge(ty, "connection-type-badge".into())
            }))
            .children((via.is_some() || saved.ssh_tunnel).then(|| {
                connection_policy::info_badge(
                    self.route_badge(&connection),
                    via.clone().unwrap_or_else(|| ROUTE_PENDING.to_string()),
                    "connection-route-badge".into(),
                )
            }))
            .children(effective.read_only.then(|| {
                connection_policy::info_badge(
                    "READ-ONLY",
                    "Writes are blocked on this connection",
                    "connection-read-only-badge".into(),
                )
            }));

        let title = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .debug_selector({
                        let name = name.clone();
                        move || format!("connection-overview-title={name}")
                    })
                    .min_w_0()
                    .truncate()
                    .text_size(px(18.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(name),
            )
            .child(badges)
            .child(
                div()
                    .debug_selector(move || format!("connection-overview-status={status_text}"))
                    .ml_auto()
                    .flex_shrink_0()
                    .pl(px(12.0))
                    .text_size(px(12.0))
                    .text_color(status_color)
                    .child(status_text),
            );
        let heading = div()
            .flex()
            .items_center()
            .gap(px(14.0))
            .child(icon)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(title)
                    .child(
                        div()
                            .debug_selector({
                                let subtitle = subtitle.clone();
                                move || format!("connection-overview-subtitle={subtitle}")
                            })
                            .truncate()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(subtitle),
                    ),
            );
        div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(heading)
            .child(self.render_overview_actions(saved, state, tab_id, cx))
    }

    fn render_overview_actions(
        &self,
        saved: &SavedConnection,
        state: &ConnectionTabState,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let connection = saved.id.to_string();
        let live = *state == ConnectionTabState::Connected;
        let button = |id: &'static str, label: &'static str| {
            Button::new((id, tab_id))
                .debug_selector(move || id.into())
                .label(label)
                .small()
        };
        let node = NavigatorNodeId::Connection {
            connection: connection.clone(),
        };
        let new_query = button("connection-new-query", "New Query")
            .icon(Icon::empty().path("icons/square-terminal.svg"))
            .when(live, |this| this.primary())
            .disabled(!live)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_new_query_for_node(&node, cx);
            }));
        let saved_for_edit = saved.clone();
        let edit = button("connection-edit", "Edit")
            .icon(Icon::new(SidebarGlyph::Edit))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_connection_dialog(Some(saved_for_edit.clone()), window, cx);
            }));
        let for_refresh = connection.clone();
        let refresh = button("connection-refresh", "Refresh")
            .icon(Icon::new(SidebarGlyph::Refresh))
            .disabled(!live)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.refresh_connection_id(&for_refresh, cx);
            }));
        let last = if live {
            let for_disconnect = connection.clone();
            button("connection-disconnect", "Disconnect").on_click(cx.listener(
                move |this, _, window, cx| {
                    this.request_disconnect(&for_disconnect, window, cx);
                },
            ))
        } else {
            button("connection-connect", "Connect")
                .icon(Icon::new(SidebarGlyph::Connected))
                .loading(*state == ConnectionTabState::Connecting)
                .disabled(matches!(
                    state,
                    ConnectionTabState::Connecting | ConnectionTabState::PasswordRequired
                ))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.retry_connection(tab_id, cx);
                }))
        };
        div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .child(new_query)
            .child(edit)
            .child(refresh)
            .child(last)
    }

    fn render_overview_grid(&self, saved: &SavedConnection, overview: &ConnectionOverview) -> Div {
        let route = self
            .via_ssh(&saved.id.to_string())
            .or_else(|| saved.ssh_tunnel.then(|| ROUTE_PENDING.to_string()));
        let rows = overview
            .info
            .iter()
            .map(|row| (row.label, row.value.clone()))
            .chain(route.map(|route| ("Route", route)))
            .collect::<Vec<_>>();
        let count = rows.len();
        let grid = div()
            .debug_selector(|| "connection-overview-grid".into())
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .children(rows.into_iter().enumerate().map(|(index, (label, value))| {
                let tooltip: SharedString = value.clone().into();
                div()
                    .debug_selector(move || format!("connection-info-{}", label.to_ascii_lowercase()))
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .when(index + 1 < count, |row| {
                        row.border_b_1().border_color(theme::divider())
                    })
                    .child(
                        div()
                            .w(px(LABEL_WIDTH))
                            .flex_shrink_0()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(label),
                    )
                    .child(
                        div()
                            .id(("connection-info-value", index))
                            .debug_selector({
                                let value = value.clone();
                                move || {
                                    format!("connection-info-value-{}={value}", label.to_ascii_lowercase())
                                }
                            })
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                            .child(value),
                    )
            }));
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(section_title("CONNECTION"))
            .child(grid)
    }

    fn render_overview_databases(
        &self,
        saved: &SavedConnection,
        state: &ConnectionTabState,
        overview: &ConnectionOverview,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let connection = saved.id.to_string();
        let section = div().flex().flex_col().gap(px(8.0));
        if *state != ConnectionTabState::Connected {
            return section
                .child(section_title("DATABASES"))
                .child(self.render_overview_unavailable(saved, state, tab_id, &overview.error_scroll, cx));
        }
        let databases = &overview.databases;
        let root = NavigatorNodeId::Connection {
            connection: connection.clone(),
        };
        let count: SharedString = if databases.is_filtered() {
            format!("DATABASES ({} of {})", databases.len(), databases.total())
        } else {
            format!("DATABASES ({})", databases.total())
        }
        .into();
        let sort = overview.sort;
        let for_refresh = connection.clone();
        let controls = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                div()
                    .debug_selector(|| "connection-database-search".into())
                    .w(px(220.0))
                    .child(
                        Input::new(&overview.search)
                            .small()
                            .cleanable(true)
                            .prefix(Icon::new(IconName::Search).size(px(12.0))),
                    ),
            )
            .child(
                Button::new(("connection-database-sort", tab_id))
                    .debug_selector(|| "connection-database-sort".into())
                    .icon(Icon::new(match sort {
                        SortOrder::Asc => SidebarGlyph::SortAsc,
                        SortOrder::Desc => SidebarGlyph::SortDesc,
                    }))
                    .ghost()
                    .small()
                    .tooltip(match sort {
                        SortOrder::Asc => "Sorted A to Z. Click for Z to A",
                        SortOrder::Desc => "Sorted Z to A. Click for A to Z",
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(overview) = this.connection_overviews.get_mut(&tab_id) {
                            overview.sort = overview.sort.toggled();
                        }
                        this.notify_tab(tab_id, cx);
                    })),
            )
            .child(
                Button::new(("connection-database-refresh", tab_id))
                    .debug_selector(|| "connection-database-refresh".into())
                    .icon(Icon::new(SidebarGlyph::Refresh))
                    .ghost()
                    .small()
                    .tooltip("Reload the databases")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.refresh_connection_id(&for_refresh, cx);
                    })),
            );
        let heading = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .debug_selector({
                        let count = count.clone();
                        move || format!("connection-databases-title={count}")
                    })
                    .child(section_title(count)),
            )
            .child(div().ml_auto().child(controls));

        let body = match self.navigator.load_state(&root) {
            Some(NodeLoadState::Failed(error)) => state_panel(
                Icon::new(IconName::TriangleAlert).text_color(theme::danger()),
                "Could not load databases",
                Some(selectable_error(error, tab_id, &overview.error_scroll)),
                vec![Button::new(("connection-databases-retry", tab_id))
                    .debug_selector(|| "connection-databases-retry".into())
                    .label("Retry")
                    .small()
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // Forgetting the failure lets the next render ask once more.
                        this.navigator.reset_node(&NavigatorNodeId::Connection {
                            connection: connection.clone(),
                        });
                        this.navigator_ui.generation = this.navigator.generation();
                        this.notify_tab(tab_id, cx);
                    }))
                    .into_any_element()],
            ),
            Some(NodeLoadState::Loaded) if databases.total() == 0 => message_panel("No databases."),
            Some(NodeLoadState::Loaded) if databases.len() == 0 => {
                message_panel("No databases match the filter.")
            }
            Some(NodeLoadState::Loaded) => self.render_database_list(overview, tab_id, cx),
            _ => message_panel("Loading databases\u{2026}"),
        };
        section.child(heading).child(body)
    }

    /// The virtualized list: only the rows in view are built.
    fn render_database_list(
        &self,
        overview: &ConnectionOverview,
        tab_id: u64,
        cx: &mut Context<Self>,
    ) -> Div {
        let rows = overview.databases.len();
        let list = uniform_list(
            ("connection-database-list", tab_id),
            rows,
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let Some(overview) = this.connection_overviews.get(&tab_id) else {
                    return Vec::new();
                };
                range
                    .filter_map(|position| {
                        let entry = overview.databases.get(position)?;
                        let selected = this.navigator.selected() == Some(&entry.id);
                        Some(database_row(
                            position,
                            &entry.name,
                            &entry.id,
                            selected,
                            cx,
                        ))
                    })
                    .collect()
            }),
        )
        .size_full()
        .track_scroll(&overview.scroll);
        div()
            .debug_selector(|| "connection-database-list".into())
            .relative()
            .h(px(ROW_HEIGHT * rows.min(MAX_LIST_ROWS) as f32 + 2.0))
            .overflow_hidden()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface())
            .child(list)
            // The page around the list is a `Scrollable`, whose `ScrollBounce` takes every wheel
            // event over it. The mask scrolls the list first and hands the event to the page
            // only once the list is at its end.
            .child(ScrollableMask::new(gpui::Axis::Vertical, &overview.scroll))
            .child(Scrollbar::vertical(&overview.scroll))
    }

    /// What the databases section says while the connection is not live.
    fn render_overview_unavailable(
        &self,
        saved: &SavedConnection,
        state: &ConnectionTabState,
        tab_id: u64,
        error_scroll: &gpui::ScrollHandle,
        cx: &mut Context<Self>,
    ) -> Div {
        let saved_for_edit = saved.clone();
        let edit = |id: &'static str, label: &'static str, primary: bool, requires_password: bool| {
            let saved = saved_for_edit.clone();
            Button::new((id, tab_id))
                .debug_selector(move || id.into())
                .label(label)
                .small()
                .when(primary, |this| this.primary())
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_connection_dialog_with_requirement(
                        Some(saved.clone()),
                        requires_password,
                        window,
                        cx,
                    );
                }))
                .into_any_element()
        };
        match state {
            ConnectionTabState::Connecting => message_panel("Connecting\u{2026}"),
            ConnectionTabState::PasswordRequired => state_panel(
                Icon::new(IconName::TriangleAlert).text_color(theme::warning()),
                "Password required",
                Some(text_line("Re-enter the password and save the connection.")),
                vec![edit("connection-edit-password", "Edit Connection", true, true)],
            ),
            ConnectionTabState::Failed(error) => {
                let message = error.clone();
                state_panel(
                    Icon::new(IconName::TriangleAlert).text_color(theme::danger()),
                    "Connection failed",
                    Some(selectable_error(error, tab_id, error_scroll)),
                    vec![
                        Button::new(("connection-failed-retry", tab_id))
                            .debug_selector(|| "connection-failed-retry".into())
                            .label("Retry")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.retry_connection(tab_id, cx);
                            }))
                            .into_any_element(),
                        Button::new(("connection-failed-copy", tab_id))
                            .debug_selector(|| "connection-failed-copy".into())
                            .label("Copy")
                            .small()
                            .on_click(move |_, _, app| {
                                app.write_to_clipboard(ClipboardItem::new_string(message.clone()));
                            })
                            .into_any_element(),
                        edit("connection-failed-edit", "Edit", false, false),
                    ],
                )
            }
            _ => state_panel(
                Icon::empty().path("icons/plug.svg").text_color(theme::text_muted()),
                "Not connected",
                Some(text_line("Connect to list this connection's databases.")),
                vec![Button::new(("connection-panel-connect", tab_id))
                    .debug_selector(|| "connection-panel-connect".into())
                    .label("Connect")
                    .small()
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.retry_connection(tab_id, cx);
                    }))
                    .into_any_element()],
            ),
        }
    }
}

/// Shown for a connection that is flagged as routed but not live: the route
/// is only known once its tunnel is open.
const ROUTE_PENDING: &str = "SSH tunnel or proxy, opened when connecting";

fn page(content: impl IntoElement) -> AnyElement {
    div()
        .size_full()
        .overflow_y_scrollbar()
        .text_color(theme::text())
        .child(
            div().w_full().flex().justify_center().child(
                div()
                    .w_full()
                    .max_w(px(MAX_WIDTH))
                    .p(px(24.0))
                    .text_size(px(13.0))
                    .child(content),
            ),
        )
        .into_any_element()
}

fn section_title(title: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme::text_muted())
        .child(title.into())
}

fn text_line(text: &'static str) -> AnyElement {
    div()
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

fn panel() -> Div {
    div()
        .rounded(px(6.0))
        .border_1()
        .border_color(theme::border())
        .bg(theme::surface())
        .p(px(16.0))
}

fn message_panel(text: &'static str) -> Div {
    panel()
        .debug_selector(|| "connection-databases-message".into())
        .text_size(px(12.0))
        .text_color(theme::text_muted())
        .child(text)
}

/// An icon, a title, an optional body and a row of actions.
fn state_panel(
    icon: Icon,
    title: &'static str,
    body: Option<AnyElement>,
    actions: Vec<AnyElement>,
) -> Div {
    panel()
        .debug_selector(|| "connection-state-panel".into())
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(icon.size(px(16.0)))
                .child(title),
        )
        .children(body)
        .when(!actions.is_empty(), |panel| {
            panel.child(div().flex().gap(px(8.0)).children(actions))
        })
}

/// A failure's whole text: selectable and scrollable. Like the database list, its scroll area is
/// next to a `ScrollableMask`: the page around it is a `Scrollable`, whose `ScrollBounce` would
/// otherwise take every wheel event over the text while the page overflows.
fn selectable_error(error: &str, tab_id: u64, scroll: &gpui::ScrollHandle) -> AnyElement {
    div()
        .debug_selector(|| "connection-error-text".into())
        .relative()
        .child(
            div()
                .id(("connection-error-scroll", tab_id))
                .max_h(px(160.0))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .text_size(px(12.0))
                .child(
                    div().debug_selector(|| "connection-error-body".into()).child(
                        TextView::markdown(
                            ("connection-error-text", tab_id),
                            navigator_status::problem_markdown(error),
                        )
                        .selectable(true),
                    ),
                ),
        )
        .child(ScrollableMask::new(gpui::Axis::Vertical, scroll))
        .into_any_element()
}

fn connection_brand_icon(kind: ConnectionKind) -> AnyElement {
    match navigator_ui::connection_brand_asset(kind) {
        Some(asset) => gpui::img(asset)
            .w(px(ICON_SIZE))
            .h(px(ICON_SIZE))
            .into_any_element(),
        None => Icon::new(IconName::Building2)
            .size(px(ICON_SIZE))
            .text_color(theme::text_muted())
            .into_any_element(),
    }
}

/// One database: its name, and a New Query action while the mouse is on it.
/// A click selects it in the navigator, expands it and scrolls it into view.
fn database_row(
    position: usize,
    name: &str,
    id: &NavigatorNodeId,
    selected: bool,
    cx: &mut Context<AppRoot>,
) -> AnyElement {
    let (reveal_id, query_id) = (id.clone(), id.clone());
    div()
        .id(("connection-database", position))
        .debug_selector(move || format!("connection-database-row-{position}"))
        .group(ROW_GROUP)
        .h(px(ROW_HEIGHT))
        .w_full()
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .border_b_1()
        .border_color(theme::divider())
        .cursor_pointer()
        .bg(if selected { theme::selected() } else { theme::surface() })
        .hover(|style| style.bg(theme::hover()))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.reveal_node_in_sidebar(&reveal_id, cx);
        }))
        .child(
            Icon::new(SidebarGlyph::Database)
                .size(px(14.0))
                .text_color(theme::icon_database()),
        )
        .child(div().flex_1().min_w_0().truncate().child(name.to_string()))
        .child(
            Button::new(("connection-database-new-query", position))
                .debug_selector(move || format!("connection-database-new-query-{position}"))
                .label("New Query")
                .ghost()
                .xsmall()
                .invisible()
                .group_hover(ROW_GROUP, |style| style.visible())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.open_new_query_for_node(&query_id, cx);
                    cx.stop_propagation();
                })),
        )
        .into_any_element()
}
