//! `ListDelegate` of the palette: turns the query into sections of row
//! references (ranking happens in `matcher`) and renders only the rows the
//! virtualized list asks for.

use super::super::tab_bar::TabIcon;
use super::super::AppRoot;
use super::index::{Command, Index, Kind};
use super::matcher::{highlight_ranges, Hit, Query, Ranker};
use super::{SEG_ACTIONS, SEG_CONNECTIONS, SEG_TABS};
use crate::navigator::NavigatorNodeId;
use crate::theme;
use gpui::prelude::FluentBuilder;
use gpui::{
    div, px, Context, FontWeight, HighlightStyle, InteractiveElement, IntoElement, ParentElement,
    SharedString,
    StyledText, Styled, Task, WeakEntity, Window,
};
use gpui_component::list::{ListDelegate, ListItem, ListState};
use gpui_component::{IndexPath, WindowExt as _};
use std::sync::Arc;

/// Rows kept per keystroke; the rest of a huge match set is never shown.
pub(super) const RESULT_LIMIT: usize = 64;
const IDLE_TABS: usize = 8;
const IDLE_CONNECTIONS: usize = 8;
const ROW_HEIGHT: f32 = 32.0;
const HEADER_HEIGHT: f32 = 26.0;
/// Search field strip above the rows (input + border).
const INPUT_HEIGHT: f32 = 40.0;
/// The compact "No matching items" body.
const EMPTY_HEIGHT: f32 = 44.0;
const MAX_PANEL_HEIGHT: f32 = 440.0;
/// An activated or remembered item, independent of any index snapshot.
#[derive(Clone)]
pub(super) struct Recent {
    pub(super) kind: Kind,
    pub(super) title: SharedString,
    pub(super) detail: SharedString,
    pub(super) icon: TabIcon,
    pub(super) command: Command,
}

#[derive(Clone, Copy)]
enum Row {
    Entry { segment: u8, entry: u32 },
    Recent(u32),
}

struct Section {
    title: &'static str,
    rows: Vec<Row>,
}

pub(super) struct PaletteDelegate {
    root: WeakEntity<AppRoot>,
    index: Arc<Index>,
    recent: Vec<Recent>,
    query: Query,
    ranker: Ranker,
    hits: Vec<Hit>,
    sections: Vec<Section>,
    selected: Option<IndexPath>,
}

impl PaletteDelegate {
    pub(super) fn new(root: WeakEntity<AppRoot>) -> Self {
        Self {
            root,
            index: Arc::default(),
            recent: Vec::new(),
            query: Query::parse(""),
            ranker: Ranker::default(),
            hits: Vec::new(),
            sections: Vec::new(),
            selected: None,
        }
    }

    /// Installs a fresh index snapshot + recent list (called on every open).
    pub(super) fn load(&mut self, index: Arc<Index>, recent: Vec<Recent>) {
        self.index = index;
        self.recent = recent;
        self.query = Query::parse("");
        self.refresh();
    }

    /// Height that fits the search field plus the current sections / rows
    /// (all heights are fixed), capped like the old fixed panel.
    pub(super) fn panel_height(&self) -> f32 {
        let body = if self.sections.is_empty() {
            EMPTY_HEIGHT
        } else {
            self.sections
                .iter()
                .map(|section| HEADER_HEIGHT + section.rows.len() as f32 * ROW_HEIGHT)
                .sum()
        };
        (INPUT_HEIGHT + body).min(MAX_PANEL_HEIGHT)
    }

    fn has_rows(&self) -> bool {
        !self.sections.is_empty()
    }

    fn refresh(&mut self) {
        self.sections.clear();
        if self.query.is_empty() {
            self.build_idle();
        } else {
            self.build_ranked();
        }
    }

    /// Empty query: Recent, then a short list of what is one keystroke away.
    fn build_idle(&mut self) {
        if !self.recent.is_empty() {
            self.sections.push(Section {
                title: "Recent",
                rows: (0..self.recent.len() as u32).map(Row::Recent).collect(),
            });
        }
        for (segment, title, cap) in [
            (SEG_TABS, "Open Tabs", IDLE_TABS),
            (SEG_CONNECTIONS, "Connections", IDLE_CONNECTIONS),
            (SEG_ACTIONS, "Actions", usize::MAX),
        ] {
            let Some(source) = self.index.segments.get(segment) else {
                continue;
            };
            let rows: Vec<Row> = (0..source.entries.len().min(cap))
                .map(|entry| Row::Entry {
                    segment: segment as u8,
                    entry: entry as u32,
                })
                .collect();
            if !rows.is_empty() {
                self.sections.push(Section { title, rows });
            }
        }
    }

    /// Typed query: best matches overall, grouped by kind; sections are
    /// ordered by their best hit so the top result is always first.
    fn build_ranked(&mut self) {
        self.ranker
            .top(&self.index, &self.query, RESULT_LIMIT, &mut self.hits);
        for hit in &self.hits {
            let kind = self.index.segments[usize::from(hit.segment)].entries[hit.entry as usize].kind;
            let title = kind.section();
            let row = Row::Entry {
                segment: hit.segment,
                entry: hit.entry,
            };
            match self.sections.iter_mut().find(|section| section.title == title) {
                Some(section) => section.rows.push(row),
                None => self.sections.push(Section {
                    title,
                    rows: vec![row],
                }),
            }
        }
    }

    fn item(&self, row: Row) -> Option<Recent> {
        match row {
            Row::Recent(ix) => self.recent.get(ix as usize).cloned(),
            Row::Entry { segment, entry } => {
                let segment = self.index.segments.get(usize::from(segment))?;
                let entry = segment.entries.get(entry as usize)?;
                Some(Recent {
                    kind: entry.kind,
                    title: SharedString::from(entry.title.to_string()),
                    detail: SharedString::from(segment.detail(entry).to_string()),
                    icon: segment.icon(entry),
                    command: segment.command(entry),
                })
            }
        }
    }

    /// `(section title, row titles)` as currently shown.
    #[cfg(test)]
    pub(super) fn snapshot(&self) -> Vec<(&'static str, Vec<String>)> {
        self.sections
            .iter()
            .map(|section| {
                let titles = section
                    .rows
                    .iter()
                    .filter_map(|row| self.item(*row))
                    .map(|item| item.title.to_string())
                    .collect();
                (section.title, titles)
            })
            .collect()
    }

    fn row_at(&self, ix: IndexPath) -> Option<Row> {
        self.sections.get(ix.section)?.rows.get(ix.row).copied()
    }

    /// Title ranges to highlight (only ranked rows have any).
    fn title_ranges(&self, row: Row) -> Vec<std::ops::Range<usize>> {
        match row {
            Row::Entry { segment, entry } if !self.query.is_empty() => self
                .index
                .segments
                .get(usize::from(segment))
                .and_then(|segment| segment.entries.get(entry as usize))
                .map(|entry| highlight_ranges(entry, &self.query))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

fn badge(kind: Kind) -> impl IntoElement {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(theme::border())
        .text_size(px(10.0))
        .text_color(theme::text_faint())
        .child(kind.badge())
}

impl ListDelegate for PaletteDelegate {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = Query::parse(query);
        self.refresh();
        // The list selects against its previous (possibly empty) row cache;
        // make sure a fresh result set always has its first row highlighted.
        cx.defer_in(window, |state, window, cx| {
            if state.selected_index().is_none() && state.delegate().has_rows() {
                state.set_selected_index(Some(IndexPath::default()), window, cx);
            }
        });
        Task::ready(())
    }

    fn sections_count(&self, _: &gpui::App) -> usize {
        self.sections.len().max(1)
    }

    fn items_count(&self, section: usize, _: &gpui::App) -> usize {
        self.sections.get(section).map_or(0, |section| section.rows.len())
    }

    fn render_section_header(
        &mut self,
        section: usize,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<impl IntoElement> {
        let title = self.sections.get(section)?.title;
        Some(
            div()
                .h(px(HEADER_HEIGHT))
                .px_3()
                .pt(px(6.0))
                .text_size(px(11.0))
                .text_color(theme::text_faint())
                .child(title),
        )
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        let row = self.row_at(ix)?;
        let (kind, title, detail, icon) = match row {
            Row::Recent(recent) => {
                let recent = self.recent.get(recent as usize)?;
                (recent.kind, recent.title.clone(), recent.detail.clone(), recent.icon)
            }
            Row::Entry { segment, entry } => {
                let segment = self.index.segments.get(usize::from(segment))?;
                let entry = segment.entries.get(entry as usize)?;
                (
                    entry.kind,
                    SharedString::from(entry.title.to_string()),
                    SharedString::from(segment.detail(entry).to_string()),
                    segment.icon(entry),
                )
            }
        };
        let ranges = self.title_ranges(row);
        let highlight = HighlightStyle {
            color: Some(theme::accent().into()),
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
        };
        let label = StyledText::new(title)
            .with_highlights(ranges.into_iter().map(|range| (range, highlight)));
        // Secondary actions of a database / schema, shown on the highlighted row.
        let hint = self.selected == Some(ix) && matches!(kind, Kind::Database | Kind::Schema);
        let (section, line) = (ix.section, ix.row);
        Some(
            ListItem::new(("palette-row", (section * 1000 + line) as u64))
                .debug_selector(move || format!("palette-row-{section}-{line}"))
                .h(px(ROW_HEIGHT))
                .py_0()
                .px_3()
                .rounded(px(6.0))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(13.0))
                        .text_color(theme::text())
                        .child(div().flex_none().child(icon.element(14.0, theme::text_muted())))
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(300.0))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(label),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(12.0))
                                .text_color(theme::text_muted())
                                .when(!detail.is_empty(), |this| this.child(detail)),
                        )
                        .when(hint, |this| {
                            this.child(
                                div()
                                    .flex_none()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_faint())
                                    .child(crate::platform::PALETTE_NODE_HINT),
                            )
                        })
                        .child(badge(kind)),
                ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        div()
            .debug_selector(|| "palette-empty".into())
            .w_full()
            .h(px(EMPTY_HEIGHT))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(13.0))
            .text_color(theme::text_muted())
            .child("No matching items")
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        let how = if secondary { Activation::Secondary } else { Activation::Primary };
        self.activate(how, window, cx);
    }
}

/// How the selected row is activated.
#[derive(Clone, Copy)]
pub(super) enum Activation {
    /// Enter / click.
    Primary,
    /// ⌘↵ (Ctrl+Enter elsewhere): the ER diagram of a database or schema.
    Secondary,
    /// Shift+Enter: a New Query scoped to a database or schema.
    NewQuery,
}

impl PaletteDelegate {
    pub(super) fn activate(
        &mut self,
        how: Activation,
        window: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        let Some(item) = self
            .selected
            .and_then(|ix| self.row_at(ix))
            .and_then(|row| self.item(row))
        else {
            return;
        };
        let item = match (&item.command, how) {
            (
                Command::Node(node @ (NavigatorNodeId::Database { .. } | NavigatorNodeId::Schema { .. })),
                Activation::Secondary,
            ) => Recent { command: Command::Diagram(node.clone()), ..item },
            (Command::Node(node), Activation::NewQuery)
                if matches!(item.kind, Kind::Database | Kind::Schema) =>
            {
                Recent { command: Command::NewQuery(node.clone()), ..item }
            }
            _ => item,
        };
        let root = self.root.clone();
        // Close first: activation may open its own dialog (New Connection).
        window.close_dialog(cx);
        window.defer(cx, move |window, cx| {
            let _ = root.update(cx, |app, cx| app.activate_palette_item(item, window, cx));
        });
    }
}
