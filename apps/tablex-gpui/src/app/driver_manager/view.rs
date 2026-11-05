//! The Driver Manager tab. The list is a `uniform_list` of fixed-height rows;
//! clicks on a row (select, primary action, cancel) are hit-tested by ONE
//! mouse-down handler from the painted list bounds and the fixed row
//! geometry, like the results grid, so rows own no listeners. The `⋯` menu is
//! the official `DropdownMenu` on a plain button (its popover state exists
//! only for the visible rows).

use super::*;
use gpui_component::input::InputEvent;
use gpui_component::menu::PopupMenu;
use std::rc::Rc;
use gpui_component::progress::Progress;

pub(super) const ROW_HEIGHT: f32 = 56.0;
pub(super) const ROW_PAD: f32 = 12.0;
pub(super) const ACTION_WIDTH: f32 = 84.0;
pub(super) const ADDON_DRIVER_ACTION_WIDTH: f32 = 136.0;
pub(super) const MORE_WIDTH: f32 = 32.0;
pub(super) const ACTION_GAP: f32 = 6.0;
const BUTTON_HEIGHT: f32 = 28.0;
const DETAIL_WIDTH: f32 = 250.0;
/// The detail pane needs a list at least this wide beside it.
const LIST_MIN_WIDTH_WITH_DETAIL: f32 = 540.0;
/// Below this the state badge leaves its column and replaces the id line.
pub(super) const LIST_MIN_WIDTH_FOR_STATE_COLUMN: f32 = 640.0;
/// Width of everything in a row but the capability chips, with the state column.
const ROW_FIXED_WIDTH: f32 = 640.0;
/// Less room than this and the chips column stays empty.
const MIN_CHIP_BUDGET: f32 = 70.0;

/// A row of the list: a driver (index among the visible rows) or the MongoDB Shell add-on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListEntry {
    Driver(usize),
    Addon,
}

/// Width of the add-on row's primary button: the driver-first labels are long.
fn addon_action_width(action: ToolAction) -> f32 {
    match action {
        ToolAction::InstallDriverFirst | ToolAction::UpdateDriverFirst => ADDON_DRIVER_ACTION_WIDTH,
        _ => ACTION_WIDTH,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowHit {
    /// Install / Update / Enable / Retry / ✕.
    Primary,
    /// The `⋯` trigger: handled by its own popover.
    More,
    /// Anywhere else on the row: selects it.
    Row,
}

/// Which part of a row of `width` a point `(x, y)` (row-local) falls on, for a primary button of
/// `action_width`.
fn row_hit(width: f32, x: f32, y: f32, action_width: f32) -> RowHit {
    let from_right = width - x - ROW_PAD;
    let top = (ROW_HEIGHT - BUTTON_HEIGHT) / 2.0;
    let in_button_band = (top..=top + BUTTON_HEIGHT).contains(&y);
    if from_right < 0.0 || !in_button_band {
        return RowHit::Row;
    }
    if from_right < MORE_WIDTH {
        RowHit::More
    } else if (MORE_WIDTH + ACTION_GAP..MORE_WIDTH + ACTION_GAP + action_width).contains(&from_right)
    {
        RowHit::Primary
    } else {
        RowHit::Row
    }
}

/// How many of `chips` fit in `available` px; the rest become one "+N" chip.
/// Chip widths are estimated from the text (11 px type, 6 px padding).
fn visible_chip_count(chips: &[&str], available: f32) -> usize {
    const GAP: f32 = 4.0;
    const MORE_CHIP: f32 = 34.0;
    let width = |text: &str| text.chars().count() as f32 * 6.2 + 14.0 + GAP;
    let total: f32 = chips.iter().map(|text| width(text)).sum();
    if total <= available {
        return chips.len();
    }
    let mut used = MORE_CHIP;
    chips
        .iter()
        .take_while(|text| {
            used += width(text);
            used <= available
        })
        .count()
}

/// Brand art for a driver id (the same assets the tree / picker use).
fn driver_icon(id: &str) -> Option<&'static str> {
    Some(match id {
        "postgresql" => "icons/postgres.svg",
        "sqlite" => "icons/sqlite.svg",
        "mongodb" => "icons/mongodb.svg",
        "mysql" => "icons/mysql.svg",
        "mariadb" => "icons/mariadb.svg",
        "clickhouse" => "icons/clickhouse.svg",
        "redis" => "icons/redis.svg",
        _ => return None,
    })
}

fn driver_logo(row: &DriverRow) -> AnyElement {
    if let Some(asset) = driver_icon(&row.id) {
        return gpui::img(asset).w(px(28.0)).h(px(28.0)).into_any_element();
    }
    let initials: String = row.name.chars().take(2).collect();
    div()
        .w(px(28.0))
        .h(px(28.0))
        .rounded(px(6.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(theme::surface())
        .border_1()
        .border_color(theme::border())
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child(initials)
        .into_any_element()
}

pub(super) fn chip(text: impl Into<gpui::SharedString>) -> Div {
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(theme::border())
        .text_size(px(11.0))
        .text_color(theme::text_muted())
        .child(text.into())
}

pub(super) fn badge_element(badge: &Badge) -> Div {
    let (text, color) = match badge {
        Badge::Queued => ("Queued".to_string(), theme::text_muted()),
        Badge::Disabled => ("Disabled".to_string(), theme::warning()),
        Badge::Developer => ("Developer driver".to_string(), theme::danger()),
        Badge::Installed => ("Installed \u{2713}".to_string(), theme::success()),
        Badge::Update => ("Update".to_string(), theme::accent()),
        Badge::Unavailable(reason) => (reason.clone(), theme::warning()),
    };
    div()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .bg(theme::surface())
        .text_size(px(11.0))
        .text_color(color)
        .child(text)
}

impl AppRoot {
    /// Creates the search box on first use and keeps the model's query in step.
    fn ensure_driver_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.driver_ui.search.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search drivers, schemes, capabilities\u{2026}")
        });
        cx.subscribe(
            &input,
            |this: &mut Self,
             input: gpui::Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    let query = input.read(cx).value().to_string();
                    this.driver_ui.model.set_query(&query);
                    cx.notify();
                }
            },
        )
        .detach();
        self.driver_ui.search = Some(input);
    }

    /// Row and part under a window-space point, from the painted list bounds
    /// plus its scroll offset (the scheme of `grid_cell_at`).
    fn driver_row_at(&self, point: gpui::Point<gpui::Pixels>) -> Option<(usize, RowHit)> {
        let (bounds, offset) = {
            let state = self.driver_ui.list_scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        if !bounds.contains(&point) {
            return None;
        }
        let y = f32::from(point.y - bounds.origin.y - offset.y);
        let position = (y / ROW_HEIGHT) as usize;
        let slot = self.addon_slot();
        if y < 0.0 || position >= self.driver_list_len(slot) {
            return None;
        }
        let x = f32::from(point.x - bounds.origin.x);
        // The add-on's button may be wider than a driver's (it can say "Install MongoDB driver first").
        let action_width = match self.driver_list_entry(slot, position) {
            Some(ListEntry::Addon) => self.addon_primary().map_or(ACTION_WIDTH, addon_action_width),
            _ => ACTION_WIDTH,
        };
        Some((
            position,
            row_hit(
                f32::from(bounds.size.width),
                x,
                y - position as f32 * ROW_HEIGHT,
                action_width,
            ),
        ))
    }

    /// Where the MongoDB Shell add-on's row sits: right under the MongoDB driver's row, when the
    /// catalog (or an install) knows the add-on and that row is listed.
    fn addon_slot(&self) -> Option<usize> {
        self.driver_ui.tools.shell_item()?;
        (0..self.driver_ui.model.visible_len())
            .find(|position| {
                self.driver_ui
                    .model
                    .visible_row(*position)
                    .is_some_and(|row| row.id == engine_driver_host::MONGODB_DRIVER_ID)
            })
            .map(|position| position + 1)
    }

    fn driver_list_len(&self, slot: Option<usize>) -> usize {
        self.driver_ui.model.visible_len() + usize::from(slot.is_some())
    }

    /// What the list shows at `position`: a driver (its index in the model's visible rows) or the
    /// add-on. `slot` is [`Self::addon_slot`], found once by the caller (per render, per event).
    fn driver_list_entry(&self, slot: Option<usize>, position: usize) -> Option<ListEntry> {
        match slot {
            Some(slot) if position == slot => Some(ListEntry::Addon),
            Some(slot) if position > slot => Some(ListEntry::Driver(position - 1)),
            _ => (position < self.driver_ui.model.visible_len()).then_some(ListEntry::Driver(position)),
        }
    }

    /// The button the add-on row shows, if any (Remove is in its menu).
    fn addon_primary(&self) -> Option<ToolAction> {
        let item = self.driver_ui.tools.shell_item()?;
        let need = self.addon_need(item);
        addon_actions(item, self.driver_ui.tools.job_of(item.id.as_str()), need.as_ref())
            .into_iter()
            .find(|action| *action != ToolAction::Uninstall)
    }

    fn run_primary_driver_action(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(row) = self.driver_ui.model.row(id) else {
            return;
        };
        let job = self.driver_ui.queue.job(id);
        let stale = self.driver_sessions(id).iter().any(|session| session.stale);
        let Some(action) = row_action(row, job, stale) else {
            return;
        };
        let entry = row.entry.clone();
        // Retry installs the catalog's CURRENT entry, never the one stored with the failed job (the
        // catalog may have been re-read under another policy or the build withdrawn since).
        let offered = entry
            .clone()
            .filter(|_| !matches!(row.status, RowStatus::Unavailable(_)));
        let retry = job.map(|job| (job.kind, offered));
        match (action, entry) {
            (PrimaryAction::Install, Some(entry)) => {
                self.enqueue_driver_install(entry, JobKind::Install, cx);
            }
            (PrimaryAction::Update, Some(entry)) => {
                self.enqueue_driver_install(entry, JobKind::Update, cx);
            }
            (PrimaryAction::Retry, _) => {
                self.driver_ui.queue.dismiss(id);
                if let Some((kind, Some(entry))) = retry {
                    self.enqueue_driver_install(entry, kind, cx);
                }
            }
            (PrimaryAction::Enable, _) => self.set_driver_disabled(id.to_string(), false, cx),
            (PrimaryAction::Cancel, _) => self.cancel_driver_install(id, cx),
            (PrimaryAction::Reconnect, _) => self.reconnect_stale_sessions(id, cx),
            _ => {}
        }
        cx.notify();
    }

    pub(in crate::app) fn render_driver_manager(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_driver_search(window, cx);
        if std::mem::take(&mut self.driver_ui.scenario_prompt_pending) {
            self.open_missing_driver_dialog(cx);
        }
        let sidebar = if self.shell.sidebar_collapsed {
            0.0
        } else {
            self.shell.sidebar_width
        };
        let inner_width = f32::from(window.viewport_size().width) - sidebar - 32.0;
        let has_selection = self
            .driver_ui
            .selected
            .as_deref()
            .is_some_and(|id| self.driver_ui.model.row(id).is_some());
        let show_detail =
            has_selection && inner_width - DETAIL_WIDTH - 12.0 >= LIST_MIN_WIDTH_WITH_DETAIL;
        let list_width = if show_detail {
            inner_width - DETAIL_WIDTH - 12.0
        } else {
            inner_width
        };
        self.driver_ui.list_width = list_width;
        let ui = &self.driver_ui;
        let search = ui.search.clone();

        let toolbar = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .debug_selector(|| "driver-search".into())
                    .flex_1()
                    .children(search.map(|state| Input::new(&state))),
            )
            .child(
                Button::new("driver-check-updates")
                    .debug_selector(|| "driver-check-updates".into())
                    .label("Check for updates")
                    .compact()
                    .loading(ui.catalog_task.is_some())
                    .on_click(cx.listener(|this, _, _, cx| this.refresh_driver_catalog(cx))),
            )
            .child({
                let root = cx.entity().downgrade();
                let developer_mode = ui.developer_mode;
                Button::new("driver-more-actions")
                    .debug_selector(|| "driver-more-actions".into())
                    .label("\u{22EF}")
                    .compact()
                    .ghost()
                    .dropdown_menu(move |menu, _, _| {
                        let root = root.clone();
                        let import_root = root.clone();
                        menu.item(PopupMenuItem::new("Import local driver\u{2026}").on_click(
                            move |_, _, app| {
                                let _ =
                                    import_root.update(app, |this, cx| this.import_local_driver(cx));
                            },
                        ))
                        .item(
                            PopupMenuItem::new("Developer mode")
                                .checked(developer_mode)
                                .on_click(move |_, window, app| {
                                    let _ = root.update(app, |this, cx| {
                                        this.request_developer_mode(window, cx)
                                    });
                                }),
                        )
                    })
            });

        let banner = self.render_driver_banner(cx);
        let developer_banner = self.render_developer_banner();
        let chips = div().flex().flex_wrap().gap(px(6.0)).children(DriverFilter::CHIPS.map(|filter| {
            let active = ui.model.filter() == filter;
            Button::new(filter.selector())
                .debug_selector(move || filter.selector().into())
                .label(format!("{} {}", filter.label(), ui.model.count(filter)))
                .compact()
                .ghost()
                .selected(active)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.driver_ui.model.set_filter(filter);
                    cx.notify();
                }))
        }));

        let error_line = ui.catalog_error.as_ref().map(|error| {
            div()
                .debug_selector(|| "driver-catalog-error".into())
                .text_size(px(12.0))
                .text_color(theme::danger())
                .child(error.clone())
        });
        let body: AnyElement = if ui.model.visible_len() == 0 {
            div()
                .debug_selector(|| "driver-empty".into())
                .text_color(theme::text_muted())
                .child(if !ui.installed_loaded || ui.catalog_task.is_some() {
                    "Loading drivers\u{2026}"
                } else if ui.model.rows().is_empty() {
                    "No drivers available"
                } else {
                    "No drivers match"
                })
                .into_any_element()
        } else {
            self.render_driver_list(cx)
        };

        let detail = show_detail.then(|| self.render_driver_detail(cx));
        let tools = self.render_driver_tools(cx);
        let footer = self.render_driver_footer(cx);
        div()
            .debug_selector(|| "driver-manager".into())
            .size_full()
            .p(px(16.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(toolbar)
            .children(developer_banner)
            .children(banner)
            .child(chips)
            .children(error_line)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .gap(px(12.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .border_t_1()
                            .border_color(theme::border())
                            .child(body),
                    )
                    .children(detail),
            )
            .child(tools)
            .child(footer)
            .into_any_element()
    }

    /// The persistent warning while the trust checks are lowered.
    fn render_developer_banner(&self) -> Option<AnyElement> {
        let ui = &self.driver_ui;
        if ui.policy != TrustPolicy::Developer {
            return None;
        }
        let how = if ui.developer_mode {
            "Turn it off in the \u{22EF} menu."
        } else {
            "It is on because TABLEX_DRIVER_DEV=1 is set."
        };
        Some(
            div()
                .debug_selector(|| "driver-developer-banner".into())
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .border_1()
                .border_color(theme::warning())
                .bg(theme::surface())
                .text_size(px(13.0))
                .child(div().text_color(theme::warning()).child("Developer mode"))
                .child(div().flex_1().min_w_0().truncate().child(format!(
                    "Unsigned local manifests and ad-hoc signed drivers are accepted. {how}"
                )))
                .into_any_element(),
        )
    }

    fn render_driver_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ui = &self.driver_ui;
        let (text, button) = if let Some((finished, total)) = ui.queue.batch_counter() {
            (
                format!("Updating drivers {finished}/{total}"),
                Button::new("driver-cancel-all")
                    .debug_selector(|| "driver-cancel-all".into())
                    .label("Cancel all")
                    .compact()
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_all_driver_installs(cx))),
            )
        } else if ui.updates_available > 0 {
            let names: Vec<String> = ui
                .model
                .updatable()
                .map(|row| format!("{} {}", row.name, row.latest_version.as_deref().unwrap_or("")))
                .collect();
            (
                format!(
                    "{} update{} available \u{00B7} {}",
                    ui.updates_available,
                    if ui.updates_available == 1 { "" } else { "s" },
                    names.join(", ")
                ),
                Button::new("driver-update-all")
                    .debug_selector(|| "driver-update-all".into())
                    .label("Update all")
                    .compact()
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| this.update_all_drivers(cx))),
            )
        } else {
            return None;
        };
        Some(
            div()
                .debug_selector(|| "driver-banner".into())
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(theme::accent_surface())
                .child(div().flex_1().min_w_0().truncate().text_size(px(13.0)).child(text))
                .child(button)
                .into_any_element(),
        )
    }

    fn render_driver_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let slot = self.addon_slot();
        let rows = uniform_list(
            "driver-rows",
            self.driver_list_len(slot),
            cx.processor(move |this, range: Range<usize>, _window, cx| {
                let root = cx.entity().downgrade();
                range
                    .filter_map(|position| match this.driver_list_entry(slot, position)? {
                        ListEntry::Driver(index) => {
                            let row = this.driver_ui.model.visible_row(index)?;
                            Some(this.render_driver_row(index, row, &root))
                        }
                        ListEntry::Addon => this.render_addon_row(position, &root),
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .flex_1()
        .min_h_0()
        .track_scroll(&self.driver_ui.list_scroll);
        div()
            .id("driver-list")
            .debug_selector(|| "driver-list".into())
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    let Some((position, hit)) = this.driver_row_at(event.position) else {
                        return;
                    };
                    let index = match this.driver_list_entry(this.addon_slot(), position) {
                        Some(ListEntry::Driver(index)) => index,
                        Some(ListEntry::Addon) => {
                            if hit == RowHit::Primary {
                                if let Some(action) = this.addon_primary() {
                                    this.run_tool_action_of(
                                        engine_driver_host::MONGO_SHELL_TOOL_ID,
                                        action,
                                        cx,
                                    );
                                }
                            }
                            cx.notify();
                            return;
                        }
                        None => return,
                    };
                    let Some(id) = this
                        .driver_ui
                        .model
                        .visible_row(index)
                        .map(|row| row.id.clone())
                    else {
                        return;
                    };
                    this.driver_ui.selected = Some(id.clone());
                    if hit == RowHit::Primary {
                        this.run_primary_driver_action(&id, cx);
                    }
                    cx.notify();
                }),
            )
            .child(rows)
            .child(Scrollbar::vertical(&self.driver_ui.list_scroll))
            .into_any_element()
    }

    fn render_driver_row(
        &self,
        position: usize,
        row: &DriverRow,
        root: &gpui::WeakEntity<Self>,
    ) -> AnyElement {
        let ui = &self.driver_ui;
        let job = ui.queue.job(&row.id);
        let selected = ui.selected.as_deref() == Some(row.id.as_str());
        let sessions = self.driver_sessions(&row.id);
        let list_width = ui.list_width;
        let state_column = list_width >= LIST_MIN_WIDTH_FOR_STATE_COLUMN;
        let chip_budget = list_width - ROW_FIXED_WIDTH;
        let badge = badge_for(row, job);
        let shown_chips = visible_chip_count(&row.chips, chip_budget);
        let id = row.id.clone();

        let sub = match job.map(|job| &job.state) {
            Some(InstallState::Failed(failure)) => div()
                .debug_selector({
                    let id = id.clone();
                    move || format!("driver-error-{id}")
                })
                .truncate()
                .text_size(px(11.0))
                .text_color(theme::danger())
                .child(format!("{}: {}", failure.code, failure.message)),
            _ => {
                let stale = sessions.iter().filter(|session| session.stale).count();
                let mut text = row.id.clone();
                if stale > 0 {
                    text.push_str(&format!(
                        " \u{00B7} {stale} still on {}",
                        self.stale_session_version(&row.id).unwrap_or_else(|| "an older version".into())
                    ));
                } else if !sessions.is_empty() {
                    text.push_str(&format!(
                        " \u{00B7} {} session{} open",
                        sessions.len(),
                        if sessions.len() == 1 { "" } else { "s" }
                    ));
                }
                match (&badge, state_column) {
                    (Some(badge), false) if stale == 0 => div().flex().child(
                        badge_element(badge).truncate().debug_selector({
                            let id = id.clone();
                            move || format!("driver-badge-{id}")
                        }),
                    ),
                    _ => div().truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(text),
                }
            }
        };

        let middle = match job.map(|job| &job.state) {
            Some(state) if state.is_running() => {
                let (done, total) = match state {
                    InstallState::Downloading { bytes, total } => (*bytes, *total),
                    _ => {
                        let total = row.size_bytes.unwrap_or(0);
                        (total, total)
                    }
                };
                let percent = (state.fraction() * 100.0).round() as u32;
                let step = if job.is_some_and(InstallJob::cancelling) {
                    "Cancelling\u{2026}"
                } else {
                    state.step_label()
                };
                div()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("driver-progress-{id}")
                    })
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(Progress::new(("driver-bar", position)).value(state.fraction() * 100.0))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.0))
                            .text_color(theme::text_muted())
                            .child(format!(
                                "{percent}% \u{00B7} {} \u{00B7} {step}",
                                format_size_pair(done, total)
                            )),
                    )
                    .into_any_element()
            }
            _ => {
                let (current, target) = version_text(row);
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .w(px(112.0))
                            .flex_shrink_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(12.0))
                            .text_color(theme::text())
                            .child(current)
                            .children(target.map(|target| {
                                div()
                                    .text_color(theme::accent())
                                    .child(format!("\u{2192} {target}"))
                            })),
                    )
                    .child(
                        div()
                            .w(px(52.0))
                            .flex_shrink_0()
                            .text_size(px(12.0))
                            .text_color(theme::text_muted())
                            .child(row.size_bytes.map_or(String::new(), format_size)),
                    )
                    .when(state_column, |this| {
                        this.child(
                            div()
                                .w(px(100.0))
                                .flex_shrink_0()
                                .flex()
                                .overflow_hidden()
                                .children(badge.as_ref().map(|badge| {
                                    badge_element(badge).debug_selector({
                                        let id = id.clone();
                                        move || format!("driver-badge-{id}")
                                    })
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .items_center()
                            .gap(px(4.0))
                            .when(chip_budget >= MIN_CHIP_BUDGET, |this| {
                                this.children(row.chips.iter().take(shown_chips).map(|text| chip(*text)))
                                    .children((shown_chips < row.chips.len()).then(|| {
                                        chip(format!("+{}", row.chips.len() - shown_chips))
                                    }))
                            }),
                    )
                    .into_any_element()
            }
        };

        let action = match row_action(row, job, sessions.iter().any(|session| session.stale)) {
            Some(action) => {
                let button = Button::new(("driver-action", position)).compact();
                let button = match action {
                    PrimaryAction::Install => button.label("Install").primary(),
                    PrimaryAction::Update => button.label("Update").primary(),
                    PrimaryAction::Retry => button.label("Retry").primary(),
                    PrimaryAction::Enable => button.label("Enable"),
                    PrimaryAction::Reconnect => button.label("Reconnect"),
                    PrimaryAction::Cancel => button
                        .icon(Icon::default().path("icons/close.svg").size(px(12.0)))
                        .ghost(),
                };
                div()
                    .debug_selector({
                        let id = id.clone();
                        move || format!("driver-action-{id}")
                    })
                    .w(px(ACTION_WIDTH))
                    .flex_shrink_0()
                    .flex()
                    .justify_end()
                    .child(button.w(px(ACTION_WIDTH)))
                    .into_any_element()
            }
            None => div().w(px(ACTION_WIDTH)).flex_shrink_0().into_any_element(),
        };

        let more = if row.is_installed() {
            let (root, id_for_menu) = (root.clone(), id.clone());
            let (disabled, has_previous, can_reinstall) = (
                row.disabled,
                row.record
                    .as_ref()
                    .is_some_and(|record| record.previous_version.is_some()),
                row.entry.is_some(),
            );
            div()
                .w(px(MORE_WIDTH))
                .flex_shrink_0()
                .child(
                    Button::new(("driver-more", position))
                        .debug_selector({
                            let id = id.clone();
                            move || format!("driver-more-{id}")
                        })
                        .label("\u{22EF}")
                        .compact()
                        .ghost()
                        .w(px(MORE_WIDTH))
                        .dropdown_menu(move |menu, _, _| {
                            driver_menu(menu, &root, &id_for_menu, disabled, has_previous, can_reinstall)
                        }),
                )
                .into_any_element()
        } else {
            div().w(px(MORE_WIDTH)).flex_shrink_0().into_any_element()
        };

        div()
            .debug_selector({
                let id = id.clone();
                move || format!("driver-row-{id}")
            })
            .w_full()
            .h(px(ROW_HEIGHT))
            .px(px(ROW_PAD))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme::border())
            .when(selected, |this| this.bg(theme::selected()))
            .child(driver_logo(row))
            .child(
                div()
                    .w(px(140.0))
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(px(13.0))
                            .text_color(theme::text())
                            .child(row.name.clone()),
                    )
                    .child(sub),
            )
            .child(middle)
            .child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(ACTION_GAP))
                    .child(action)
                    .child(more),
            )
            .into_any_element()
    }

    fn render_driver_detail(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.driver_ui;
        let Some(row) = ui.selected.as_deref().and_then(|id| ui.model.row(id)) else {
            return div().into_any_element();
        };
        let id = row.id.clone();
        let sessions = self.driver_sessions(&row.id);
        let stale = sessions.iter().filter(|session| session.stale).count();
        let kv = |label: &'static str, value: String| {
            div()
                .flex()
                .gap(px(8.0))
                .text_size(px(12.0))
                .child(div().w(px(76.0)).flex_shrink_0().text_color(theme::text_muted()).child(label))
                .child(div().flex_1().min_w_0().text_color(theme::text()).child(value))
        };
        let record = row.record.as_ref();
        let active_line = match (&row.installed_version, stale) {
            (Some(version), 0) => format!("{version} active"),
            (Some(version), n) => format!(
                "{version} active for new connections \u{00B7} {n} session{} still on {}",
                if n == 1 { "" } else { "s" },
                self.stale_session_version(&row.id).unwrap_or_else(|| "an older version".into())
            ),
            (None, _) => "Not installed".to_string(),
        };
        div()
            .debug_selector(|| "driver-detail".into())
            .w(px(DETAIL_WIDTH))
            .flex_shrink_0()
            .h_full()
            .min_h_0()
            .overflow_y_scrollbar()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(theme::border())
            .child(div().text_size(px(15.0)).text_color(theme::text()).child(row.name.clone()))
            .child(
                div()
                    .debug_selector(|| "driver-detail-active".into())
                    .text_size(px(12.0))
                    .text_color(theme::text_muted())
                    .child(active_line),
            )
            .when(stale > 0, |this| {
                let id = id.clone();
                this.child(
                    Button::new("driver-reconnect-stale")
                        .debug_selector(|| "driver-reconnect-stale".into())
                        .label("Reconnect them")
                        .compact()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.reconnect_stale_sessions(&id, cx);
                        })),
                )
            })
            .children(row.entry.as_ref().and_then(|entry| entry.description.clone()).map(|text| {
                div().text_size(px(12.0)).text_color(theme::text_muted()).child(text)
            }))
            .child(kv("Size", row.size_bytes.map_or("\u{2014}".into(), format_size)))
            .child(kv(
                "Category",
                row.category.map_or("\u{2014}".into(), |category| {
                    DriverFilter::Category(category).label().to_string()
                }),
            ))
            .child(kv(
                "ABI",
                record
                    .map(|record| record.app_abi_version)
                    .or(row.entry.as_ref().map(|entry| entry.app_abi_version))
                    .map_or("\u{2014}".into(), |abi| abi.to_string()),
            ))
            .child(kv(
                "Signed by",
                match record.map(|record| record.team_id.as_str()).or(row.entry.as_ref().map(|e| e.team_id.as_str())) {
                    Some("") => "none (developer build)".into(),
                    Some(team) => format!("Team {team}"),
                    None => "\u{2014}".into(),
                },
            ))
            .child(kv(
                "License",
                record
                    .map(|record| record.license.clone())
                    .or(row.entry.as_ref().map(|entry| entry.license.clone()))
                    .unwrap_or_else(|| "\u{2014}".into()),
            ))
            .child(kv(
                "Source",
                record
                    .map(|record| record.source_repo.clone())
                    .or(row.entry.as_ref().map(|entry| entry.source_repo.clone()))
                    .unwrap_or_else(|| "\u{2014}".into()),
            ))
            .children(record.map(|record| kv("Installed", record.installed_at.clone())))
            .children(record.map(|record| kv("Path", record.path.display().to_string())))
            .children(
                record
                    .and_then(|record| record.previous_version.clone())
                    .map(|previous| kv("Previous", previous)),
            )
            .child(kv(
                "Capabilities",
                if row.features.is_empty() { "\u{2014}".into() } else { row.features.join(", ") },
            ))
            .children(
                row.entry
                    .as_ref()
                    .and_then(|entry| entry.release_notes_url.clone())
                    .map(|url| {
                        Button::new("driver-release-notes")
                            .label("Release notes \u{2197}")
                            .compact()
                            .link()
                            .on_click(move |_, _, cx| cx.open_url(&url))
                    }),
            )
            .into_any_element()
    }

    fn render_driver_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.driver_ui;
        div()
            .debug_selector(|| "driver-footer".into())
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.0))
            .text_color(theme::text_muted())
            .child(
                div().flex_1().truncate().child(match &ui.op_status {
                    Some(status) => status.clone(),
                    None => format!(
                        "Installed: {} \u{00B7} Download cache: {}",
                        format_size(ui.storage.installed_bytes),
                        format_size(ui.storage.cache_bytes)
                    ),
                }),
            )
            .child(
                Button::new("driver-clear-cache")
                    .debug_selector(|| "driver-clear-cache".into())
                    .label(format!("Clear download cache ({})", format_size(ui.storage.cache_bytes)))
                    .compact()
                    .ghost()
                    .disabled(
                        ui.storage.cache_bytes == 0
                            || ui.queue.active_count() > 0
                            || ui.op_task.is_some(),
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.clear_driver_download_cache(cx))),
            )
            .into_any_element()
    }
}

/// The `⋯` menu of an installed driver.
fn driver_menu(
    menu: PopupMenu,
    root: &gpui::WeakEntity<AppRoot>,
    id: &str,
    disabled: bool,
    has_previous: bool,
    can_reinstall: bool,
) -> PopupMenu {
    type MenuAction = Rc<dyn Fn(&mut AppRoot, &mut Window, &mut Context<AppRoot>)>;
    let item = |label: &'static str, run: MenuAction| {
        let root = root.clone();
        PopupMenuItem::new(label).on_click(move |_, window, app| {
            let _ = root.update(app, |this, cx| run(this, window, cx));
        })
    };
    let (toggle, remove, reinstall, rollback, reveal, copy) = (id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string(), id.to_string());
    menu.item(if disabled {
        item("Enable", Rc::new(move |this, _, cx| this.set_driver_disabled(toggle.clone(), false, cx)))
    } else {
        item("Disable", Rc::new(move |this, window, cx| this.request_driver_removal(toggle.clone(), false, window, cx)))
    })
    .item(item("Uninstall", Rc::new(move |this, window, cx| this.request_driver_removal(remove.clone(), true, window, cx))))
    .item(
        item("Reinstall", Rc::new(move |this, _, cx| {
            let entry = this.driver_ui.model.row(&reinstall).and_then(|row| row.entry.clone());
            if let Some(entry) = entry {
                this.enqueue_driver_install(entry, JobKind::Reinstall, cx);
            }
        }))
        .disabled(!can_reinstall),
    )
    .item(
        item("Roll back to previous version", Rc::new(move |this, window, cx| this.request_driver_rollback(rollback.clone(), window, cx)))
            .disabled(!has_previous),
    )
    .separator()
    .item(item("Show in Finder", Rc::new(move |this, _, cx| this.reveal_driver(&reveal, cx))))
    .item(item("Copy details", Rc::new(move |this, _, cx| this.copy_driver_details(&copy, cx))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_hits_map_the_fixed_action_slots_from_the_right_edge() {
        let width = 800.0;
        let mid_y = ROW_HEIGHT / 2.0;
        // Rightmost slot is the `⋯` trigger, left of it the primary button.
        assert_eq!(row_hit(width, width - ROW_PAD - 5.0, mid_y, ACTION_WIDTH), RowHit::More);
        let primary_x = width - ROW_PAD - MORE_WIDTH - ACTION_GAP - ACTION_WIDTH / 2.0;
        assert_eq!(row_hit(width, primary_x, mid_y, ACTION_WIDTH), RowHit::Primary);
        // The gap, the right padding and everything left of the button select the row.
        assert_eq!(row_hit(width, width - ROW_PAD - MORE_WIDTH - 2.0, mid_y, ACTION_WIDTH), RowHit::Row);
        assert_eq!(row_hit(width, width - 4.0, mid_y, ACTION_WIDTH), RowHit::Row);
        assert_eq!(row_hit(width, 100.0, mid_y, ACTION_WIDTH), RowHit::Row);
        // Above / below the button band is the row, not the button.
        assert_eq!(row_hit(width, primary_x, 2.0, ACTION_WIDTH), RowHit::Row);
        assert_eq!(row_hit(width, primary_x, ROW_HEIGHT - 2.0, ACTION_WIDTH), RowHit::Row);
    }

    #[test]
    fn chips_that_do_not_fit_collapse_into_a_plus_n() {
        let chips = ["navigator", "sql", "arrow", "cancel", "edit"];
        assert_eq!(visible_chip_count(&chips, 1000.0), 5, "all fit");
        assert_eq!(visible_chip_count(&chips, 0.0), 0, "no room: only the +N chip");
        let some = visible_chip_count(&chips, 200.0);
        assert!(some > 0 && some < 5, "{some}");
        assert_eq!(visible_chip_count(&[], 10.0), 0);
    }

    #[test]
    fn every_catalog_driver_has_brand_art_or_a_monogram() {
        for id in ["postgresql", "mysql", "mariadb", "sqlite", "mongodb", "redis", "clickhouse"] {
            let asset = driver_icon(id).unwrap_or_else(|| panic!("{id} has art"));
            assert!(crate::assets::asset(asset).is_some(), "{asset} is registered");
        }
        assert_eq!(driver_icon("unknown-db"), None);
    }
}
