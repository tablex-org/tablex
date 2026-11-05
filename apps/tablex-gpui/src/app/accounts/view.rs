//! The Users & Privileges tab: its entity and what its controls do. `panels` draws it, `forms` has the dialogs of New and Drop.
//!
//! The tab reads the catalog on a session of its own (see `load`), lists the roles / accounts, and edits the selected one into a
//! draft kept per account (`model`). Apply does not run anything itself: it turns the draft into typed changes and hands them to
//! the shared review dialog of the structure editor, which shows the SQL (a password hidden), asks for the confirmations the
//! connection requires, and runs the statements on a session of its own.
//!
//! A password lives in the masked input while it is typed, and in one shared cell from the moment the review has it; the cell is
//! wiped when the review closes or its apply ends, so no password outlives the dialog that was about to run it.

use gpui::{Context, SharedString, Subscription, WeakEntity, Window};
use gpui_component::input::InputEvent;
use gpui_component::select::{SelectEvent, SelectState};

use super::super::structure_edit::apply::TableOrigin;
use super::super::*;
use super::load::{CatalogSource, LoadJob, Loaded};
use super::AccountsError;
use super::model::{AccountsModel, Load};
use crate::sql_account::catalog::ReadError;
use crate::sql_account::{AccountEngine, AccountId, AccountKind, Attributes, GrantTarget, MyAttributes, Password, PasswordChange, PgAttributes, Privilege};

type SelectEntity = gpui::Entity<SelectState<Vec<SharedString>>>;

/// What the right-hand side shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum Panel {
    Attributes,
    Membership,
    Privileges,
}

impl Panel {
    pub(super) const ALL: [Self; 3] = [Self::Attributes, Self::Membership, Self::Privileges];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Attributes => "Attributes",
            Self::Membership => "Membership",
            Self::Privileges => "Privileges",
        }
    }

    pub(super) fn selector(self) -> &'static str {
        match self {
            Self::Attributes => "accounts-tab-attributes",
            Self::Membership => "accounts-tab-membership",
            Self::Privileges => "accounts-tab-privileges",
        }
    }
}

pub(in crate::app) struct AccountsView {
    pub(super) root: WeakEntity<AppRoot>,
    pub(super) tab_id: u64,
    pub(super) connection: String,
    pub(super) kind: ConnectionKind,
    pub(super) engine: AccountEngine,
    pub(super) model: AccountsModel,
    pub(super) panel: Panel,
    /// Filters the list of roles / accounts.
    pub(super) filter: gpui::Entity<InputState>,
    /// Filters the roles of the Membership panel and the objects of the Privileges panel.
    pub(super) detail_filter: gpui::Entity<InputState>,
    pub(super) connection_limit: gpui::Entity<InputState>,
    pub(super) valid_until: gpui::Entity<InputState>,
    pub(super) max_queries: gpui::Entity<InputState>,
    pub(super) max_updates: gpui::Entity<InputState>,
    pub(super) max_connections: gpui::Entity<InputState>,
    pub(super) max_user_connections: gpui::Entity<InputState>,
    pub(super) password: gpui::Entity<InputState>,
    pub(super) database_select: SelectEntity,
    pub(super) list_scroll: gpui::UniformListScrollHandle,
    pub(super) roles_scroll: gpui::UniformListScrollHandle,
    pub(super) objects_scroll: gpui::UniformListScrollHandle,
    /// The roles the Membership panel lists (positions in the catalog) and the objects the Privileges panel lists (positions in the
    /// selected account's privilege catalog), after the filter.
    pub(super) roles_shown: Vec<usize>,
    pub(super) objects_shown: Vec<usize>,
    pub(super) selected_object: Option<GrantTarget>,
    /// Why the attribute fields cannot be turned into attributes.
    pub(super) field_error: Option<String>,
    /// A line under the header: what happened last, and whether it failed.
    pub(super) notice: Option<(String, bool)>,
    /// Loads that answer a request older than this are dropped.
    generation: u64,
    /// The inputs must show the selected account's edits at the next frame.
    fill_pending: bool,
    /// The databases the select must offer at the next frame, and the one it shows.
    pending_databases: Option<(Vec<SharedString>, SharedString)>,
    #[cfg(test)]
    pub(super) drawn: std::cell::Cell<[(usize, usize); 3]>,
    _subscriptions: Vec<Subscription>,
}

fn input(placeholder: &'static str, window: &mut Window, cx: &mut Context<AccountsView>) -> gpui::Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

fn database_labels(databases: &[String]) -> Vec<SharedString> {
    databases.iter().map(|name| SharedString::from(name.clone())).collect()
}

impl AccountsView {
    pub(super) fn new(root: WeakEntity<AppRoot>, tab_id: u64, connection: String, kind: ConnectionKind, engine: AccountEngine, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            root,
            tab_id,
            connection,
            kind,
            engine,
            model: AccountsModel::new(engine),
            panel: Panel::Attributes,
            filter: input("Filter", window, cx),
            detail_filter: input("Filter", window, cx),
            connection_limit: input("-1 for no limit", window, cx),
            valid_until: input("never", window, cx),
            max_queries: input("0 for no limit", window, cx),
            max_updates: input("0 for no limit", window, cx),
            max_connections: input("0 for no limit", window, cx),
            max_user_connections: input("0 for no limit", window, cx),
            password: cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("Leave empty to keep the password")),
            database_select: cx.new(|cx| SelectState::new(Vec::<SharedString>::new(), None, window, cx)),
            list_scroll: gpui::UniformListScrollHandle::new(),
            roles_scroll: gpui::UniformListScrollHandle::new(),
            objects_scroll: gpui::UniformListScrollHandle::new(),
            roles_shown: Vec::new(),
            objects_shown: Vec::new(),
            selected_object: None,
            field_error: None,
            notice: None,
            generation: 0,
            fill_pending: false,
            pending_databases: None,
            #[cfg(test)]
            drawn: Default::default(),
            _subscriptions: Vec::new(),
        };
        view.wire(window, cx);
        view
    }

    fn wire(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(&self.filter, |this: &mut Self, state, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let text = state.read(cx).value().to_string();
                this.model.set_filter(&text);
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe(&self.detail_filter, |this: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.refilter_detail(cx);
                cx.notify();
            }
        }));
        for state in [&self.connection_limit, &self.valid_until, &self.max_queries, &self.max_updates, &self.max_connections, &self.max_user_connections] {
            subscriptions.push(cx.subscribe(state, |this: &mut Self, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.attributes_from_inputs(cx);
                    cx.notify();
                }
            }));
        }
        subscriptions.push(cx.subscribe(&self.password, |_: &mut Self, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe_in(&self.database_select, window, |this: &mut Self, select, event: &SelectEvent<Vec<SharedString>>, _, cx| {
            if matches!(event, SelectEvent::Confirm(Some(_))) {
                let name = select.read(cx).selected_value().map(|value| value.to_string());
                if let Some(name) = name.filter(|name| *name != this.model.database) {
                    this.choose_database(name, cx);
                }
            }
        }));
        self._subscriptions = subscriptions;
    }

    /// Where the tab's apply runs: PostgreSQL's privileges belong to one database.
    pub(super) fn origin(&self) -> TableOrigin {
        TableOrigin { connection: self.connection.clone(), database: self.model.database.clone(), schema: String::new(), object: String::new() }
    }

    // ---- loading -------------------------------------------------------------------------------------

    /// Reads the roles / accounts from `source`.
    pub(in crate::app) fn start(&mut self, source: Result<CatalogSource, AccountsError>, cx: &mut Context<Self>) {
        self.reload_with(source, cx);
    }

    /// The Refresh and Try again buttons: reads again from a new session of the connection.
    pub(in crate::app) fn reload(&mut self, cx: &mut Context<Self>) {
        let source = self.root.read_with(cx, |root, _| root.accounts_catalog_source(&self.connection)).unwrap_or(Err(AccountsError::WindowClosing));
        self.reload_with(source, cx);
    }

    fn reload_with(&mut self, source: Result<CatalogSource, AccountsError>, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.model.catalog = Load::Loading;
        self.model.forget_privileges();
        let engine = self.engine;
        cx.notify();
        match source {
            Err(error) => self.model.catalog = Load::Failed(ReadError::Failed(error.to_string())),
            Ok(source) => self.spawn_load(source, LoadJob::Accounts { engine }, move |this, result, cx| this.catalog_loaded(generation, result, cx), cx),
        }
    }

    fn spawn_load(&self, source: CatalogSource, job: LoadJob, done: impl FnOnce(&mut Self, Result<Loaded, ReadError>, &mut Context<Self>) + 'static, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let work = cx.background_executor().spawn(async move { smol::unblock(move || source.run(job)).await });
            let result = work.await;
            let _ = this.update(cx, |this, cx| done(this, result, cx));
        })
        .detach();
    }

    fn catalog_loaded(&mut self, generation: u64, result: Result<Loaded, ReadError>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(Loaded::Accounts(catalog)) => {
                if let Some(mode) = catalog.sql_mode.clone() {
                    let connection = self.connection.clone();
                    let _ = self.root.update(cx, |root, _| root.diagram.sql_modes.insert(connection, mode));
                }
                let databases = database_labels(&catalog.databases);
                let database = if self.model.database.is_empty() { catalog.database.clone() } else { self.model.database.clone() };
                // What is typed in the password field is kept with the account before the catalog is replaced under it.
                self.stash_password(cx);
                self.model.set_catalog(*catalog);
                self.fill_pending = true;
                self.set_databases(databases, &database);
                self.load_privileges_of_edited(cx);
                self.refilter_detail(cx);
            }
            Ok(Loaded::Privileges(_)) => {}
            Err(error) => self.model.catalog = Load::Failed(error),
        }
        cx.notify();
    }

    /// The databases the PostgreSQL select offers: set at the next frame (a select changes with a window).
    fn set_databases(&mut self, databases: Vec<SharedString>, current: &str) {
        if self.engine == AccountEngine::Postgres {
            self.pending_databases = Some((databases, current.to_owned().into()));
        }
    }

    /// Reads what the selected account holds, once, for the database in use.
    pub(super) fn load_privileges_if_needed(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.model.selected.clone() {
            self.load_privileges_for(&id, cx);
        }
    }

    /// [`Self::load_privileges_if_needed`] for the selected account and every account with privilege edits (after an apply their edits are
    /// compared with what the server holds now).
    fn load_privileges_of_edited(&mut self, cx: &mut Context<Self>) {
        for id in self.model.ids_with_privilege_edits() {
            self.load_privileges_for(&id, cx);
        }
        self.load_privileges_if_needed(cx);
    }

    fn load_privileges_for(&mut self, id: &AccountId, cx: &mut Context<Self>) {
        let id = id.clone();
        let Some(detail) = self.model.detail_of(&id) else { return };
        if !matches!(detail.privileges, Load::Idle) || self.model.loaded().is_none() {
            return;
        }
        let sql_mode = self.model.loaded().and_then(|catalog| catalog.sql_mode.clone()).unwrap_or_default();
        let database = (self.engine == AccountEngine::Postgres).then(|| self.model.database.clone()).filter(|name| !name.is_empty());
        let source = self.root.read_with(cx, |root, _| root.accounts_catalog_source(&self.connection)).unwrap_or(Err(AccountsError::WindowClosing));
        let generation = self.generation;
        if let Some(detail) = self.model.detail_of(&id) {
            detail.privileges = match &source {
                Ok(_) => Load::Loading,
                Err(error) => Load::Failed(ReadError::Failed(error.to_string())),
            };
        }
        let Ok(source) = source else { return };
        let job = LoadJob::Privileges { kind: self.kind, sql_mode, id: id.clone(), database: database.clone() };
        self.spawn_load(
            source,
            job,
            move |this, result, cx| {
                // An answer for another database or an older load is not this account's now.
                if generation != this.generation || (this.engine == AccountEngine::Postgres && database.as_deref() != Some(this.model.database.as_str())) {
                    return;
                }
                if let Some(detail) = this.model.detail_of(&id) {
                    detail.privileges = match result {
                        Ok(Loaded::Privileges(catalog)) => Load::Ready(std::sync::Arc::from(catalog)),
                        Ok(Loaded::Accounts(_)) => Load::Failed(ReadError::Malformed("an unexpected answer".to_owned())),
                        Err(error) => Load::Failed(error),
                    };
                }
                // Edits the server now holds (an apply ran them) are no longer edits.
                this.model.reconcile_grants(&id);
                this.refilter_detail(cx);
                cx.notify();
            },
            cx,
        );
    }

    /// PostgreSQL: another database's schemas and tables.
    fn choose_database(&mut self, name: String, cx: &mut Context<Self>) {
        if self.model.has_privilege_edits() {
            self.notice = Some(("Apply or discard the privilege changes before choosing another database.".to_owned(), true));
            cx.notify();
            return;
        }
        self.model.database = name;
        self.selected_object = None;
        self.model.forget_privileges();
        self.generation += 1;
        self.load_privileges_if_needed(cx);
        cx.notify();
    }

    // ---- selection and the inputs ----------------------------------------------------------------------

    pub(super) fn select_position(&mut self, position: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.model.row_at(position).map(|row| row.id.clone()) else { return };
        self.select(&id, window, cx);
    }

    pub(super) fn select(&mut self, id: &AccountId, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.selected.as_ref() == Some(id) {
            return;
        }
        self.stash_password(cx);
        self.model.select(id);
        self.selected_object = None;
        self.field_error = None;
        self.fill_inputs(window, cx);
        self.load_privileges_if_needed(cx);
        self.refilter_detail(cx);
        cx.notify();
    }

    /// What the password input holds goes to the selected account's edits (and the input is emptied).
    fn stash_password(&mut self, cx: &mut Context<Self>) {
        if self.model.detail().is_some_and(|detail| detail.working.password == PasswordChange::Clear) {
            return;
        }
        let text = self.password.read(cx).value().to_string();
        self.model.set_password(if text.is_empty() { PasswordChange::Keep } else { PasswordChange::Set(Password::new(text)) });
    }

    /// The inputs show the selected account's edits.
    pub(super) fn fill_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fill_pending = false;
        let Some(detail) = self.model.detail() else {
            for state in self.all_inputs() {
                state.update(cx, |state, cx| state.set_value("", window, cx));
            }
            return;
        };
        let typed = match &detail.working.password {
            PasswordChange::Set(password) => password.with(str::to_owned).unwrap_or_default(),
            _ => String::new(),
        };
        let fields: [(&gpui::Entity<InputState>, String); 6] = match &detail.working.attributes {
            Attributes::Postgres(attributes) => [
                (&self.connection_limit, attributes.connection_limit.to_string()),
                (&self.valid_until, attributes.valid_until.clone().unwrap_or_default()),
                (&self.max_queries, String::new()),
                (&self.max_updates, String::new()),
                (&self.max_connections, String::new()),
                (&self.max_user_connections, String::new()),
            ],
            Attributes::Mysql(attributes) => [
                (&self.connection_limit, String::new()),
                (&self.valid_until, String::new()),
                (&self.max_queries, attributes.max_queries_per_hour.to_string()),
                (&self.max_updates, attributes.max_updates_per_hour.to_string()),
                (&self.max_connections, attributes.max_connections_per_hour.to_string()),
                (&self.max_user_connections, attributes.max_user_connections.to_string()),
            ],
        };
        for (state, value) in fields {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        self.password.update(cx, |state, cx| state.set_value(typed, window, cx));
    }

    fn all_inputs(&self) -> [&gpui::Entity<InputState>; 7] {
        [&self.connection_limit, &self.valid_until, &self.max_queries, &self.max_updates, &self.max_connections, &self.max_user_connections, &self.password]
    }

    /// The attribute fields were edited: the working attributes follow, or the field says what is wrong.
    fn attributes_from_inputs(&mut self, cx: &mut Context<Self>) {
        let Some(working) = self.model.detail().map(|detail| detail.working.attributes.clone()) else { return };
        let read = |state: &gpui::Entity<InputState>, cx: &gpui::App| state.read(cx).value().trim().to_owned();
        let mut error = None;
        let attributes = match working {
            Attributes::Postgres(attributes) => {
                let limit = read(&self.connection_limit, cx);
                let until = read(&self.valid_until, cx);
                let connection_limit = if limit.is_empty() {
                    -1
                } else {
                    limit.parse::<i32>().ok().filter(|limit| *limit >= -1).unwrap_or_else(|| {
                        error = Some("The connection limit is -1 (none) or a whole number from 0.".to_owned());
                        attributes.connection_limit
                    })
                };
                Attributes::Postgres(PgAttributes { connection_limit, valid_until: Some(until).filter(|until| !until.is_empty()), ..attributes })
            }
            Attributes::Mysql(attributes) => {
                let mut number = |state: &gpui::Entity<InputState>, name: &str, was: u32| -> u32 {
                    let text = read(state, cx);
                    if text.is_empty() {
                        return 0;
                    }
                    text.parse::<u32>().unwrap_or_else(|_| {
                        error = Some(format!("{name} is a whole number (0 for no limit)."));
                        was
                    })
                };
                Attributes::Mysql(MyAttributes {
                    max_queries_per_hour: number(&self.max_queries, "Max queries per hour", attributes.max_queries_per_hour),
                    max_updates_per_hour: number(&self.max_updates, "Max updates per hour", attributes.max_updates_per_hour),
                    max_connections_per_hour: number(&self.max_connections, "Max connections per hour", attributes.max_connections_per_hour),
                    max_user_connections: number(&self.max_user_connections, "Max simultaneous connections", attributes.max_user_connections),
                    ..attributes
                })
            }
        };
        self.field_error = error;
        self.model.set_attributes(attributes);
    }

    /// A PostgreSQL flag was ticked or cleared.
    pub(super) fn set_pg_flag(&mut self, change: impl FnOnce(&mut PgAttributes), cx: &mut Context<Self>) {
        if let Some(Attributes::Postgres(mut attributes)) = self.model.detail().map(|detail| detail.working.attributes.clone()) {
            change(&mut attributes);
            self.model.set_attributes(Attributes::Postgres(attributes));
            cx.notify();
        }
    }

    pub(super) fn set_my_flag(&mut self, change: impl FnOnce(&mut MyAttributes), cx: &mut Context<Self>) {
        if let Some(Attributes::Mysql(mut attributes)) = self.model.detail().map(|detail| detail.working.attributes.clone()) {
            change(&mut attributes);
            self.model.set_attributes(Attributes::Mysql(attributes));
            cx.notify();
        }
    }

    /// PostgreSQL: "Remove the password" was ticked or cleared.
    pub(super) fn set_clear_password(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        if on {
            self.password.update(cx, |state, cx| state.set_value("", window, cx));
            self.model.set_password(PasswordChange::Clear);
        } else {
            self.model.set_password(PasswordChange::Keep);
        }
        cx.notify();
    }

    pub(super) fn set_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel == panel {
            return;
        }
        self.panel = panel;
        self.detail_filter.update(cx, |state, cx| state.set_value("", window, cx));
        self.refilter_detail(cx);
        if panel == Panel::Privileges {
            self.load_privileges_if_needed(cx);
        }
        cx.notify();
    }

    /// The roles of the Membership panel and the objects of the Privileges panel, after the filter.
    pub(super) fn refilter_detail(&mut self, cx: &mut Context<Self>) {
        let needle = self.detail_filter.read(cx).value().trim().to_lowercase();
        let matches = |text: &str| needle.is_empty() || text.to_lowercase().contains(&needle);
        let mysql = self.engine.is_mysql();
        self.roles_shown = match (self.model.loaded(), self.model.selected.as_ref()) {
            (Some(catalog), Some(selected)) => catalog
                .accounts
                .iter()
                .enumerate()
                // A MySQL account joins roles; PostgreSQL joins any role.
                .filter(|(_, row)| row.id != *selected && (!mysql || row.kind == AccountKind::Role) && matches(&row.id.to_string()))
                .map(|(at, _)| at)
                .collect(),
            _ => Vec::new(),
        };
        self.objects_shown = match self.model.detail().and_then(|detail| detail.privileges.ready()) {
            Some(privileges) => privileges.objects.iter().enumerate().filter(|(_, target)| matches(&target.label(mysql))).map(|(at, _)| at).collect(),
            None => Vec::new(),
        };
    }

    // ---- edits that need the catalog ------------------------------------------------------------------

    pub(super) fn toggle_role(&mut self, position: usize, cx: &mut Context<Self>) {
        let Some(role) = self.roles_shown.get(position).copied().and_then(|at| self.model.loaded().and_then(|catalog| catalog.accounts.get(at))).map(|row| row.id.clone()) else { return };
        self.notice = self.model.toggle_membership(&role).err().map(|error| (error.to_string(), true));
        cx.notify();
    }

    pub(super) fn set_privilege(&mut self, target: &GrantTarget, privilege: Privilege, on: bool, cx: &mut Context<Self>) {
        self.model.set_privilege(target, privilege, on);
        cx.notify();
    }

    pub(super) fn set_grant_option(&mut self, target: &GrantTarget, privilege: Privilege, on: bool, cx: &mut Context<Self>) {
        self.model.set_grant_option(target, privilege, on);
        cx.notify();
    }

    pub(super) fn set_all_privileges(&mut self, target: &GrantTarget, on: bool, cx: &mut Context<Self>) {
        self.model.set_all_privileges(target, on);
        cx.notify();
    }

    pub(super) fn select_object(&mut self, position: usize, cx: &mut Context<Self>) {
        let target = self
            .objects_shown
            .get(position)
            .copied()
            .and_then(|at| self.model.detail().and_then(|detail| detail.privileges.ready()).and_then(|privileges| privileges.objects.get(at)))
            .cloned();
        if target.is_some() {
            self.selected_object = target;
            cx.notify();
        }
    }

    // ---- the review -----------------------------------------------------------------------------------

    /// Whether anything is edited or typed.
    pub(in crate::app) fn has_edits(&self, cx: &gpui::App) -> bool {
        self.model.dirty() || !self.password.read(cx).value().is_empty()
    }

    /// How many changes the edits make.
    pub(super) fn pending(&self) -> usize {
        self.model.changes().len()
    }

    /// Turns the edits into a change-set and opens the review dialog on it. The passwords go to the review and are not kept here.
    pub(super) fn review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.stash_password(cx);
        if let Some(error) = self.field_error.clone() {
            self.notice = Some((error, true));
            return cx.notify();
        }
        let changes = self.model.changes();
        if changes.is_empty() {
            self.notice = Some(if self.model.has_privilege_edits() {
                ("The privileges are being read again: review in a moment.".to_owned(), false)
            } else {
                ("There is nothing to review: no change has been made.".to_owned(), false)
            });
            return cx.notify();
        }
        // The passwords belong to the review once it is open; if it could not open, they stay where they were typed.
        if self.open_review(changes, window, cx) {
            self.model.release_passwords();
            self.password.update(cx, |state, cx| state.set_value("", window, cx));
            self.notice = None;
        } else if self.notice.is_none() {
            self.notice = Some(("The changes could not be reviewed: see the message in the navigator, or close the dialog that is open.".to_owned(), true));
        }
        cx.notify();
    }

    /// Opens the review of `changes` (the form dialogs share it); whether it opened.
    pub(super) fn open_review(&mut self, changes: Vec<crate::sql_ddl::Change>, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let origin = self.origin();
        self.root.update(cx, |root, cx| root.open_object_review(origin, move |_, _| changes, window, cx)).unwrap_or(false)
    }

    /// Forgets every edit.
    pub(super) fn discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.discard_edits();
        self.selected_object = None;
        self.field_error = None;
        self.notice = None;
        self.fill_inputs(window, cx);
        cx.notify();
    }

    /// An apply on this connection ended (whatever it did): the catalog is read again, and the edits (all of them were in that
    /// apply) are gone.
    pub(in crate::app) fn after_apply(&mut self, source: Result<CatalogSource, AccountsError>, cx: &mut Context<Self>) {
        // What ran is now what the server holds, and the edits are measured from that again (`AccountsModel::set_catalog`,
        // `reconcile_grants`): what did not run (a statement after the one that failed, on a server that cannot roll back) is still pending.
        // The passwords went with the review and are not kept: a password change that did not run has to be typed again.
        self.reload_with(source, cx);
    }

    /// `render` calls this first: what could not be done without a window (filling the inputs after a reload) happens now.
    pub(super) fn settle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.fill_pending) {
            cx.defer_in(window, |this, window, cx| this.fill_inputs(window, cx));
        }
        if let Some((databases, current)) = self.pending_databases.take() {
            let select = self.database_select.clone();
            cx.defer_in(window, move |_, window, cx| {
                select.update(cx, |select, cx| {
                    select.set_items(databases, window, cx);
                    select.set_selected_value(&current, window, cx);
                });
            });
        }
    }

    pub(super) fn connection_name(&self, cx: &gpui::App) -> String {
        self.root.read_with(cx, |root, _| root.saved_connections.iter().find(|saved| saved.matches_id(&self.connection)).map(|saved| saved.name.clone())).ok().flatten().unwrap_or_else(|| "the connection".to_owned())
    }

    /// The account the selected row stands for is the one this connection logs in as.
    pub(super) fn is_login(&self, id: &AccountId) -> bool {
        self.model.is_login(id)
    }
}

impl Drop for AccountsView {
    /// Closing the tab leaves no password behind.
    fn drop(&mut self) {
        self.model.discard_edits();
    }
}

/// What the tests look at (the tab's state, not its pixels).
#[cfg(test)]
#[derive(Debug, Clone)]
pub(in crate::app) struct Snapshot {
    pub(in crate::app) rows: Vec<String>,
    pub(in crate::app) total: usize,
    pub(in crate::app) selected: Option<String>,
    pub(in crate::app) panel: Panel,
    pub(in crate::app) loading: bool,
    pub(in crate::app) error: Option<String>,
    pub(in crate::app) notice: Option<(String, bool)>,
    pub(in crate::app) database: String,
    pub(in crate::app) objects: Vec<String>,
    pub(in crate::app) privileges_ready: bool,
    pub(in crate::app) pending: usize,
    pub(in crate::app) drawn: [(usize, usize); 3],
    pub(in crate::app) password_typed: bool,
    /// The attributes the selected account should have after Apply, and what the numeric inputs show.
    pub(in crate::app) working: Option<Attributes>,
    pub(in crate::app) inputs: Vec<String>,
    pub(in crate::app) field_error: Option<String>,
    /// The roles the Membership panel lists.
    pub(in crate::app) roles: Vec<String>,
}

#[cfg(test)]
impl AccountsView {
    pub(in crate::app) fn snapshot(&self, cx: &gpui::App) -> Snapshot {
        let mysql = self.engine.is_mysql();
        let catalog = self.model.loaded();
        Snapshot {
            rows: self.model.shown.iter().filter_map(|at| catalog.and_then(|c| c.accounts.get(*at))).map(|row| row.id.to_string()).collect(),
            total: catalog.map_or(0, |catalog| catalog.accounts.len()),
            selected: self.model.selected.as_ref().map(ToString::to_string),
            panel: self.panel,
            loading: matches!(self.model.catalog, Load::Loading),
            error: match &self.model.catalog {
                Load::Failed(error) => Some(error.to_string()),
                _ => None,
            },
            notice: self.notice.clone(),
            database: self.model.database.clone(),
            objects: self
                .model
                .detail()
                .and_then(|detail| detail.privileges.ready())
                .map(|privileges| self.objects_shown.iter().filter_map(|at| privileges.objects.get(*at)).map(|target| target.label(mysql)).collect())
                .unwrap_or_default(),
            privileges_ready: self.model.detail().is_some_and(|detail| detail.privileges.ready().is_some()),
            pending: self.pending(),
            drawn: self.drawn.get(),
            password_typed: !self.password.read(cx).value().is_empty(),
            working: self.model.detail().map(|detail| detail.working.attributes.clone()),
            field_error: self.field_error.clone(),
            roles: self.roles_shown.iter().filter_map(|at| catalog.and_then(|c| c.accounts.get(*at))).map(|row| row.id.to_string()).collect(),
            inputs: [&self.connection_limit, &self.valid_until, &self.max_queries, &self.max_updates, &self.max_connections, &self.max_user_connections].iter().map(|state| state.read(cx).value().to_string()).collect(),
        }
    }
}

/// Captures drive the tab through these (the tab's own handlers need a click).
#[cfg(all(test, feature = "bench"))]
impl AccountsView {
    pub(in crate::app) fn capture_select(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.model.loaded().and_then(|catalog| catalog.accounts.iter().find(|row| row.id.to_string() == name)).map(|row| row.id.clone());
        if let Some(id) = id {
            self.select(&id, window, cx);
        }
    }

    pub(in crate::app) fn capture_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        self.set_panel(panel, window, cx);
    }

    pub(in crate::app) fn capture_password(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.password.update(cx, |state, cx| state.set_value(text.to_owned(), window, cx));
    }

    pub(in crate::app) fn capture_object(&mut self, label: &str, cx: &mut Context<Self>) {
        let mysql = self.engine.is_mysql();
        let target = self.model.detail().and_then(|detail| detail.privileges.ready()).and_then(|privileges| privileges.objects.iter().find(|target| target.label(mysql) == label)).cloned();
        self.selected_object = target;
        cx.notify();
    }

    pub(in crate::app) fn capture_privilege(&mut self, label: &str, privilege: &str, on: bool, option: bool, cx: &mut Context<Self>) {
        let mysql = self.engine.is_mysql();
        let target = self.model.detail().and_then(|detail| detail.privileges.ready()).and_then(|privileges| privileges.objects.iter().find(|target| target.label(mysql) == label)).cloned();
        if let Some(target) = target {
            if let Some(privilege) = Privilege::find(target.offered(mysql), privilege) {
                self.model.set_privilege(&target, privilege, on);
                if option {
                    self.model.set_grant_option(&target, privilege, true);
                }
            }
        }
        cx.notify();
    }

    pub(in crate::app) fn capture_flag(&mut self, change: impl FnOnce(&mut PgAttributes), cx: &mut Context<Self>) {
        self.set_pg_flag(change, cx);
    }

    pub(in crate::app) fn capture_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review(window, cx);
    }
}
