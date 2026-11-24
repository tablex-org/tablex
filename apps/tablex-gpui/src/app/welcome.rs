//! The Welcome tab: where to start (a connection, a query, a file, an import, the Driver Manager), the
//! connections used last, a note while no driver is installed, the main shortcuts and the project's
//! links. Everything it shows is already in memory: a render reads no file and runs nothing.

use super::*;
use crate::connection_import::ImportSource;
use crate::{navigator::driver_title, theme};
use gpui::SharedString;
use gpui_component::kbd::Kbd;
use uuid::Uuid;

/// How many connections the Recent list shows.
pub(super) const RECENT_LIMIT: usize = 5;
/// The page's text column.
const MAX_WIDTH: f32 = 820.0;
/// The narrowest a section gets before the next one wraps under it.
const SECTION_MIN_WIDTH: f32 = 280.0;
const REPOSITORY: &str = "https://github.com/tablex-org/tablex";
const RELEASE_NOTES: &str = concat!("https://github.com/tablex-org/tablex/releases/tag/v", env!("CARGO_PKG_VERSION"));

/// When each saved connection last came up (the store's `last_connected`, seconds since 1970).
#[derive(Default)]
pub(super) struct WelcomeState {
    pub(super) last_used: HashMap<Uuid, f64>,
}

impl WelcomeState {
    /// What the store knows: the connections that have ever connected.
    pub(super) fn from_records(records: &[engine_storage::connection::SavedConnectionRecord]) -> Self {
        let last_used = records
            .iter()
            .filter_map(|record| Some((Uuid::parse_str(&record.id).ok()?, record.last_connected?)))
            .collect();
        Self { last_used }
    }
}

/// The connections the Recent list shows: those that connected, the latest first, then the rest in the
/// sidebar's order; at most `limit`. A connection of a kind this build cannot open is left out.
pub(super) fn recent_connections<'a>(
    saved: &'a [SavedConnection],
    last_used: &HashMap<Uuid, f64>,
    limit: usize,
) -> Vec<&'a SavedConnection> {
    let mut openable: Vec<&SavedConnection> = saved.iter().filter(|saved| !saved.disabled).collect();
    // Stable: connections never used keep their order.
    openable.sort_by(|a, b| match (last_used.get(&a.id), last_used.get(&b.id)) {
        (Some(a), Some(b)) => b.total_cmp(a),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    openable.truncate(limit);
    openable
}

/// The shortcuts the page lists, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hint {
    CommandPalette,
    GoToObject,
    OpenSqlFile,
    RunQuery,
    Settings,
}

impl Hint {
    pub(super) const ALL: [Hint; 5] =
        [Self::CommandPalette, Self::GoToObject, Self::OpenSqlFile, Self::RunQuery, Self::Settings];

    /// The registry entry whose chord it shows.
    pub(super) fn shortcut(self) -> &'static str {
        match self {
            Self::CommandPalette => "workspace.open_command_palette",
            Self::GoToObject => "navigation.go_to_object",
            Self::OpenSqlFile => "editor.open_sql_file",
            Self::RunQuery => "editor.run",
            Self::Settings => "workspace.open_settings",
        }
    }

    fn title(self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::CommandPalette => t!("welcome.command_palette"),
            Self::GoToObject => t!("welcome.go_to_object"),
            Self::OpenSqlFile => t!("welcome.open_sql_file"),
            Self::RunQuery => t!("welcome.run_query"),
            Self::Settings => t!("titlebar.settings"),
        }
    }
}

/// The shortcuts that have a key now (a rebound or removed one follows the user's settings), each with
/// its first chord.
pub(super) fn hints(overrides: &shortcuts::Overrides) -> Vec<(Hint, gpui::Keystroke)> {
    Hint::ALL
        .into_iter()
        .filter_map(|hint| {
            let chord = shortcuts::find(hint.shortcut())?.effective(overrides).into_iter().next()?;
            Some((hint, shortcuts::parse_chord(&chord).ok()?))
        })
        .collect()
}

fn icon(path: &'static str) -> Icon {
    Icon::empty().path(path).size(px(14.0)).text_color(theme::text_muted())
}

fn section(selector: &'static str, title: SharedString) -> Div {
    div()
        .debug_selector(move || selector.into())
        .flex()
        .flex_col()
        .flex_1()
        .min_w(px(SECTION_MIN_WIDTH))
        .gap(px(4.0))
        .child(
            div()
                .pb(px(4.0))
                .text_size(px(11.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(title),
        )
}

/// A row of the page: an icon and a label, the label reading from the left.
fn row_button(id: impl Into<gpui::ElementId>, selector: String, glyph: Icon, label: impl IntoElement) -> Button {
    Button::new(id)
        .debug_selector(move || selector.clone())
        .ghost()
        .w_full()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(glyph)
                .child(div().flex_1().min_w_0().truncate().text_left().child(label)),
        )
}

impl AppRoot {
    /// No driver is installed or loaded (the startup read of the store has finished).
    pub(super) fn welcome_needs_drivers(&self) -> bool {
        self.driver_ui.installed_loaded && self.driver_ui.installed.is_empty() && self.driver_ui.handles.is_empty()
    }

    /// A connection has come up: it moves to the top of the Recent list, and the store remembers it.
    pub(super) fn welcome_note_connected(&mut self, connection: &str, cx: &mut Context<Self>) {
        let Some(saved) = self.saved_connections.iter().find(|saved| saved.matches_id(connection)) else {
            return;
        };
        let now = connection_ui::current_time();
        self.welcome.last_used.insert(saved.id, now);
        // A scenario has no store, and a test never writes the user's.
        if self.scenario_ui.service.is_some() || cfg!(test) {
            return;
        }
        // The ids the store holds are upper case.
        let id = saved.id.to_string().to_uppercase();
        let connection = connection.to_owned();
        let job = cx
            .background_executor()
            .spawn(async move { smol::unblock(move || engine_storage::connection::touch_connection(&id, now)).await });
        cx.spawn(async move |this, cx| {
            if let Err(error) = job.await {
                let _ = this.update(cx, |this, _| this.welcome_touch_failed(&connection, &error));
            }
        })
        .detach();
    }

    /// The store could not note the connection: the Recent list is right for this run only, so
    /// it is said in the error log instead of being dropped.
    pub(super) fn welcome_touch_failed(&mut self, connection: &str, error: &dyn std::fmt::Display) {
        self.report_warning(
            Area::App,
            Some(connection),
            format!("Could not save when the connection was last used: {error}"),
        );
    }

    pub(super) fn render_welcome(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let root = cx.entity().downgrade();
        let version = format!("v{}", env!("CARGO_PKG_VERSION"));

        let header = div()
            .debug_selector(|| "welcome-header".into())
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(div().text_size(px(24.0)).font_weight(gpui::FontWeight::SEMIBOLD).child("tablex"))
                    .child(
                        div()
                            .debug_selector(|| "welcome-version".into())
                            .text_size(px(12.0))
                            .text_color(theme::text_faint())
                            .child(version),
                    ),
            )
            .child(div().text_color(theme::text_muted()).child(t!("welcome.subtitle")));

        let drivers = self.welcome_needs_drivers().then(|| {
            div()
                .debug_selector(|| "welcome-drivers-hint".into())
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(12.0))
                .p(px(12.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme::warning())
                .bg(theme::surface())
                .child(
                    div()
                        .flex_1()
                        .min_w(px(SECTION_MIN_WIDTH))
                        .child(div().font_weight(gpui::FontWeight::MEDIUM).child(t!("welcome.no_drivers_title")))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .child(t!("welcome.no_drivers_body")),
                        ),
                )
                .child(
                    Button::new("welcome-install-drivers")
                        .debug_selector(|| "welcome-install-drivers".into())
                        .primary()
                        .compact()
                        .label(t!("welcome.install_drivers"))
                        .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
                )
        });

        let import_root = root.clone();
        let start = section("welcome-start", ts!("welcome.start"))
            .child(
                row_button(
                    "welcome-new-connection",
                    "welcome-new-connection".into(),
                    icon("icons/plus.svg"),
                    t!("titlebar.new_connection"),
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_connection_dialog(None, window, cx))),
            )
            .child(
                row_button(
                    "welcome-new-query",
                    "welcome-new-query".into(),
                    icon("icons/square-terminal.svg"),
                    t!("titlebar.new_query"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_scoped_or_generic_query(cx))),
            )
            .child(
                row_button(
                    "welcome-open-sql",
                    "welcome-open-sql".into(),
                    icon("icons/folder-open.svg"),
                    t!("welcome.open_sql_file"),
                )
                .on_click(cx.listener(|this, _, window, cx| this.open_sql_file(window, cx))),
            )
            .child(
                row_button(
                    "welcome-import",
                    "welcome-import".into(),
                    icon("icons/download.svg"),
                    t!("welcome.import_connections"),
                )
                .dropdown_caret(true)
                .dropdown_menu(move |menu, _, _| {
                    // The sidebar's own entries and texts.
                    let item = |selector: &'static str, label: std::borrow::Cow<'static, str>, source: ImportSource| {
                        let root = import_root.clone();
                        PopupMenuItem::element(move |_, _| {
                            div().debug_selector(move || selector.to_string()).child(label.clone())
                        })
                        .on_click(move |_, window, app| {
                            let _ = root.update(app, |this, cx| this.import_foreign_connections(source, window, cx));
                        })
                    };
                    menu.item(item("welcome-import-dbx", t!("sidebar.import_dbx"), ImportSource::Dbx))
                        .item(item("welcome-import-navicat", t!("sidebar.import_navicat"), ImportSource::Navicat))
                        .item(item("welcome-import-dbeaver", t!("sidebar.import_dbeaver"), ImportSource::Dbeaver))
                        .item(item("welcome-import-datagrip", t!("sidebar.import_datagrip"), ImportSource::DataGrip))
                }),
            )
            .child(
                row_button(
                    "welcome-driver-manager",
                    "welcome-driver-manager".into(),
                    icon("icons/settings-2.svg"),
                    t!("titlebar.driver_manager"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.open_driver_manager(cx))),
            );

        let recent = recent_connections(&self.saved_connections, &self.welcome.last_used, RECENT_LIMIT);
        let recent_section = section("welcome-recent", ts!("welcome.recent")).children(if recent.is_empty() {
            vec![div()
                .debug_selector(|| "welcome-recent-empty".into())
                .px(px(12.0))
                .py(px(6.0))
                .text_size(px(12.0))
                .text_color(theme::text_muted())
                .child(t!("welcome.recent_empty"))
                .into_any_element()]
        } else {
            recent
                .into_iter()
                .enumerate()
                .map(|(index, saved)| {
                    let id = saved.id.to_string();
                    let logo = match navigator_ui::connection_brand_asset(saved.kind) {
                        Some(asset) => gpui::img(asset).w(px(14.0)).h(px(14.0)).flex_shrink_0().into_any_element(),
                        None => Icon::new(IconName::Building2).size(px(14.0)).text_color(theme::text_muted()).into_any_element(),
                    };
                    Button::new(("welcome-recent-row", index))
                        .debug_selector(move || format!("welcome-recent-{index}"))
                        .ghost()
                        .w_full()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(logo)
                                .child(div().flex_1().min_w_0().truncate().text_left().child(saved.name.clone()))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .text_size(px(11.0))
                                        .text_color(theme::text_faint())
                                        .child(driver_title(saved.kind)),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.open_connection_target(&id, cx)))
                        .into_any_element()
                })
                .collect()
        });

        let keys = section("welcome-keyboard", ts!("welcome.keyboard")).children(
            hints(&self.settings.shortcuts).into_iter().map(|(hint, stroke)| {
                let selector = format!("welcome-key-{}", hint.shortcut());
                div()
                    .debug_selector(move || selector.clone())
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.0))
                    .px(px(12.0))
                    .h(px(30.0))
                    .child(div().flex_1().min_w_0().truncate().child(hint.title()))
                    .child(Kbd::new(stroke))
            }),
        );

        let link = |id: &'static str, label: std::borrow::Cow<'static, str>, url: &'static str| {
            row_button(id, id.into(), icon("icons/link.svg"), label).on_click(move |_, _, cx| cx.open_url(url))
        };
        let links = section("welcome-links", ts!("welcome.links"))
            .child(link("welcome-link-repository", t!("welcome.repository"), REPOSITORY))
            .child(link("welcome-link-release-notes", t!("welcome.release_notes"), RELEASE_NOTES));

        div()
            .debug_selector(|| "welcome-page".into())
            .flex_1()
            .min_h_0()
            .min_w_0()
            .text_color(theme::text())
            .text_size(px(13.0))
            .child(
                div().size_full().overflow_y_scrollbar().child(
                    div().w_full().flex().justify_center().child(
                        div()
                            .w_full()
                            .max_w(px(MAX_WIDTH))
                            .p(px(24.0))
                            .flex()
                            .flex_col()
                            .gap(px(20.0))
                            .child(header)
                            .children(drivers)
                            .child(div().flex().flex_wrap().gap(px(24.0)).child(start).child(recent_section))
                            .child(div().flex().flex_wrap().gap(px(24.0)).child(keys).child(links)),
                    ),
                ),
            )
            .into_any_element()
    }
}
