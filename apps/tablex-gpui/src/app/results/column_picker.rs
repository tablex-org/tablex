//! Shared "Columns k/N" popover: a searchable checkbox list that shows or
//! hides result columns in the grid (display level only: the display->source
//! map `GridUiState::visible_columns`; nothing is re-queried).
//!
//! Used by the Mongo toolbar (Visible / Projection tabs) and the SQL table
//! bar. The owner supplies the data (`ColumnPicker`), the optional tab switch
//! and footer, and the toggle / open callbacks; layout, search, virtualized
//! rows and the selector names (`{prefix}-panel`, `{prefix}-row-{i}`, ...)
//! live here so both toolbars look and behave alike.

use super::super::*;
use std::rc::Rc;

/// "Columns k/N" label of the toolbar button.
pub(in crate::app) fn columns_button_label(visible: usize, total: usize) -> String {
    format!("Columns {visible}/{total}")
}

pub(in crate::app) struct ColumnPicker {
    /// Selector / element-id prefix (`mongo-column`, `sql-column`).
    pub(in crate::app) prefix: &'static str,
    pub(in crate::app) tab_id: u64,
    /// Every column, in result order.
    pub(in crate::app) columns: Vec<String>,
    /// Checked columns.
    pub(in crate::app) selected: HashSet<String>,
    /// Columns drawn dimmed and unchecked (Mongo projection: not projected).
    pub(in crate::app) unavailable: HashSet<String>,
    pub(in crate::app) search: String,
    pub(in crate::app) open: bool,
    /// Rows ignore clicks (SQL: staged changes pin the display mapping).
    pub(in crate::app) locked: bool,
    /// Segmented control under the title (Mongo).
    pub(in crate::app) tabs: Option<AnyElement>,
    pub(in crate::app) footer: AnyElement,
    pub(in crate::app) trigger: Button,
    pub(in crate::app) search_editor: gpui::Entity<InputState>,
}

type ToggleFn = Rc<dyn Fn(&mut AppRoot, String, &mut Context<AppRoot>)>;
type OpenFn = Rc<dyn Fn(&mut AppRoot, bool, &mut Context<AppRoot>)>;

impl AppRoot {
    pub(in crate::app) fn render_column_picker(
        &mut self,
        picker: ColumnPicker,
        on_toggle: impl Fn(&mut AppRoot, String, &mut Context<AppRoot>) + 'static,
        on_open_change: impl Fn(&mut AppRoot, bool, &mut Context<AppRoot>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ColumnPicker {
            prefix,
            tab_id,
            columns,
            selected,
            unavailable,
            search,
            open,
            locked,
            tabs,
            footer,
            trigger,
            search_editor,
        } = picker;
        let on_toggle: ToggleFn = Rc::new(on_toggle);
        let on_open_change: OpenFn = Rc::new(on_open_change);
        let name = |suffix: &str| format!("{prefix}-{suffix}");
        let element_id = |suffix: &str| gpui::ElementId::from(gpui::SharedString::from(name(suffix)));
        let selected_count = columns
            .iter()
            .filter(|column| selected.contains(*column))
            .count();
        let search = search.to_ascii_lowercase();
        let filtered = columns
            .iter()
            .filter(|column| search.is_empty() || column.to_ascii_lowercase().contains(&search))
            .cloned()
            .collect::<Vec<_>>();
        let total = columns.len();

        let search_box = div()
            .debug_selector({
                let selector = name("search");
                move || selector.clone()
            })
            .h(px(28.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme::border())
            .bg(theme::background())
            .child(Icon::new(IconName::Search).size(px(12.0)))
            .child(
                div().flex_1().min_w(px(0.0)).child(
                    Input::new(&search_editor)
                        .appearance(false)
                        .with_size(gpui_component::Size::XSmall),
                ),
            );

        let row_prefix = prefix;
        let row_toggle = on_toggle.clone();
        let list = uniform_list(
            gpui::ElementId::from((element_id("list"), tab_id.to_string())),
            filtered.len(),
            cx.processor(move |_this, range: Range<usize>, _window, cx| {
                range
                    .map(|index| {
                        let column = filtered[index].clone();
                        let available = !unavailable.contains(&column);
                        let checked = selected.contains(&column) && available;
                        let click_name = column.clone();
                        let toggle = row_toggle.clone();
                        div()
                            .id((
                                gpui::ElementId::from(gpui::SharedString::from(format!(
                                    "{row_prefix}-row-{tab_id}"
                                ))),
                                index.to_string(),
                            ))
                            .debug_selector(move || format!("{row_prefix}-row-{index}"))
                            .w_full()
                            .h(px(28.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .when(!available || locked, |this| this.opacity(0.5))
                            .child(
                                Checkbox::new((
                                    gpui::ElementId::from(gpui::SharedString::from(format!(
                                        "{row_prefix}-checkbox-{tab_id}"
                                    ))),
                                    index.to_string(),
                                ))
                                .debug_selector(move || format!("{row_prefix}-checkbox-{index}"))
                                .with_size(gpui_component::Size::XSmall)
                                .checked(checked)
                                .tab_stop(false),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_color(if available {
                                        theme::text()
                                    } else {
                                        theme::text_muted()
                                    })
                                    .text_size(px(12.0))
                                    .child(column),
                            )
                            .hover(|style| style.bg(theme::hover()))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !locked {
                                    toggle(this, click_name.clone(), cx);
                                }
                            }))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .debug_selector({
            let selector = name("list");
            move || selector.clone()
        })
        .h(px(230.0))
        .p(px(8.0))
        .track_scroll(&self.view(tab_id).column_picker_scroll);

        let header = div()
            .debug_selector({
                let selector = name("header");
                move || selector.clone()
            })
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(12.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_size(px(13.0)).child("Columns"))
                    .child(
                        div()
                            .debug_selector({
                                let selector = name("count");
                                move || selector.clone()
                            })
                            .text_size(px(10.0))
                            .text_color(theme::text_muted())
                            .child(format!("{selected_count}/{total}")),
                    ),
            )
            .children(tabs)
            .child(search_box);
        let panel = div()
            .id((element_id("panel"), tab_id.to_string()))
            .debug_selector({
                let selector = name("panel");
                move || selector.clone()
            })
            .w(px(320.0))
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(theme::surface())
            .child(header)
            .child(div().h(px(1.0)).w_full().bg(theme::border()))
            .child(list)
            .child(footer);
        let root = cx.entity().downgrade();
        Popover::new((element_id("popover"), tab_id.to_string()))
            .trigger(trigger)
            .open(open)
            .p(px(0.0))
            .bg(theme::surface())
            .on_open_change(move |open, _, app| {
                let on_open_change = on_open_change.clone();
                let open = *open;
                let _ = root.update(app, |this, cx| on_open_change(this, open, cx));
            })
            .child(panel)
            .into_any_element()
    }
}

/// Footer shell shared by the pickers: hint on the left, actions on the right.
pub(in crate::app) fn column_picker_footer(
    prefix: &'static str,
    hint: &'static str,
    actions: impl IntoElement,
) -> AnyElement {
    div()
        .debug_selector(move || format!("{prefix}-footer"))
        .flex()
        .items_center()
        .justify_between()
        .px(px(12.0))
        .py(px(10.0))
        .border_t_1()
        .border_color(theme::border())
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(px(10.0))
                .text_color(theme::text_muted())
                .child(hint),
        )
        .child(actions)
        .into_any_element()
}

impl AppRoot {
    /// Search box of a column picker. `on_change` stores the text in the
    /// owner's state; the caller caches the entity per tab.
    pub(in crate::app) fn new_column_search_input(
        tab_id: u64,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_change: fn(&mut AppRoot, u64, String),
    ) -> gpui::Entity<InputState> {
        let editor = cx.new(|cx| InputState::new(window, cx).default_value(initial));
        let source = editor.clone();
        cx.subscribe(
            &editor,
            move |this: &mut Self,
                  _: gpui::Entity<InputState>,
                  event: &gpui_component::input::InputEvent,
                  cx: &mut Context<Self>| {
                if matches!(event, gpui_component::input::InputEvent::Change) {
                    on_change(this, tab_id, source.read(cx).value().to_string());
                    cx.notify();
                }
            },
        )
        .detach();
        editor
    }
}
