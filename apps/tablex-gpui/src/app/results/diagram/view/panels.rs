//! The toolbar buttons of D2 and the side dock: relationship suggestions,
//! layers (groups) and the inspector of the selected table.
//!
//! Panels are plain elements in a scrolling column (not part of the canvas):
//! their rows exist only while a panel is open and are capped, so a large
//! schema never builds thousands of rows. The inspector is built from
//! [`InspectorColumn`] rows so that D3 can swap a cell for an editor without
//! touching the layout.

use super::annotate::Panel;
use super::*;
use crate::app::results::diagram::infer::{NameMatch, TypeMatch};
use crate::app::results::diagram::model::{DiagramIndex, DiagramEntity};

/// Width of the dock, pixels.
pub(super) const PANEL_WIDTH: f32 = 320.0;
/// Most rows a list panel builds.
pub(super) const MAX_ROWS: usize = 200;

/// What a key marker of an inspector row says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum KeyKind {
    Primary,
    Foreign,
    None,
}

/// One column of the inspector. Read-only in D2; D3 makes the cells editable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) struct InspectorColumn<'a> {
    pub(in crate::app) name: &'a str,
    pub(in crate::app) data_type: &'a str,
    pub(in crate::app) key: KeyKind,
    pub(in crate::app) nullable: bool,
    pub(in crate::app) default: Option<&'a str>,
    pub(in crate::app) comment: Option<&'a str>,
}

/// The inspector rows of `entity`, in column order.
pub(in crate::app) fn inspector_columns(entity: &DiagramEntity) -> Vec<InspectorColumn<'_>> {
    entity
        .columns
        .iter()
        .map(|c| InspectorColumn {
            name: &c.name,
            data_type: &c.data_type,
            key: if c.primary_key {
                KeyKind::Primary
            } else if c.foreign_key {
                KeyKind::Foreign
            } else {
                KeyKind::None
            },
            nullable: c.nullable,
            default: c.default.as_deref().filter(|d| !d.is_empty()),
            comment: c.comment.as_deref().filter(|d| !d.is_empty()),
        })
        .collect()
}

impl DiagramView {
    /// Opens `panel`, or closes it when it is the open one.
    pub(in crate::app) fn toggle_panel(&mut self, panel: Panel, cx: &mut Context<Self>) {
        self.panel = if self.panel == Some(panel) { None } else { Some(panel) };
        cx.notify();
    }

    /// Undo / redo and the panel toggles at the end of the toolbar, and the
    /// overflow menu that takes over the controls `level` (see
    /// `toolbar_level`) has collapsed.
    pub(super) fn annotate_toolbar(&self, level: usize, cx: &mut Context<Self>) -> Div {
        let icon_button = |id: &'static str, path: &'static str, tooltip: &'static str| {
            Button::new(id)
                .icon(Icon::empty().path(path).size(px(14.0)))
                .with_size(Size::Small)
                .compact()
                .ghost()
                .tooltip(tooltip)
                .debug_selector(move || id.into())
        };
        let toggle = |id: &'static str, path: &'static str, label: String, panel: Panel, tooltip: &'static str, cx: &mut Context<Self>| {
            Button::new(id)
                .icon(Icon::empty().path(path).size(px(14.0)))
                .when(!label.is_empty(), |button| button.label(label))
                .with_size(Size::Small)
                .compact()
                .ghost()
                .selected(self.panel == Some(panel))
                .tooltip(tooltip)
                .debug_selector(move || id.into())
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_panel(panel, cx)))
        };
        let suggestions = self.suggestions.len();
        let separator = || div().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(theme::border());
        div()
            .flex()
            .flex_shrink_0()
            .items_center()
            .gap(px(4.0))
            .children((level < 3).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(separator())
                    .child(
                        icon_button("diagram-undo", "icons/undo-2.svg", "Undo")
                            .disabled(!self.session.history.can_undo())
                            .on_click(cx.listener(|this, _, _, cx| this.undo(cx))),
                    )
                    .child(
                        icon_button("diagram-redo", "icons/redo-2.svg", "Redo")
                            .disabled(!self.session.history.can_redo())
                            .on_click(cx.listener(|this, _, _, cx| this.redo(cx))),
                    )
            }))
            .children((level < 4).then(|| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(separator())
                    .child(toggle(
                        "diagram-suggestions",
                        "icons/list-checks.svg",
                        if suggestions > 0 { suggestions.to_string() } else { String::new() },
                        Panel::Suggestions,
                        "Suggested relationships (inferred from column names)",
                        cx,
                    ))
                    .child(toggle("diagram-layers", "icons/frame.svg", String::new(), Panel::Layers, "Layers: groups of tables", cx))
                    .child(toggle(
                        "diagram-inspector",
                        "icons/panel-right.svg",
                        String::new(),
                        Panel::Inspector,
                        "Inspector: columns and indexes of the selected table",
                        cx,
                    ))
            }))
            .children((level > 0).then(|| self.overflow_button(level, cx)))
    }

    /// The "..." menu holding what the toolbar had no room for. Entries keep
    /// the debug selectors of the buttons they replace.
    fn overflow_button(&self, level: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let (notation, mode, panel) = (self.notation, self.mode, self.panel);
        let (can_undo, can_redo, suggestions) = (self.session.history.can_undo(), self.session.history.can_redo(), self.suggestions.len());
        let changes = self.changes_label();
        Button::new("diagram-overflow")
            .icon(Icon::empty().path("icons/ellipsis.svg").size(px(14.0)))
            .with_size(Size::Small)
            .compact()
            .ghost()
            .tooltip("More: controls that did not fit")
            .debug_selector(|| "diagram-overflow".into())
            .dropdown_menu(move |menu, _, _| {
                // A menu entry with the selector of the toolbar button it stands for.
                let entry = |id: String, label: String| {
                    PopupMenuItem::element(move |_, _| {
                        let (id, label) = (id.clone(), label.clone());
                        div().debug_selector(move || id.clone()).child(label)
                    })
                };
                let mut menu = menu;
                if level >= 1 {
                    let view = view.clone();
                    menu = menu.item(entry("diagram-notation".into(), "Crow's foot".into()).checked(notation == RelationNotation::CrowsFoot).on_click(
                        move |_, _, app| {
                            let _ = view.update(app, |this, cx| this.toggle_notation(cx));
                        },
                    ));
                }
                if level >= 2 {
                    menu = menu.separator();
                    for (index, choice) in AttributeMode::ALL.into_iter().enumerate() {
                        let view = view.clone();
                        menu = menu.item(entry(format!("diagram-mode-{index}"), choice.label().to_string()).checked(mode == choice).on_click(
                            move |_, _, app| {
                                let _ = view.update(app, |this, cx| this.set_mode(choice, cx));
                            },
                        ));
                    }
                }
                if level >= 3 {
                    let (undo, redo) = (view.clone(), view.clone());
                    menu = menu
                        .separator()
                        .item(entry("diagram-undo".into(), "Undo".into()).disabled(!can_undo).on_click(move |_, _, app| {
                            let _ = undo.update(app, |this, cx| this.undo(cx));
                        }))
                        .item(entry("diagram-redo".into(), "Redo".into()).disabled(!can_redo).on_click(move |_, _, app| {
                            let _ = redo.update(app, |this, cx| this.redo(cx));
                        }));
                }
                if level >= 5 {
                    let view = view.clone();
                    menu = menu.separator().item(
                        entry("diagram-changes".into(), changes.clone())
                            .on_click(move |_, window, app| {
                                let _ = view.update(app, |this, cx| this.open_changes(window, cx));
                            }),
                    );
                }
                if level >= 4 {
                    menu = menu.separator();
                    let panels = [
                        ("diagram-suggestions", if suggestions > 0 { format!("Suggestions ({suggestions})") } else { "Suggestions".to_string() }, Panel::Suggestions),
                        ("diagram-layers", "Layers".to_string(), Panel::Layers),
                        ("diagram-inspector", "Inspector".to_string(), Panel::Inspector),
                    ];
                    for (id, label, target) in panels {
                        let view = view.clone();
                        menu = menu.item(entry(id.into(), label).checked(panel == Some(target)).on_click(move |_, _, app| {
                            let _ = view.update(app, |this, cx| this.toggle_panel(target, cx));
                        }));
                    }
                }
                menu
            })
    }

    /// The open panel in its dock, if any.
    pub(super) fn dock(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let panel = self.panel?;
        let (title, body) = match panel {
            Panel::Suggestions => ("Suggested relationships", self.suggestions_panel(cx)),
            Panel::Layers => ("Layers", self.layers_panel(cx)),
            Panel::Inspector => ("Inspector", self.inspector_panel(window, cx)),
        };
        Some(
            div()
                .debug_selector(|| "diagram-dock".into())
                .w(px(PANEL_WIDTH))
                .h_full()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .bg(theme::chrome())
                .border_l_1()
                .border_color(theme::border())
                // Presses inside the dock are not clicks on the canvas.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .h(px(30.0))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .px(px(10.0))
                        .border_b_1()
                        .border_color(theme::border())
                        .text_size(px(12.0))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme::text())
                        .child(title),
                )
                .child(div().id("diagram-dock-body").flex_1().min_h_0().overflow_y_scroll().child(body))
                .into_any_element(),
        )
    }

    fn panel_button(id: &'static str, label: &'static str) -> Button {
        Button::new(id).label(label).with_size(Size::Small).compact().ghost().debug_selector(move || id.into())
    }

    fn panel_note(text: &'static str, selector: &'static str) -> Div {
        div()
            .debug_selector(move || selector.into())
            .p(px(12.0))
            .text_size(px(11.5))
            .text_color(theme::text_muted())
            .child(text)
    }

    // ---- suggestions ----

    fn suggestions_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(graph) = self.snapshot.as_ref().map(|snapshot| snapshot.graph.clone()) else {
            return Self::panel_note("Loading...", "suggestions-empty").into_any_element();
        };
        let ignored = self.annotations.ignored.len();
        let suggestions = self.suggestions.clone();
        let actions = div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(6.0))
            .child(
                Self::panel_button("suggestions-confirm-all", "Confirm all")
                    .disabled(suggestions.is_empty())
                    .tooltip("Add every suggestion as a logical relationship")
                    .on_click(cx.listener(|this, _, _, cx| this.confirm_all_suggestions(cx))),
            )
            .child(
                Self::panel_button("suggestions-ignore-all", "Ignore all")
                    .disabled(suggestions.is_empty())
                    .tooltip("Dismiss every suggestion")
                    .on_click(cx.listener(|this, _, _, cx| this.ignore_all_suggestions(cx))),
            )
            .children((ignored > 0).then(|| {
                Self::panel_button("suggestions-restore", "Restore ignored")
                    .tooltip("Bring the dismissed suggestions back")
                    .on_click(cx.listener(|this, _, _, cx| this.restore_ignored_suggestions(cx)))
            }));
        let rows = suggestions.iter().take(MAX_ROWS).enumerate().map(|(index, s)| {
            let (child, parent) = (&graph.entities[s.child.0], &graph.entities[s.parent.0]);
            let reason = format!(
                "{} {:.0}% · {} name, {} type",
                s.level().label(),
                s.confidence * 100.0,
                match s.name {
                    NameMatch::Table => "table",
                    NameMatch::Singular => "singular/plural",
                },
                match s.types {
                    TypeMatch::Same => "same",
                    TypeMatch::Compatible => "compatible",
                },
            );
            let (child_id, shown) = (s.child, s.confidence);
            let _ = shown;
            div()
                .debug_selector(move || format!("suggestion-{index}"))
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(10.0))
                .py(px(5.0))
                .border_b_1()
                .border_color(theme::divider())
                .child(
                    div()
                        .id(("suggestion-text", index))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.reveal_entity(child_id, cx)))
                        .child(
                            div().truncate().text_size(px(12.0)).text_color(theme::text()).child(format!(
                                "{}.{} → {}.{}",
                                child.label, child.columns[s.child_column].name, parent.label, parent.columns[s.parent_column].name
                            )),
                        )
                        .child(div().truncate().text_size(px(10.5)).text_color(theme::text_faint()).child(reason)),
                )
                .child(
                    Button::new(("suggestion-confirm", index))
                        .icon(Icon::empty().path("icons/check.svg").size(px(13.0)))
                        .with_size(Size::Small)
                        .compact()
                        .ghost()
                        .tooltip("Confirm: add as a logical relationship")
                        .debug_selector(move || format!("suggestion-confirm-{index}"))
                        .on_click(cx.listener(move |this, _, _, cx| this.confirm_suggestion(index, cx))),
                )
                .child(
                    Button::new(("suggestion-ignore", index))
                        .icon(Icon::empty().path("icons/close.svg").size(px(13.0)))
                        .with_size(Size::Small)
                        .compact()
                        .ghost()
                        .tooltip("Ignore this suggestion")
                        .debug_selector(move || format!("suggestion-ignore-{index}"))
                        .on_click(cx.listener(move |this, _, _, cx| this.ignore_suggestion(index, cx))),
                )
        });
        let hidden = suggestions.len().saturating_sub(MAX_ROWS);
        div()
            .flex()
            .flex_col()
            .child(actions)
            .children(suggestions.is_empty().then(|| {
                Self::panel_note(
                    "No suggestions: every column named like <table>_id or <table>Id already has a relationship, or was ignored.",
                    "suggestions-empty",
                )
            }))
            .children(rows)
            .children((hidden > 0).then(|| {
                div().p(px(10.0)).text_size(px(11.0)).text_color(theme::text_faint()).child(format!("{hidden} more not listed"))
            }))
            .into_any_element()
    }

    /// Selects `id` and brings it to the middle of the view.
    pub(in crate::app) fn reveal_entity(&mut self, id: EntityId, cx: &mut Context<Self>) {
        if self.groups.shown(id.0) {
            self.clear_selection_state();
            self.selected = Some(id);
            self.selection.push(id);
            self.center_on_entity(id);
        }
        cx.notify();
    }

    // ---- layers ----

    fn layers_panel(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let groups = self.annotations.groups.clone();
        let selection = self.selection.len();
        let new_group = div().px(px(8.0)).py(px(6.0)).child(
            Self::panel_button("layers-new", "Group selection")
                .disabled(selection == 0)
                .tooltip("Make a group of the selected tables (Shift-click or Shift-drag selects several)")
                .on_click(cx.listener(|this, _, _, cx| {
                    let selection = this.selection.clone();
                    this.group_entities(&selection, cx);
                })),
        );
        let rows = groups.iter().enumerate().map(|(index, group)| {
            let id = group.id;
            let members = self.groups.members.get(index).map_or(0, Vec::len);
            let renaming = self.group_rename.as_ref().filter(|rename| rename.id == id);
            let icon_button = |key: &'static str, path: &'static str, tooltip: &'static str| {
                Button::new((key, index))
                    .icon(Icon::empty().path(path).size(px(13.0)))
                    .with_size(Size::Small)
                    .compact()
                    .ghost()
                    .tooltip(tooltip)
                    .debug_selector(move || format!("{key}-{id}"))
            };
            let name: AnyElement = match renaming {
                Some(rename) => div()
                    .flex_1()
                    .min_w_0()
                    // Escape drops the typed name (Enter or leaving keeps it).
                    .on_action(cx.listener(|this, _: &gpui_component::input::Escape, window, cx| {
                        this.group_rename = None;
                        window.focus(&this.focus_handle, cx);
                        cx.notify();
                    }))
                    .child(Input::new(&rename.state).with_size(Size::Small))
                    .into_any_element(),
                None => div()
                    .id(("layer-name", index))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .cursor_pointer()
                    .text_size(px(12.0))
                    .text_color(if group.hidden { theme::text_faint() } else { theme::text() })
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        this.clear_selection_state();
                        this.selected_group = Some(id);
                        if event.click_count() >= 2 {
                            this.begin_group_rename(id, window, cx);
                        }
                        cx.notify();
                    }))
                    .child(format!("{} ({members})", group.name))
                    .into_any_element(),
            };
            let color = group.rgb().channels();
            div()
                .debug_selector(move || format!("layer-row-{id}"))
                .flex()
                .items_center()
                .gap(px(2.0))
                .px(px(8.0))
                .py(px(3.0))
                .bg(if self.selected_group == Some(id) { theme::selected() } else { gpui::transparent_black().into() })
                .border_b_1()
                .border_color(theme::divider())
                .child(
                    div()
                        .size(px(9.0))
                        .mr(px(4.0))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(gpui::rgb(u32::from(color.0) << 16 | u32::from(color.1) << 8 | u32::from(color.2))),
                )
                .child(name)
                .child(
                    icon_button("layer-visible", if group.hidden { "icons/eye-off.svg" } else { "icons/eye.svg" }, "Show or hide the group")
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_group_hidden(id, cx))),
                )
                .child(
                    icon_button(
                        "layer-collapse",
                        if group.collapsed { "icons/chevron-right.svg" } else { "icons/chevron-down.svg" },
                        "Collapse the group to one chip, or expand it",
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_group_collapsed(id, cx))),
                )
                .child(
                    icon_button("layer-delete", "icons/trash.svg", "Ungroup (the tables stay)")
                        .on_click(cx.listener(move |this, _, _, cx| this.ungroup(id, cx))),
                )
        });
        div()
            .flex()
            .flex_col()
            .child(new_group)
            .children(groups.is_empty().then(|| {
                Self::panel_note(
                    "No groups yet. Select tables (Shift-click, or Shift-drag on empty canvas) and press Group selection.",
                    "layers-empty",
                )
            }))
            .children(rows)
            .into_any_element()
    }

    // ---- inspector ----

    pub(super) fn inspector_read_only(&self) -> AnyElement {
        let entity = self.selected.and_then(|id| self.graph()?.entities.get(id.0));
        let Some(entity) = entity else {
            return Self::panel_note("Select a table to see its columns and indexes.", "inspector-empty").into_any_element();
        };
        let columns = inspector_columns(entity);
        let header = div()
            .debug_selector(|| "inspector-title".into())
            .px(px(10.0))
            .py(px(8.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(div().text_size(px(13.0)).font_weight(gpui::FontWeight::SEMIBOLD).text_color(theme::text()).child(entity.label.clone()))
            .child(div().text_size(px(11.0)).text_color(theme::text_faint()).child(match &entity.state {
                EntityState::Ready => format!("{} columns, {} indexes", columns.len(), entity.indexes.len()),
                EntityState::Loading => "Loading...".to_string(),
                EntityState::Failed(error) => format!("Could not load: {error}"),
            }));
        let section = |title: &'static str| {
            div()
                .px(px(10.0))
                .pt(px(8.0))
                .pb(px(3.0))
                .text_size(px(10.5))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme::text_muted())
                .child(title)
        };
        let column_rows = columns.iter().take(MAX_ROWS).enumerate().map(|(index, column)| {
            let marker = match column.key {
                KeyKind::Primary => Some(("icons/key-round.svg", theme::warning())),
                KeyKind::Foreign => Some(("icons/link.svg", theme::text_faint())),
                KeyKind::None => None,
            };
            let detail = {
                let mut parts = vec![if column.nullable { "NULL" } else { "NOT NULL" }.to_string()];
                if column.key == KeyKind::Primary {
                    parts.push("PK".into());
                }
                if column.key == KeyKind::Foreign {
                    parts.push("FK".into());
                }
                if let Some(default) = column.default {
                    parts.push(format!("default {default}"));
                }
                parts.join(" · ")
            };
            div()
                .debug_selector(move || format!("inspector-column-{index}"))
                .px(px(10.0))
                .py(px(3.0))
                .border_b_1()
                .border_color(theme::divider())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(div().w(px(14.0)).flex_shrink_0().children(
                            marker.map(|(path, color)| Icon::empty().path(path).size(px(11.0)).text_color(color)),
                        ))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.0))
                                .text_color(theme::text())
                                .when(column.key == KeyKind::Primary, |this| this.font_weight(gpui::FontWeight::BOLD))
                                .child(column.name.to_string()),
                        )
                        .child(
                            div()
                                .max_w(px(110.0))
                                .truncate()
                                .text_size(px(10.5))
                                .text_color(theme::text_faint())
                                .child(column.data_type.to_string()),
                        ),
                )
                .child(div().pl(px(18.0)).truncate().text_size(px(10.5)).text_color(theme::text_faint()).child(detail))
                .children(column.comment.map(|comment| {
                    div()
                        .debug_selector(move || format!("inspector-comment-{index}"))
                        .pl(px(18.0))
                        .truncate()
                        .text_size(px(10.5))
                        .italic()
                        .text_color(theme::text_muted())
                        .child(comment.to_string())
                }))
        });
        let index_rows = entity.indexes.iter().take(MAX_ROWS).enumerate().map(|(index, i): (usize, &DiagramIndex)| {
            let mut flags = Vec::new();
            if i.primary {
                flags.push("primary".to_string());
            } else if i.unique {
                flags.push("unique".to_string());
            }
            if let Some(method) = &i.method {
                flags.push(method.clone());
            }
            div()
                .debug_selector(move || format!("inspector-index-{index}"))
                .px(px(10.0))
                .py(px(3.0))
                .border_b_1()
                .border_color(theme::divider())
                .child(div().truncate().text_size(px(12.0)).text_color(theme::text()).child(i.name.clone()))
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(theme::text_faint())
                        .child(format!("({}){}", i.columns.join(", "), if flags.is_empty() { String::new() } else { format!(" · {}", flags.join(" · ")) })),
                )
        });
        div()
            .flex()
            .flex_col()
            .child(header)
            .child(section("COLUMNS"))
            .children(column_rows)
            .child(section("INDEXES"))
            .children(entity.indexes.is_empty().then(|| {
                div().px(px(10.0)).py(px(4.0)).text_size(px(11.0)).text_color(theme::text_faint()).child("No indexes reported")
            }))
            .children(index_rows)
            .into_any_element()
    }
}
