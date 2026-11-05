//! How the Users & Privileges tab looks: the header, the list of roles / accounts, and the three panels of the selected one
//! (Attributes, Membership, Privileges). Everything standard is a `gpui-component` control (`Input`, `Checkbox`, `Select`,
//! `Button`, `TabBar`); the lists are virtualized and hold no control of their own: a row is drawn, and the controls that edit
//! it exist once, in the panel beside the list.

use gpui::{Context, SharedString, Window};
use gpui_component::select::Select;
use gpui_component::tab::{Tab, TabBar};

use super::super::*;
use super::model::Load;
use super::view::{AccountsView, Panel};
use crate::sql_account::catalog::{AccountRow, ReadError};
use crate::sql_account::privilege::Privilege;
use crate::sql_account::{AccountEngine, AccountKind, Attributes, GrantTarget, PasswordChange};

const ROW_HEIGHT: f32 = 26.0;
const LIST_WIDTH: f32 = 300.0;

fn note(selector: &'static str, text: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    div().debug_selector(move || selector.into()).text_size(px(11.5)).text_color(color).child(text.into())
}

fn field(label: &'static str, width: f32, control: impl IntoElement) -> Div {
    div().flex().flex_col().gap(px(3.0)).w(px(width)).child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label)).child(control)
}

fn badge(text: impl Into<SharedString>, color: gpui::Rgba) -> Div {
    div().px(px(5.0)).py(px(1.0)).rounded(px(3.0)).border_1().border_color(color).text_size(px(10.0)).text_color(color).child(text.into())
}

/// A look-alike of a checkbox for a row of a list (the row carries the click; no control is made per row).
fn drawn_box(on: bool) -> Div {
    div()
        .w(px(14.0))
        .h(px(14.0))
        .flex_shrink_0()
        .rounded(px(3.0))
        .border_1()
        .border_color(if on { theme::accent() } else { theme::border_strong() })
        .bg(if on { theme::accent() } else { theme::background() })
        .flex()
        .items_center()
        .justify_center()
        .children(on.then(|| Icon::empty().path("icons/check.svg").size(px(10.0)).text_color(theme::background())))
}

impl AccountsView {
    fn flag(&self, selector: &'static str, label: &'static str, checked: bool, cx: &mut Context<Self>, on: impl Fn(&mut Self, bool, &mut Window, &mut Context<Self>) + 'static) -> impl IntoElement {
        div().self_start().debug_selector(move || selector.into()).child(
            Checkbox::new((selector, self.tab_id)).label(label).checked(checked).on_click(cx.listener(move |this, checked: &bool, window, cx| on(this, *checked, window, cx))),
        )
    }

    // ---- header and footer -------------------------------------------------------------------------------

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let login = self.model.loaded().and_then(|catalog| catalog.login.clone());
        let subtitle = format!(
            "{} \u{00B7} {}{}",
            self.connection_name(cx),
            self.engine.label(),
            login.map(|login| format!(" \u{00B7} logged in as {login}")).unwrap_or_default()
        );
        let noun = self.engine.noun();
        let selected = self.model.selected_row().is_some();
        div()
            .h(px(48.0))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .bg(theme::chrome())
            .border_b_1()
            .border_color(theme::border())
            .child(Icon::empty().path("icons/key-round.svg").size(px(16.0)).text_color(theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().debug_selector(|| "accounts-title".into()).truncate().text_size(px(13.0)).child("Users & Privileges"))
                    .child(div().debug_selector(|| "accounts-subtitle".into()).truncate().text_size(px(11.0)).text_color(theme::text_muted()).child(subtitle)),
            )
            .child(
                Button::new(("accounts-new", tab_id))
                    .debug_selector(|| "accounts-new".into())
                    .label(format!("New {noun}\u{2026}"))
                    .compact()
                    .on_click(cx.listener(|this, _, window, cx| this.open_new_dialog(window, cx))),
            )
            .child(
                Button::new(("accounts-drop", tab_id))
                    .debug_selector(|| "accounts-drop".into())
                    .label(format!("Drop {noun}\u{2026}"))
                    .compact()
                    .ghost()
                    .disabled(!selected)
                    .on_click(cx.listener(|this, _, window, cx| this.open_drop_dialog(window, cx))),
            )
            .child(
                Button::new(("accounts-refresh", tab_id))
                    .debug_selector(|| "accounts-refresh".into())
                    .icon(Icon::empty().path("icons/refresh-cw.svg").size(px(12.0)))
                    .compact()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
            )
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = self.tab_id;
        let pending = self.pending() + usize::from(!self.password.read(cx).value().is_empty() && self.model.detail().is_some_and(|d| d.working.password == PasswordChange::Keep));
        let edits = self.has_edits(cx);
        let message = self.notice.clone().or_else(|| self.field_error.clone().map(|error| (error, true)));
        div()
            .h(px(44.0))
            .flex_shrink_0()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_t_1()
            .border_color(theme::border())
            .bg(theme::chrome())
            .child(
                div()
                    .debug_selector(|| "accounts-message".into())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(if message.as_ref().is_some_and(|(_, failed)| *failed) { theme::danger() } else { theme::text_muted() })
                    .child(match message {
                        Some((text, _)) => text,
                        None if edits => format!("{pending} pending {}. Nothing runs until you review and apply them.", if pending == 1 { "change" } else { "changes" }),
                        None => "Select a role to edit it. Changes are reviewed as SQL before anything runs.".to_owned(),
                    }),
            )
            .child(
                Button::new(("accounts-discard", tab_id))
                    .debug_selector(|| "accounts-discard".into())
                    .label("Discard")
                    .compact()
                    .ghost()
                    .disabled(!edits)
                    .on_click(cx.listener(|this, _, window, cx| this.discard(window, cx))),
            )
            .child(
                Button::new(("accounts-review", tab_id))
                    .debug_selector(|| "accounts-review".into())
                    .label("Review changes")
                    .compact()
                    .primary()
                    .disabled(!edits)
                    .on_click(cx.listener(|this, _, window, cx| this.review(window, cx))),
            )
    }

    // ---- the list ---------------------------------------------------------------------------------------

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let total = self.model.loaded().map_or(0, |catalog| catalog.accounts.len());
        div()
            .debug_selector(|| "accounts-list-pane".into())
            .w(px(LIST_WIDTH))
            .flex_shrink_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .border_r_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "accounts-filter".into()).flex_1().child(Input::new(&self.filter).small()))
                    .child(div().debug_selector(|| "accounts-count".into()).text_size(px(11.0)).text_color(theme::text_muted()).child(format!("{} of {total}", self.model.shown.len()))),
            )
            .child(
                div().debug_selector(|| "accounts-list".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                    uniform_list(
                        "accounts-list-rows",
                        self.model.shown.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            #[cfg(test)]
                            this.drawn.set([(range.start, range.end), this.drawn.get()[1], this.drawn.get()[2]]);
                            range.filter_map(|position| this.list_row(position, cx)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.list_scroll),
                ),
            )
    }

    fn list_row(&self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = self.model.row_at(position)?;
        let selected = self.model.selected.as_ref() == Some(&row.id);
        let dim = row.kind == AccountKind::Role && self.engine.is_mysql();
        let edited = self.model.is_edited(&row.id);
        let flags = self.row_flags(row);
        Some(
            div()
                .id(("accounts-row", position))
                .debug_selector(move || format!("accounts-row-{position}"))
                .h(px(ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .when(selected, |this| this.bg(theme::selected()))
                .hover(|this| this.bg(theme::hover()))
                .on_click(cx.listener(move |this, _, window, cx| this.select_position(position, window, cx)))
                .child(div().w(px(6.0)).h(px(6.0)).flex_shrink_0().rounded(px(3.0)).bg(if edited { theme::accent() } else { gpui::Rgba { a: 0.0, ..theme::accent() } }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(if dim { theme::text_muted() } else { theme::text() })
                        .child(row.id.to_string()),
                )
                .children(flags.into_iter().map(|(text, color)| badge(text, color)))
                .into_any_element(),
        )
    }

    /// The marks on a row of the list: you, superuser, role, locked, cannot log in.
    fn row_flags(&self, row: &AccountRow) -> Vec<(&'static str, gpui::Rgba)> {
        let mut flags = Vec::new();
        if self.is_login(&row.id) {
            flags.push(("you", theme::accent()));
        }
        match &row.attributes {
            Attributes::Postgres(attributes) => {
                if attributes.superuser {
                    flags.push(("superuser", theme::warning()));
                }
                if !attributes.login {
                    flags.push(("no login", theme::text_faint()));
                }
            }
            Attributes::Mysql(attributes) => {
                if row.kind == AccountKind::Role {
                    flags.push(("role", theme::text_faint()));
                } else if attributes.locked {
                    flags.push(("locked", theme::warning()));
                }
            }
        }
        flags
    }

    // ---- the selected account ---------------------------------------------------------------------------

    fn render_detail(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.model.selected_row() else {
            return div()
                .debug_selector(|| "accounts-nothing-selected".into())
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Select a role on the left to see and edit it.")
                .into_any_element();
        };
        let (title, kind) = (row.id.to_string(), row.kind);
        let flags = self.row_flags(row);
        let active = Panel::ALL.iter().position(|panel| *panel == self.panel).unwrap_or(0);
        let tabs = TabBar::new(("accounts-tabs", self.tab_id)).segmented().selected_index(active).on_click(cx.listener(|this, index: &usize, window, cx| {
            if let Some(panel) = Panel::ALL.get(*index) {
                this.set_panel(*panel, window, cx);
            }
        }));
        let tabs = tabs.children(Panel::ALL.iter().map(|panel| {
            let selector = panel.selector();
            Tab::new().label(panel.label()).debug_selector(move || selector.into())
        }));
        let body = match self.panel {
            Panel::Attributes => self.render_attributes(kind, cx),
            Panel::Membership => self.render_membership(cx).into_any_element(),
            Panel::Privileges => self.render_privileges(window, cx).into_any_element(),
        };
        div()
            .debug_selector(|| "accounts-detail".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "accounts-selected".into()).truncate().text_size(px(14.0)).font_weight(gpui::FontWeight::SEMIBOLD).child(title))
                    .children(flags.into_iter().map(|(text, color)| badge(text, color)))
                    .child(div().flex_1())
                    .child(tabs),
            )
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            .into_any_element()
    }

    // ---- Attributes -----------------------------------------------------------------------------------

    fn render_attributes(&self, kind: AccountKind, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = self.model.detail() else { return div().into_any_element() };
        let own = self.model.selected.as_ref().is_some_and(|id| self.is_login(id));
        let panel = div().debug_selector(|| "accounts-attributes".into()).flex_1().min_h_0().overflow_y_scrollbar().flex().flex_col().gap(px(12.0)).pr(px(6.0));
        let panel = panel.children(own.then(|| {
            note("accounts-own-warning", "This is the account this connection logs in as: changing its login, lock or password can lock you out of the server.", theme::warning())
        }));
        match &detail.working.attributes {
            Attributes::Postgres(attributes) => {
                let a = attributes.clone();
                panel
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(18.0))
                            .child(self.flag("accounts-attr-login", "Can log in (LOGIN)", a.login, cx, |this, on, _, cx| this.set_pg_flag(|a| a.login = on, cx)))
                            .child(self.flag("accounts-attr-superuser", "Superuser", a.superuser, cx, |this, on, _, cx| this.set_pg_flag(|a| a.superuser = on, cx)))
                            .child(self.flag("accounts-attr-createdb", "Create databases", a.create_db, cx, |this, on, _, cx| this.set_pg_flag(|a| a.create_db = on, cx)))
                            .child(self.flag("accounts-attr-createrole", "Create roles", a.create_role, cx, |this, on, _, cx| this.set_pg_flag(|a| a.create_role = on, cx)))
                            .child(self.flag("accounts-attr-inherit", "Inherit the privileges of its roles", a.inherit, cx, |this, on, _, cx| this.set_pg_flag(|a| a.inherit = on, cx)))
                            .child(self.flag("accounts-attr-replication", "Replication", a.replication, cx, |this, on, _, cx| this.set_pg_flag(|a| a.replication = on, cx)))
                            .child(self.flag("accounts-attr-bypassrls", "Bypass row-level security", a.bypass_rls, cx, |this, on, _, cx| this.set_pg_flag(|a| a.bypass_rls = on, cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(12.0))
                            .child(field("Connection limit", 160.0, div().debug_selector(|| "accounts-attr-limit".into()).child(Input::new(&self.connection_limit).small())))
                            .child(field("Valid until", 260.0, div().debug_selector(|| "accounts-attr-valid-until".into()).child(Input::new(&self.valid_until).small()))),
                    )
                    .child(self.render_password(kind, cx))
                    .into_any_element()
            }
            Attributes::Mysql(attributes) => {
                let a = attributes.clone();
                let role = kind == AccountKind::Role;
                let row = self.model.selected_row();
                let plugin = row.and_then(|row| row.plugin.clone()).unwrap_or_else(|| "none".to_owned());
                let expired = row.is_some_and(|row| row.password_expired);
                let panel = panel.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(note("accounts-plugin", format!("Authentication plugin: {plugin} (shown, not edited here)"), theme::text_muted()))
                        .child(note("accounts-expired", if expired { "Its password has expired: it must choose a new one at its next login." } else { "Its password has not expired." }, theme::text_muted())),
                );
                if role {
                    return panel.child(note("accounts-role-note", "A role has no password or limits: grant it privileges in the Privileges tab, and add accounts to it from their Membership tab.", theme::text_muted())).into_any_element();
                }
                panel
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(18.0))
                            .child(self.flag("accounts-attr-locked", "Account locked", a.locked, cx, |this, on, _, cx| this.set_my_flag(|a| a.locked = on, cx)))
                            .child(self.flag("accounts-attr-expire", "Require a new password at the next login", a.expire_password, cx, |this, on, _, cx| this.set_my_flag(|a| a.expire_password = on, cx))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap(px(12.0))
                            .child(field("Max queries per hour", 170.0, div().debug_selector(|| "accounts-attr-max-queries".into()).child(Input::new(&self.max_queries).small())))
                            .child(field("Max updates per hour", 170.0, div().debug_selector(|| "accounts-attr-max-updates".into()).child(Input::new(&self.max_updates).small())))
                            .child(field("Max connections per hour", 170.0, div().debug_selector(|| "accounts-attr-max-connections".into()).child(Input::new(&self.max_connections).small())))
                            .child(field("Max simultaneous connections", 190.0, div().debug_selector(|| "accounts-attr-max-user".into()).child(Input::new(&self.max_user_connections).small()))),
                    )
                    .child(self.render_password(kind, cx))
                    .into_any_element()
            }
        }
    }

    fn render_password(&self, kind: AccountKind, cx: &mut Context<Self>) -> AnyElement {
        let postgres = self.engine == AccountEngine::Postgres;
        let clearing = self.model.detail().is_some_and(|detail| detail.working.password == PasswordChange::Clear);
        if !postgres && kind == AccountKind::Role {
            return div().into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(field("New password", 340.0, div().debug_selector(|| "accounts-password".into()).child(Input::new(&self.password).small().mask_toggle().disabled(clearing))))
            .children(postgres.then(|| {
                self.flag("accounts-password-clear", "Remove the password (the role can no longer log in with one)", clearing, cx, |this, on, window, cx| {
                    this.set_clear_password(on, window, cx)
                })
            }))
            .child(note(
                "accounts-password-note",
                if postgres {
                    "The password is hashed here (SCRAM-SHA-256): the server receives only the verifier, never the password. The review hides it."
                } else {
                    "MySQL and MariaDB take the password as text in the statement: use a TLS connection. The review and the logs hide it."
                },
                theme::text_muted(),
            ))
            .into_any_element()
    }

    // ---- Membership -----------------------------------------------------------------------------------

    fn render_membership(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(id) = self.model.selected.clone() else { return div() };
        let members: Vec<String> = self.model.loaded().map(|catalog| catalog.members_of(&id).map(|m| m.member.to_string()).collect()).unwrap_or_default();
        div()
            .debug_selector(|| "accounts-membership".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(note("accounts-membership-hint", format!("{id} belongs to the roles ticked below."), theme::text_muted()))
            .child(div().debug_selector(|| "accounts-detail-filter".into()).w(px(260.0)).child(Input::new(&self.detail_filter).small()))
            .child(
                div().debug_selector(|| "accounts-roles".into()).flex_1().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
                    uniform_list(
                        "accounts-role-rows",
                        self.roles_shown.len(),
                        cx.processor(|this, range: Range<usize>, _window, cx| {
                            #[cfg(test)]
                            this.drawn.set([this.drawn.get()[0], (range.start, range.end), this.drawn.get()[2]]);
                            range.filter_map(|position| this.role_row(position, cx)).collect::<Vec<_>>()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.roles_scroll),
                ),
            )
            .child(note(
                "accounts-members",
                if members.is_empty() { format!("Nobody belongs to {id}.") } else { format!("Members of {id}: {}", members.join(", ")) },
                theme::text_muted(),
            ))
    }

    fn role_row(&self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let row = self.model.loaded()?.accounts.get(*self.roles_shown.get(position)?)?;
        let member = self.model.is_member_of(&row.id);
        // A membership another role granted: shown with whose it is (the tab cannot take it back).
        let foreign = self
            .model
            .selected
            .as_ref()
            .and_then(|selected| self.model.loaded().and_then(|catalog| catalog.roles_of(selected).find(|membership| membership.role == row.id)))
            .filter(|membership| !membership.revocable)
            .map(|membership| membership.grantor.clone());
        Some(
            div()
                .id(("accounts-role", position))
                .debug_selector(move || format!("accounts-role-{position}"))
                .h(px(ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .cursor_pointer()
                .hover(|this| this.bg(theme::hover()))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_role(position, cx)))
                .child(drawn_box(member))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(row.id.to_string()))
                .children(foreign.map(|grantor| div().text_size(px(10.5)).text_color(theme::text_muted()).child(format!("granted by {grantor}"))))
                .into_any_element(),
        )
    }

    // ---- Privileges -----------------------------------------------------------------------------------

    fn render_privileges(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let Some(detail) = self.model.detail() else { return div() };
        let postgres = self.engine == AccountEngine::Postgres;
        let body = match &detail.privileges {
            Load::Idle | Load::Loading => div().debug_selector(|| "accounts-privileges-loading".into()).p(px(4.0)).text_color(theme::text_muted()).child("Reading privileges\u{2026}").into_any_element(),
            Load::Failed(error) => self.render_error("accounts-privileges-error", error, cx).into_any_element(),
            Load::Ready(privileges) => {
                let mut footer = Vec::new();
                if privileges.tables_truncated {
                    footer.push(format!("Only the first {} tables are listed; the ones that hold a privilege always are.", crate::sql_account::catalog::TABLE_LIMIT));
                }
                if !privileges.other.is_empty() {
                    let list: Vec<String> = privileges.other.iter().map(|(target, name)| format!("{name} on {}", target.label(!postgres))).collect();
                    footer.push(format!("Also held, shown and never changed here: {}.", list.join(", ")));
                }
                if !privileges.columns.is_empty() {
                    footer.push(format!("Column privileges (read-only): {}.", privileges.columns.join(", ")));
                }
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .gap(px(10.0))
                            .child(self.render_objects(cx))
                            .child(self.render_object_editor(cx)),
                    )
                    .children(footer.into_iter().map(|text| note("accounts-privileges-note", text, theme::text_muted())))
                    .into_any_element()
            }
        };
        div()
            .debug_selector(|| "accounts-privileges".into())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().debug_selector(|| "accounts-detail-filter".into()).w(px(260.0)).child(Input::new(&self.detail_filter).small()))
                    .children(postgres.then(|| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child("Database"))
                            .child(div().debug_selector(|| "accounts-database".into()).w(px(200.0)).child(Select::new(&self.database_select).small().w_full().menu_max_h(px(240.0))))
                    })),
            )
            .child(body)
    }

    fn render_objects(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().debug_selector(|| "accounts-objects".into()).w(px(300.0)).flex_shrink_0().min_h_0().border_1().border_color(theme::border()).rounded(px(4.0)).child(
            uniform_list(
                "accounts-object-rows",
                self.objects_shown.len(),
                cx.processor(|this, range: Range<usize>, _window, cx| {
                    #[cfg(test)]
                    this.drawn.set([this.drawn.get()[0], this.drawn.get()[1], (range.start, range.end)]);
                    range.filter_map(|position| this.object_row(position, cx)).collect::<Vec<_>>()
                }),
            )
            .size_full()
            .track_scroll(&self.objects_scroll),
        )
    }

    fn object_row(&self, position: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let detail = self.model.detail()?;
        let target = detail.privileges.ready()?.objects.get(*self.objects_shown.get(position)?)?;
        let mysql = self.engine.is_mysql();
        let selected = self.selected_object.as_ref() == Some(target);
        let wanted = detail.wanted(target);
        let edited = detail.working.grants.contains_key(target);
        let summary = {
            let names: Vec<&str> = target.offered(mysql).iter().filter(|privilege| wanted.contains_key(privilege)).map(|privilege| privilege.name()).collect();
            match names.len() {
                0 => String::new(),
                1..=2 => names.join(", "),
                n => format!("{} +{}", names[..2].join(", "), n - 2),
            }
        };
        let kind = target.noun(mysql);
        Some(
            div()
                .id(("accounts-object", position))
                .debug_selector(move || format!("accounts-object-{position}"))
                .h(px(ROW_HEIGHT))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .when(selected, |this| this.bg(theme::selected()))
                .hover(|this| this.bg(theme::hover()))
                .on_click(cx.listener(move |this, _, _, cx| this.select_object(position, cx)))
                .child(div().w(px(6.0)).h(px(6.0)).flex_shrink_0().rounded(px(3.0)).bg(if edited { theme::accent() } else { gpui::Rgba { a: 0.0, ..theme::accent() } }))
                .child(div().text_size(px(10.0)).w(px(48.0)).flex_shrink_0().text_color(theme::text_faint()).child(kind))
                .child(div().flex_1().min_w_0().truncate().text_size(px(12.0)).child(target.label(mysql)))
                .child(div().max_w(px(110.0)).truncate().text_size(px(10.5)).text_color(theme::text_muted()).child(summary))
                .into_any_element(),
        )
    }

    /// The checkboxes of the object selected in the list: the only ones that exist.
    fn render_object_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(target) = self.selected_object.clone() else {
            return div().debug_selector(|| "accounts-object-hint".into()).flex_1().text_color(theme::text_muted()).child("Select an object to see and change what this role may do with it.").into_any_element();
        };
        let Some(detail) = self.model.detail() else { return div().into_any_element() };
        let mysql = self.engine.is_mysql();
        let wanted = detail.wanted(&target);
        let foreign: Vec<String> = detail.foreign(&target).iter().map(|grant| format!("{} by {}", grant.privilege, grant.grantor)).collect();
        let tab_id = self.tab_id;
        let rows: Vec<AnyElement> = target
            .offered(mysql)
            .iter()
            .copied()
            .map(|privilege| self.privilege_row(&target, privilege, wanted.get(&privilege).copied(), cx))
            .collect();
        div()
            .debug_selector(|| "accounts-object-editor".into())
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().debug_selector(|| "accounts-object-title".into()).flex_1().min_w_0().truncate().text_size(px(12.5)).font_weight(gpui::FontWeight::SEMIBOLD).child(format!("{} {}", target.noun(mysql), target.label(mysql))))
                    .child({
                        let target = target.clone();
                        Button::new(("accounts-priv-all", tab_id)).debug_selector(|| "accounts-priv-all".into()).label("All").compact().ghost().on_click(cx.listener(move |this, _, _, cx| this.set_all_privileges(&target, true, cx)))
                    })
                    .child({
                        let target = target.clone();
                        Button::new(("accounts-priv-none", tab_id)).debug_selector(|| "accounts-priv-none".into()).label("None").compact().ghost().on_click(cx.listener(move |this, _, _, cx| this.set_all_privileges(&target, false, cx)))
                    }),
            )
            .children((!mysql).then(|| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .text_size(px(10.5))
                    .text_color(theme::text_faint())
                    .child(div().w(px(140.0)).child("Privilege"))
                    .child(div().w(px(110.0)).child("With grant option"))
            }))
            .child(div().flex_1().min_h_0().overflow_y_scrollbar().flex().flex_col().gap(px(2.0)).children(rows))
            .children((!foreign.is_empty()).then(|| {
                note(
                    "accounts-priv-foreign",
                    format!("Also granted by other roles, which this connection cannot take back: {}.", foreign.join(", ")),
                    theme::text_muted(),
                )
            }))
            .into_any_element()
    }

    fn privilege_row(&self, target: &GrantTarget, privilege: Privilege, state: Option<bool>, cx: &mut Context<Self>) -> AnyElement {
        let tab_id = self.tab_id;
        let name = privilege.name();
        let slug = name.to_lowercase().replace(' ', "-");
        let (granted_selector, option_selector) = (format!("accounts-priv-{slug}"), format!("accounts-priv-option-{slug}"));
        let postgres = self.engine == AccountEngine::Postgres;
        let granted = {
            let target = target.clone();
            let selector = granted_selector.clone();
            div().w(px(140.0)).flex().child(
                div().debug_selector(move || selector.clone()).child(
                    Checkbox::new(gpui::ElementId::Name(format!("{granted_selector}-{tab_id}").into()))
                        .label(name)
                        .checked(state.is_some())
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_privilege(&target, privilege, *checked, cx))),
                ),
            )
        };
        let option = postgres.then(|| {
            let target = target.clone();
            let selector = option_selector.clone();
            div().w(px(110.0)).flex().child(
                div().debug_selector(move || selector.clone()).child(
                    Checkbox::new(gpui::ElementId::Name(format!("{option_selector}-{tab_id}").into()))
                        .checked(state == Some(true))
                        .disabled(state.is_none())
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| this.set_grant_option(&target, privilege, *checked, cx))),
                ),
            )
        });
        div().flex().items_center().gap(px(8.0)).h(px(24.0)).child(granted).children(option).into_any_element()
    }

    // ---- states -----------------------------------------------------------------------------------------

    fn render_error(&self, selector: &'static str, error: &ReadError, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = match error {
            ReadError::Denied(_) => Some(if self.engine.is_mysql() {
                "The account this connection logs in as may not read the users: it needs SELECT on the mysql schema (and CREATE USER or GRANT OPTION to change them). Connect with an administrator account."
            } else {
                "The account this connection logs in as may not read the roles. Connect with an account that can."
            }),
            _ => None,
        };
        div()
            .debug_selector(move || selector.into())
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(div().text_color(theme::danger()).child(error.to_string()))
            .children(hint.map(|hint| div().text_size(px(11.5)).text_color(theme::text_muted()).child(hint)))
            .child(
                div().flex().child(
                    Button::new((selector, self.tab_id)).debug_selector(move || format!("{selector}-retry")).label("Try again").compact().on_click(cx.listener(|this, _, _, cx| {
                        this.reload(cx);
                    })),
                ),
            )
    }
}

impl Render for AccountsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.settle(window, cx);
        let tab_id = self.tab_id;
        let body = match &self.model.catalog {
            Load::Idle | Load::Loading => div()
                .debug_selector(|| "accounts-loading".into())
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme::text_muted())
                .child("Reading the roles\u{2026}")
                .into_any_element(),
            Load::Failed(error) => div().flex_1().child(self.render_error("accounts-error", error, cx)).into_any_element(),
            Load::Ready(_) => div().flex_1().min_h_0().flex().child(self.render_list(cx)).child(self.render_detail(window, cx)).into_any_element(),
        };
        div()
            .id(("accounts-view", tab_id))
            .debug_selector(|| "accounts-view".into())
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme::background())
            .text_size(px(12.0))
            .text_color(theme::text())
            .child(self.render_header(cx))
            .child(body)
            .child(self.render_footer(cx))
    }
}
