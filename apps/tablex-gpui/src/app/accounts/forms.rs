//! The dialogs of New role / user and Drop role / user. Each collects what the statement needs, checks it with the same generator
//! the review uses (so what the dialog accepts is what the review can show), and hands the change-set to the tab, which opens the
//! shared review dialog. Nothing runs from here.

use std::collections::HashSet;

use gpui::{SharedString, Subscription, WeakEntity};
use gpui_component::dialog::DialogFooter;
use gpui_component::input::InputEvent;
use gpui_component::select::{Select, SelectState};
use gpui_component::IndexPath;

use super::super::*;
use super::view::AccountsView;
use crate::sql_account::{
    AccountChange, AccountEngine, AccountId, AccountKind, Attributes, CreateAccount, DropAccount, MyAttributes, Password, PasswordChange, PgAttributes, Reveal,
};
use crate::sql_ddl::{dialect_in_mode, Change};

type SelectEntity = gpui::Entity<SelectState<Vec<SharedString>>>;

fn text_input<V: 'static>(placeholder: &'static str, masked: bool, window: &mut Window, cx: &mut Context<V>) -> gpui::Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).masked(masked).placeholder(placeholder))
}

fn field(label: &'static str, selector: &'static str, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(label))
        .child(div().debug_selector(move || selector.into()).child(control))
}

fn error_line(selector: &'static str, error: Option<String>) -> Option<Div> {
    error.map(|error| div().debug_selector(move || selector.into()).text_size(px(11.5)).text_color(theme::danger()).child(error))
}

/// The footer of both dialogs.
fn footer(cancel: &'static str, submit: &'static str, label: &'static str, destructive: bool, on_submit: impl Fn(&mut Window, &mut gpui::App) + 'static) -> DialogFooter {
    let button = Button::new(submit).label(label).compact().debug_selector(move || submit.into());
    DialogFooter::new()
        .child(Button::new(cancel).label("Cancel").compact().ghost().debug_selector(move || cancel.into()).on_click(|_, window, cx| window.close_dialog(cx)))
        .child((if destructive { button.danger() } else { button.primary() }).on_click(move |_, window, cx| on_submit(window, cx)))
}

const REVIEW_NOT_OPENED: &str = "The review could not be opened (the connection cannot be edited, or another dialog is open). Nothing was changed; what you typed is kept.";

/// What is wrong with the New dialog's fields.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FormError {
    Exists(AccountId),
    PasswordsDiffer,
    NoDialect,
    /// The statement the fields make is refused (a name, an attribute, a password): the generator's words.
    Statement(crate::sql_ddl::DdlError),
}

impl std::fmt::Display for FormError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exists(id) => write!(f, "{id} exists already."),
            Self::PasswordsDiffer => f.write_str("The two passwords differ."),
            Self::NoDialect => f.write_str("This connection has no SQL dialect."),
            Self::Statement(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for FormError {}

// ---- New role / user ------------------------------------------------------------------------------------

struct NewAccountDialog {
    view: WeakEntity<AccountsView>,
    root: WeakEntity<AppRoot>,
    engine: AccountEngine,
    kind: ConnectionKind,
    connection: String,
    existing: HashSet<AccountId>,
    name: gpui::Entity<InputState>,
    host: gpui::Entity<InputState>,
    password: gpui::Entity<InputState>,
    confirm: gpui::Entity<InputState>,
    /// MySQL, MariaDB: user or role.
    kind_select: SelectEntity,
    pg: PgAttributes,
    locked: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl NewAccountDialog {
    fn role_chosen(&self, cx: &gpui::App) -> bool {
        self.engine.is_mysql() && self.kind_select.read(cx).selected_index(cx).is_some_and(|index| index.row == 1)
    }

    /// The change the fields describe, or what is wrong with them.
    fn build(&self, cx: &gpui::App) -> Result<CreateAccount, FormError> {
        let name = self.name.read(cx).value().to_string();
        let role = self.role_chosen(cx);
        let host = self.host.read(cx).value().to_string();
        let id = match (self.engine, role) {
            (AccountEngine::Postgres, _) => AccountId::role(&name),
            (AccountEngine::Mariadb, true) => AccountId::role(&name),
            _ => AccountId::account(&name, host.trim()),
        };
        if self.existing.contains(&id) {
            return Err(FormError::Exists(id));
        }
        // Both are held as passwords from the moment they are read: wiped when dropped, and compared without a plain copy.
        let typed = Password::new(self.password.read(cx).value().to_string());
        let again = Password::new(self.confirm.read(cx).value().to_string());
        if typed != again {
            return Err(FormError::PasswordsDiffer);
        }
        let attributes = match self.engine {
            AccountEngine::Postgres => Attributes::Postgres(self.pg.clone()),
            _ => Attributes::Mysql(MyAttributes { locked: self.locked, ..MyAttributes::default() }),
        };
        let password = if typed.with(str::is_empty).unwrap_or(true) { PasswordChange::Keep } else { PasswordChange::Set(typed) };
        let create = CreateAccount { id, kind: if role { AccountKind::Role } else { AccountKind::User }, attributes, password };
        // The generator the review uses says whether the name and the attributes make a statement.
        let sql_mode = self.root.read_with(cx, |root, _| root.diagram.sql_modes.get(&self.connection).cloned()).ok().flatten().unwrap_or_default();
        let dialect = dialect_in_mode(self.kind, &sql_mode).ok_or(FormError::NoDialect)?;
        match dialect.account_statements(&AccountChange::Create(create.clone()), Reveal::Redacted) {
            Ok(_) => Ok(create),
            Err(error) => {
                create.password.wipe();
                Err(FormError::Statement(error))
            }
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let create = match self.build(cx) {
            Ok(create) => create,
            Err(error) => {
                self.error = Some(error.to_string());
                return cx.notify();
            }
        };
        // The review cannot open over this dialog: it closes first. What was typed goes (the passwords with it) only once the review
        // has opened; if it could not, the dialog comes back as it was.
        window.close_dialog(cx);
        let opened = self.view.update(cx, |view, cx| view.open_review(vec![Change::Account(AccountChange::Create(create))], window, cx)).unwrap_or(false);
        if opened {
            for state in [&self.password, &self.confirm] {
                state.update(cx, |state, cx| state.set_value("", window, cx));
            }
        } else {
            self.error = Some(REVIEW_NOT_OPENED.to_owned());
            present_new(cx.entity(), format!("New {}", self.engine.noun()), window, cx);
        }
    }

    fn flag(&self, selector: &'static str, label: &'static str, checked: bool, cx: &mut Context<Self>, on: impl Fn(&mut Self, bool) + 'static) -> impl IntoElement {
        div().self_start().debug_selector(move || selector.into()).child(Checkbox::new(selector).label(label).checked(checked).on_click(cx.listener(move |this, checked: &bool, _, cx| {
            on(this, *checked);
            cx.notify();
        })))
    }
}

impl Render for NewAccountDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let postgres = self.engine == AccountEngine::Postgres;
        let role = self.role_chosen(cx);
        let a = self.pg.clone();
        let noun = if role { "role" } else { self.engine.noun() };
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(field("Name", "accounts-new-name", Input::new(&self.name)))
            .children((!postgres).then(|| field("Type", "accounts-new-kind", Select::new(&self.kind_select).small().w_full())))
            .children((!postgres && (!role || self.engine != AccountEngine::Mariadb)).then(|| field("Host (% for any)", "accounts-new-host", Input::new(&self.host))))
            .children((!role).then(|| field("Password", "accounts-new-password", Input::new(&self.password).mask_toggle())))
            .children((!role).then(|| field("Password again", "accounts-new-confirm", Input::new(&self.confirm).mask_toggle())))
            .children(postgres.then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(14.0))
                    .child(self.flag("accounts-new-login", "Can log in", a.login, cx, |this, on| this.pg.login = on))
                    .child(self.flag("accounts-new-superuser", "Superuser", a.superuser, cx, |this, on| this.pg.superuser = on))
                    .child(self.flag("accounts-new-createdb", "Create databases", a.create_db, cx, |this, on| this.pg.create_db = on))
                    .child(self.flag("accounts-new-createrole", "Create roles", a.create_role, cx, |this, on| this.pg.create_role = on))
                    .child(self.flag("accounts-new-inherit", "Inherit privileges", a.inherit, cx, |this, on| this.pg.inherit = on))
                    .child(self.flag("accounts-new-replication", "Replication", a.replication, cx, |this, on| this.pg.replication = on))
                    .child(self.flag("accounts-new-bypassrls", "Bypass row-level security", a.bypass_rls, cx, |this, on| this.pg.bypass_rls = on))
            }))
            .children((!postgres && !role).then(|| self.flag("accounts-new-locked", "Create it locked", self.locked, cx, |this, on| this.locked = on)))
            .children(error_line("accounts-new-error", self.error.clone()))
            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child(if postgres {
                format!("The {noun} is created by the SQL shown next; nothing runs until you apply it. Its password is hashed here: only a verifier is sent.")
            } else {
                format!("The {noun} is created by the SQL shown next; nothing runs until you apply it. The password is sent as text: use a TLS connection.")
            }))
    }
}

/// Shows the New dialog (again, when its review could not open).
fn present_new(dialog: gpui::Entity<NewAccountDialog>, title: String, window: &mut Window, cx: &mut gpui::App) {
    window.open_dialog(cx, move |builder, _, _| {
        let (submit, enter) = (dialog.clone(), dialog.clone());
        builder
            .title(title.clone())
            .w(px(520.0))
            .child(dialog.clone())
            .on_ok(move |_, window, cx| {
                enter.update(cx, |this, cx| this.submit(window, cx));
                false
            })
            .footer(footer("accounts-new-cancel", "accounts-new-submit", "Review SQL", false, move |window, cx| {
                submit.update(cx, |this, cx| this.submit(window, cx));
            }))
    });
}

impl AccountsView {
    pub(super) fn open_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.loaded().is_none() || window.has_active_dialog(cx) {
            return;
        }
        let existing: HashSet<AccountId> = self.model.loaded().map(|catalog| catalog.accounts.iter().map(|row| row.id.clone()).collect()).unwrap_or_default();
        let engine = self.engine;
        let noun = engine.noun();
        let (view, root, kind, connection) = (cx.entity().downgrade(), self.root.clone(), self.kind, self.connection.clone());
        let dialog = cx.new(|cx| {
            let host = text_input("%", false, window, cx);
            host.update(cx, |state, cx| state.set_value("%", window, cx));
            let name = text_input(if engine == AccountEngine::Postgres { "Role name" } else { "User name" }, false, window, cx);
            let subscription = cx.subscribe(&name, |this: &mut NewAccountDialog, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.error = None;
                    cx.notify();
                }
            });
            NewAccountDialog {
                view,
                root,
                engine,
                kind,
                connection,
                existing,
                name,
                host,
                password: text_input("Password", true, window, cx),
                confirm: text_input("Password again", true, window, cx),
                kind_select: cx.new(|cx| SelectState::new(vec![SharedString::from("User"), SharedString::from("Role")], Some(IndexPath::default()), window, cx)),
                pg: PgAttributes { login: true, ..PgAttributes::default() },
                locked: false,
                error: None,
                _subscriptions: vec![subscription],
            }
        });
        let focus = dialog.read(cx).name.clone();
        present_new(dialog, format!("New {noun}"), window, cx);
        focus.update(cx, |state, cx| state.focus(window, cx));
    }
}

// ---- Drop role / user -----------------------------------------------------------------------------------

struct DropAccountDialog {
    view: WeakEntity<AccountsView>,
    engine: AccountEngine,
    id: AccountId,
    kind: AccountKind,
    own: bool,
    database: String,
    /// The accounts a PostgreSQL role's objects can go to.
    others: Vec<AccountId>,
    reassign: bool,
    drop_owned: bool,
    target_select: SelectEntity,
    error: Option<String>,
}

impl DropAccountDialog {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut changes = Vec::new();
        if self.reassign {
            let Some(to) = self.target_select.read(cx).selected_index(cx).and_then(|index| self.others.get(index.row)).cloned() else {
                self.error = Some("Choose the role that takes over what it owns.".to_owned());
                return cx.notify();
            };
            changes.push(Change::Account(AccountChange::ReassignOwned { from: self.id.clone(), to, database: self.database.clone() }));
        }
        if self.drop_owned {
            changes.push(Change::Account(AccountChange::DropOwned { role: self.id.clone(), database: self.database.clone() }));
        }
        changes.push(Change::Account(AccountChange::Drop(DropAccount { id: self.id.clone(), kind: self.kind, own: self.own })));
        window.close_dialog(cx);
        if !self.view.update(cx, |view, cx| view.open_review(changes, window, cx)).unwrap_or(false) {
            self.error = Some(REVIEW_NOT_OPENED.to_owned());
            present_drop(cx.entity(), format!("Drop {}", self.kind.noun()), window, cx);
        }
    }
}

impl Render for DropAccountDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let postgres = self.engine == AccountEngine::Postgres;
        let id = self.id.to_string();
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(self.own.then(|| {
                div()
                    .debug_selector(|| "accounts-drop-own".into())
                    .p(px(8.0))
                    .rounded(px(4.0))
                    .bg(theme::pending_cell())
                    .text_size(px(11.5))
                    .child(format!("{id} is the account this connection logs in as. Dropping it ends your access to the server once this session closes."))
            }))
            .child(div().text_size(px(12.0)).child(format!("Drop {} {id}?", self.kind.noun())))
            .children(postgres.then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(div().text_size(px(11.5)).text_color(theme::text_muted()).child(format!(
                        "PostgreSQL refuses to drop a role that still owns objects or holds privileges. Choose what happens to them in {} (the other databases of the server need the same, one by one).",
                        self.database
                    )))
                    .child(div().self_start().debug_selector(|| "accounts-drop-reassign".into()).child(
                        Checkbox::new("accounts-drop-reassign-box").label("Give what it owns to another role first (REASSIGN OWNED BY)").checked(self.reassign).on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.reassign = *checked;
                            this.error = None;
                            cx.notify();
                        })),
                    ))
                    .children(self.reassign.then(|| field("New owner", "accounts-drop-target", Select::new(&self.target_select).small().w_full().menu_max_h(px(220.0)))))
                    .child(div().self_start().debug_selector(|| "accounts-drop-owned".into()).child(
                        Checkbox::new("accounts-drop-owned-box").label("Drop what it still owns and revoke its privileges (DROP OWNED BY)").checked(self.drop_owned).on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.drop_owned = *checked;
                            cx.notify();
                        })),
                    ))
                    .children(self.drop_owned.then(|| div().text_size(px(11.5)).text_color(theme::danger()).child("DROP OWNED BY deletes the tables and other objects it owns in this database, rows included.")))
            }))
            .children((!postgres).then(|| {
                div().debug_selector(|| "accounts-drop-sessions".into()).text_size(px(11.5)).text_color(theme::text_muted()).child(
                    "Sessions this account has open stay connected until they end; only new logins are refused. Kill them from the server's process list if they must stop now.",
                )
            }))
            .children(error_line("accounts-drop-error", self.error.clone()))
            .child(div().text_size(px(11.0)).text_color(theme::text_muted()).child("The SQL is shown next, and nothing runs until you apply it."))
    }
}

/// Shows the Drop dialog (again, when its review could not open).
fn present_drop(dialog: gpui::Entity<DropAccountDialog>, title: String, window: &mut Window, cx: &mut gpui::App) {
    window.open_dialog(cx, move |builder, _, _| {
        let (submit, enter) = (dialog.clone(), dialog.clone());
        builder
            .title(title.clone())
            .w(px(520.0))
            .child(dialog.clone())
            .on_ok(move |_, window, cx| {
                enter.update(cx, |this, cx| this.submit(window, cx));
                false
            })
            .footer(footer("accounts-drop-cancel", "accounts-drop-submit", "Review SQL", true, move |window, cx| {
                submit.update(cx, |this, cx| this.submit(window, cx));
            }))
    });
}

impl AccountsView {
    pub(super) fn open_drop_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.model.selected_row().cloned() else { return };
        if window.has_active_dialog(cx) {
            return;
        }
        let others: Vec<AccountId> = self.model.loaded().map(|catalog| catalog.accounts.iter().filter(|other| other.id != row.id).map(|other| other.id.clone()).collect()).unwrap_or_default();
        let labels: Vec<SharedString> = others.iter().map(|id| SharedString::from(id.to_string())).collect();
        let (view, engine, own, database) = (cx.entity().downgrade(), self.engine, self.is_login(&row.id), self.model.database.clone());
        let dialog = cx.new(|cx| DropAccountDialog {
            view,
            engine,
            id: row.id.clone(),
            kind: row.kind,
            own,
            database,
            others,
            reassign: false,
            drop_owned: false,
            target_select: cx.new(|cx| SelectState::new(labels, Some(IndexPath::default()), window, cx)),
            error: None,
        });
        present_drop(dialog, format!("Drop {}", row.kind.noun()), window, cx);
    }
}
