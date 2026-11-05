//! The state of the Users & Privileges tab, without a window: the roles that were read, the one selected, what has been
//! edited (kept per account, so switching between accounts loses nothing) and the change-set those edits make.
//!
//! An edit is a difference from what the catalog holds: a checkbox put back to its first state is no edit at all, and what
//! Apply sends is only the difference (`changes`), as typed [`Change`]s the shared review dialog shows.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::sql_account::catalog::{AccountCatalog, AccountRow, ForeignGrant, PrivilegeCatalog, ReadError};
use crate::sql_account::privilege::{self, PrivSet};
use crate::sql_account::{
    AccountChange, AccountEngine, AccountId, AlterAccount, Attributes, Grant, GrantTarget, Membership, PasswordChange, Privilege, Revoke,
};
use crate::sql_ddl::Change;

/// Something read in the background.
#[derive(Debug, Clone, Default)]
pub(in crate::app) enum Load<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(ReadError),
}

impl<T> Load<T> {
    pub(in crate::app) fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// Why a membership cannot be taken back from the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::app) enum MembershipError {
    /// Another role granted it: only that role takes it back.
    GrantedBy { role: AccountId, grantor: String },
}

impl std::fmt::Display for MembershipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GrantedBy { role, grantor } => write!(f, "The membership in {role} was granted by {grantor}: only {grantor} (or a superuser acting as it) can take it back."),
        }
    }
}

impl std::error::Error for MembershipError {}

/// What an account should be after Apply, edited from what the catalog holds.
#[derive(Debug, Clone)]
pub(in crate::app) struct Working {
    pub(in crate::app) attributes: Attributes,
    pub(in crate::app) password: PasswordChange,
    /// The roles the account should (`true`) or should not (`false`) belong to, for the ones that were toggled.
    pub(in crate::app) member_of: BTreeMap<AccountId, bool>,
    /// The privileges the account should hold, for the objects that were edited.
    pub(in crate::app) grants: BTreeMap<GrantTarget, PrivSet>,
}

/// One account the user has opened.
pub(in crate::app) struct Detail {
    pub(in crate::app) privileges: Load<Arc<PrivilegeCatalog>>,
    pub(in crate::app) working: Working,
    original: Attributes,
}

impl Detail {
    fn new(row: &AccountRow) -> Self {
        Self {
            privileges: Load::Idle,
            original: row.attributes.clone(),
            working: Working { attributes: row.attributes.clone(), password: PasswordChange::Keep, member_of: BTreeMap::new(), grants: BTreeMap::new() },
        }
    }

    fn dirty(&self) -> bool {
        self.working.attributes != self.original
            || self.working.password != PasswordChange::Keep
            || !self.working.member_of.is_empty()
            || !self.working.grants.is_empty()
    }

    /// The privileges held on `target` now.
    pub(in crate::app) fn held(&self, target: &GrantTarget) -> PrivSet {
        self.privileges.ready().and_then(|catalog| catalog.held.get(target)).cloned().unwrap_or_default()
    }

    /// The privileges on `target` that other roles granted and this connection's role cannot take back.
    pub(in crate::app) fn foreign(&self, target: &GrantTarget) -> Vec<&ForeignGrant> {
        self.privileges.ready().map(|catalog| catalog.foreign.iter().filter(|grant| grant.target == *target).collect()).unwrap_or_default()
    }

    /// The privileges `target` should hold after Apply.
    pub(in crate::app) fn wanted(&self, target: &GrantTarget) -> PrivSet {
        self.working.grants.get(target).cloned().unwrap_or_else(|| self.held(target))
    }
}

/// Everything the tab shows and edits.
pub(in crate::app) struct AccountsModel {
    pub(in crate::app) engine: AccountEngine,
    pub(in crate::app) catalog: Load<Arc<AccountCatalog>>,
    pub(in crate::app) selected: Option<AccountId>,
    /// The roles / accounts that match the filter, as positions in the catalog.
    pub(in crate::app) shown: Vec<usize>,
    filter: String,
    details: BTreeMap<AccountId, Detail>,
    /// PostgreSQL: the database the privileges are read and granted in.
    pub(in crate::app) database: String,
}

impl AccountsModel {
    pub(in crate::app) fn new(engine: AccountEngine) -> Self {
        Self { engine, catalog: Load::Idle, selected: None, shown: Vec::new(), filter: String::new(), details: BTreeMap::new(), database: String::new() }
    }

    pub(in crate::app) fn loaded(&self) -> Option<&Arc<AccountCatalog>> {
        self.catalog.ready()
    }

    /// The catalog was read: what was selected stays selected when it still exists. Edits of accounts that are gone are dropped.
    pub(in crate::app) fn set_catalog(&mut self, catalog: AccountCatalog) {
        if self.database.is_empty() {
            self.database = catalog.database.clone();
        }
        let catalog = Arc::new(catalog);
        self.details.retain(|id, _| catalog.account(id).is_some());
        // What an account is now is what its edits are measured from; an account without edits starts over from it.
        for (id, detail) in &mut self.details {
            let Some(row) = catalog.account(id) else { continue };
            if detail.dirty() {
                detail.original = row.attributes.clone();
                // A role the account was to join (or leave) that it has joined (left) since is no edit.
                let member_of: Vec<&AccountId> = catalog.roles_of(id).map(|membership| &membership.role).collect();
                detail.working.member_of.retain(|role, wanted| member_of.contains(&role) != *wanted);
            } else {
                *detail = Detail::new(row);
            }
        }
        if self.selected.as_ref().is_some_and(|id| catalog.account(id).is_none()) {
            self.selected = None;
        }
        self.catalog = Load::Ready(catalog);
        self.refilter();
    }

    pub(in crate::app) fn set_filter(&mut self, text: &str) {
        if self.filter != text {
            self.filter = text.to_owned();
            self.refilter();
        }
    }

    fn refilter(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        self.shown = match self.loaded() {
            Some(catalog) => catalog
                .accounts
                .iter()
                .enumerate()
                .filter(|(_, row)| needle.is_empty() || row.id.to_string().to_lowercase().contains(&needle))
                .map(|(at, _)| at)
                .collect(),
            None => Vec::new(),
        };
    }

    pub(in crate::app) fn row_at(&self, position: usize) -> Option<&AccountRow> {
        let catalog = self.loaded()?;
        catalog.accounts.get(*self.shown.get(position)?)
    }

    pub(in crate::app) fn selected_row(&self) -> Option<&AccountRow> {
        self.loaded()?.account(self.selected.as_ref()?)
    }

    /// Opens `id`: its edits start from the catalog's attributes.
    pub(in crate::app) fn select(&mut self, id: &AccountId) {
        let Some(row) = self.loaded().and_then(|catalog| catalog.account(id)).cloned() else { return };
        self.details.entry(id.clone()).or_insert_with(|| Detail::new(&row));
        self.selected = Some(id.clone());
    }

    pub(in crate::app) fn detail(&self) -> Option<&Detail> {
        self.details.get(self.selected.as_ref()?)
    }

    pub(in crate::app) fn detail_mut(&mut self) -> Option<&mut Detail> {
        self.details.get_mut(self.selected.as_ref()?)
    }

    pub(in crate::app) fn detail_of(&mut self, id: &AccountId) -> Option<&mut Detail> {
        self.details.get_mut(id)
    }

    /// Whether the account the connection logs in as is `id`.
    pub(in crate::app) fn is_login(&self, id: &AccountId) -> bool {
        self.loaded().is_some_and(|catalog| catalog.login.as_ref() == Some(id))
    }

    // ---- edits -------------------------------------------------------------------------------------

    pub(in crate::app) fn set_attributes(&mut self, attributes: Attributes) {
        if let Some(detail) = self.detail_mut() {
            detail.working.attributes = attributes;
        }
    }

    pub(in crate::app) fn set_password(&mut self, password: PasswordChange) {
        if let Some(detail) = self.detail_mut() {
            detail.working.password = password;
        }
    }

    /// Whether the selected account belongs to `role` once the edits are applied.
    pub(in crate::app) fn is_member_of(&self, role: &AccountId) -> bool {
        let Some(selected) = &self.selected else { return false };
        match self.detail().and_then(|detail| detail.working.member_of.get(role)) {
            Some(wanted) => *wanted,
            None => self.loaded().is_some_and(|catalog| catalog.roles_of(selected).any(|membership| membership.role == *role)),
        }
    }

    /// Puts the selected account in `role` or takes it out. A membership that another role granted cannot be taken back by this
    /// connection's role (PostgreSQL's `REVOKE` only reaches its own grants): it stays, and the error says whose it is.
    pub(in crate::app) fn toggle_membership(&mut self, role: &AccountId) -> Result<(), MembershipError> {
        let Some(selected) = self.selected.clone() else { return Ok(()) };
        let existing = self.loaded().and_then(|catalog| catalog.roles_of(&selected).find(|membership| membership.role == *role)).cloned();
        let current = existing.is_some();
        let wanted = !self.is_member_of(role);
        if let Some(membership) = existing.filter(|membership| !membership.revocable && !wanted) {
            return Err(MembershipError::GrantedBy { role: role.clone(), grantor: membership.grantor });
        }
        if let Some(detail) = self.detail_mut() {
            if wanted == current {
                detail.working.member_of.remove(role);
            } else {
                detail.working.member_of.insert(role.clone(), wanted);
            }
        }
        Ok(())
    }

    /// Puts `privilege` on `target` for the selected account, or takes it off. Taking a privilege off also takes its
    /// grant option off.
    pub(in crate::app) fn set_privilege(&mut self, target: &GrantTarget, privilege: Privilege, on: bool) {
        self.edit_grants(target, |wanted| {
            if on {
                wanted.entry(privilege).or_insert(false);
            } else {
                wanted.remove(&privilege);
            }
        });
    }

    /// PostgreSQL: `WITH GRANT OPTION` of a privilege that is held or being granted.
    pub(in crate::app) fn set_grant_option(&mut self, target: &GrantTarget, privilege: Privilege, on: bool) {
        self.edit_grants(target, |wanted| {
            if let Some(option) = wanted.get_mut(&privilege) {
                *option = on;
            }
        });
    }

    /// Every privilege the editor offers on `target`, or none of them.
    pub(in crate::app) fn set_all_privileges(&mut self, target: &GrantTarget, on: bool) {
        let offered = target.offered(self.engine.is_mysql());
        self.edit_grants(target, |wanted| {
            for privilege in offered {
                if on {
                    wanted.entry(*privilege).or_insert(false);
                } else {
                    wanted.remove(privilege);
                }
            }
        });
    }

    fn edit_grants(&mut self, target: &GrantTarget, edit: impl FnOnce(&mut PrivSet)) {
        let offered = target.offered(self.engine.is_mysql());
        let Some(detail) = self.detail_mut() else { return };
        if detail.privileges.ready().is_none() {
            return;
        }
        let held = detail.held(target);
        let mut wanted = detail.wanted(target);
        edit(&mut wanted);
        // Back to what the account holds (as far as the editor offers): no edit.
        let same = offered.iter().all(|privilege| held.get(privilege) == wanted.get(privilege));
        if same {
            detail.working.grants.remove(target);
        } else {
            detail.working.grants.insert(target.clone(), wanted);
        }
    }

    // ---- what is pending ----------------------------------------------------------------------------

    /// Whether `id` has edits.
    pub(in crate::app) fn is_edited(&self, id: &AccountId) -> bool {
        self.details.get(id).is_some_and(Detail::dirty)
    }

    /// Whether any account has edits.
    pub(in crate::app) fn dirty(&self) -> bool {
        self.details.values().any(Detail::dirty)
    }

    /// Whether the edits include a privilege (they are made in one database: PostgreSQL cannot grant across several).
    pub(in crate::app) fn has_privilege_edits(&self) -> bool {
        self.details.values().any(|detail| !detail.working.grants.is_empty())
    }

    /// Forgets every edit and wipes every password typed. The selected account stays selected, from what the catalog holds.
    pub(in crate::app) fn discard_edits(&mut self) {
        for detail in self.details.values() {
            detail.working.password.wipe();
        }
        self.details.clear();
        if let Some(id) = self.selected.clone() {
            self.select(&id);
        }
    }

    /// Takes the passwords out of the edits (they now belong to the review's change-set, which wipes them).
    pub(in crate::app) fn release_passwords(&mut self) {
        for detail in self.details.values_mut() {
            detail.working.password = PasswordChange::Keep;
        }
    }

    /// The accounts that have edits of privileges.
    pub(in crate::app) fn ids_with_privilege_edits(&self) -> Vec<AccountId> {
        self.details.iter().filter(|(_, detail)| !detail.working.grants.is_empty()).map(|(id, _)| id.clone()).collect()
    }

    /// Drops the privilege edits of `id` that the privileges it holds now already are (an apply that ran them).
    pub(in crate::app) fn reconcile_grants(&mut self, id: &AccountId) {
        let mysql = self.engine.is_mysql();
        let Some(detail) = self.details.get_mut(id) else { return };
        if detail.privileges.ready().is_none() {
            return;
        }
        let held: Vec<(GrantTarget, PrivSet)> = detail.working.grants.keys().map(|target| (target.clone(), detail.held(target))).collect();
        for (target, held) in held {
            let wanted = &detail.working.grants[&target];
            if target.offered(mysql).iter().all(|privilege| held.get(privilege) == wanted.get(privilege)) {
                detail.working.grants.remove(&target);
            }
        }
    }

    /// Forgets the privileges read for every account (the objects changed under them).
    pub(in crate::app) fn forget_privileges(&mut self) {
        for detail in self.details.values_mut() {
            detail.privileges = Load::Idle;
        }
    }

    /// The change-set of the edits, in account order; each account's attributes and password, then roles, then privileges.
    pub(in crate::app) fn changes(&self) -> Vec<Change> {
        let Some(catalog) = self.loaded() else { return Vec::new() };
        let mut changes = Vec::new();
        for (id, detail) in &self.details {
            let Some(row) = catalog.account(id) else { continue };
            let own = catalog.login.as_ref() == Some(id);
            let working = &detail.working;
            if working.attributes != detail.original || working.password != PasswordChange::Keep {
                changes.push(Change::Account(AccountChange::Alter(AlterAccount {
                    id: id.clone(),
                    from: row.attributes.clone(),
                    to: working.attributes.clone(),
                    password: working.password.clone(),
                    own,
                })));
            }
            for (role, wanted) in &working.member_of {
                let membership = Membership { role: role.clone(), member: id.clone(), admin_option: false, own };
                changes.push(Change::Account(if *wanted { AccountChange::GrantRole(membership) } else { AccountChange::RevokeRole(membership) }));
            }
            // The privileges held are known only once they are read: while they are read again (after an apply, a Refresh) what the edits
            // add and take away cannot be told, and a grant for everything would be wrong.
            for (target, wanted) in working.grants.iter().filter(|_| detail.privileges.ready().is_some()) {
                changes.extend(self.privilege_changes(id, own, target, &detail.held(target), wanted));
            }
        }
        changes
    }

    fn privilege_changes(&self, id: &AccountId, own: bool, target: &GrantTarget, held: &PrivSet, wanted: &PrivSet) -> Vec<Change> {
        let delta = privilege::diff(target.offered(self.engine.is_mysql()), held, wanted);
        let revoke = |privileges: Vec<Privilege>, grant_option_only: bool| {
            (!privileges.is_empty()).then(|| Change::Account(AccountChange::Revoke(Revoke { grantee: id.clone(), target: target.clone(), privileges, grant_option_only, own })))
        };
        let grant = |privileges: Vec<Privilege>, with_grant_option: bool| {
            (!privileges.is_empty())
                .then(|| Change::Account(AccountChange::Grant(Grant { grantee: id.clone(), target: target.clone(), privileges, with_grant_option })))
        };
        [revoke(delta.revoke, false), revoke(delta.revoke_option, true), grant(delta.grant, false), grant(delta.grant_with_option, true)].into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_account::catalog::{AccountRow, MembershipRow};
    use crate::sql_account::{AccountKind, MyAttributes, PgAttributes};

    fn row(name: &str) -> AccountRow {
        AccountRow {
            id: AccountId::role(name),
            kind: AccountKind::Role,
            attributes: Attributes::Postgres(PgAttributes { login: true, ..PgAttributes::default() }),
            plugin: None,
            password_expired: false,
        }
    }

    fn catalog() -> AccountCatalog {
        AccountCatalog {
            engine: AccountEngine::Postgres,
            accounts: vec![row("app"), row("boss"), row("reader")],
            memberships: vec![MembershipRow { role: AccountId::role("reader"), member: AccountId::role("app"), admin_option: false, grantor: "boss".into(), revocable: true }],
            login: Some(AccountId::role("boss")),
            database: "shop".into(),
            databases: vec!["shop".into()],
            sql_mode: None,
            version: String::new(),
        }
    }

    fn pg_model() -> AccountsModel {
        let mut model = AccountsModel::new(AccountEngine::Postgres);
        model.set_catalog(catalog());
        model
    }

    fn orders() -> GrantTarget {
        GrantTarget::Table { schema: "public".into(), name: "orders".into() }
    }

    fn with_privileges(model: &mut AccountsModel, held: &[(&str, bool)]) {
        let mut privileges = PrivilegeCatalog::default();
        privileges.held.insert(orders(), held.iter().map(|(name, option)| (Privilege::find(privilege::PG_TABLE, name).unwrap(), *option)).collect());
        model.detail_mut().unwrap().privileges = Load::Ready(Arc::new(privileges));
    }

    fn priv_of(name: &str) -> Privilege {
        Privilege::find(privilege::PG_TABLE, name).unwrap()
    }

    #[test]
    fn the_filter_matches_names_and_hosts_in_any_case() {
        let mut model = pg_model();
        assert_eq!(model.shown.len(), 3);
        model.set_filter("  OR");
        assert!(model.shown.is_empty());
        model.set_filter("RE");
        assert_eq!(model.shown.iter().map(|at| model.row_at(model.shown.iter().position(|s| s == at).unwrap()).unwrap().id.name.clone()).collect::<Vec<_>>(), ["reader"]);
        model.set_filter("");
        assert_eq!(model.shown, [0, 1, 2]);
    }

    #[test]
    fn an_edit_put_back_is_no_edit_and_a_membership_toggled_twice_is_none() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        assert!(!model.dirty());
        let Some(Attributes::Postgres(mut attributes)) = model.detail().map(|d| d.working.attributes.clone()) else { panic!() };
        attributes.superuser = true;
        model.set_attributes(Attributes::Postgres(attributes.clone()));
        assert!(model.dirty());
        attributes.superuser = false;
        model.set_attributes(Attributes::Postgres(attributes));
        assert!(!model.dirty());

        let boss = AccountId::role("boss");
        assert!(!model.is_member_of(&boss));
        model.toggle_membership(&boss).unwrap();
        assert!(model.is_member_of(&boss) && model.dirty());
        model.toggle_membership(&boss).unwrap();
        assert!(!model.dirty());
        // Leaving a role it belongs to, then joining again.
        let reader = AccountId::role("reader");
        assert!(model.is_member_of(&reader));
        model.toggle_membership(&reader).unwrap();
        assert!(!model.is_member_of(&reader) && model.dirty());
        model.toggle_membership(&reader).unwrap();
        assert!(!model.dirty());
    }

    #[test]
    fn a_membership_another_role_granted_cannot_be_taken_back_from_the_tab() {
        let mut model = AccountsModel::new(AccountEngine::Postgres);
        let mut found = catalog();
        found.memberships[0].revocable = false;
        found.memberships[0].grantor = "someone".into();
        model.set_catalog(found);
        model.select(&AccountId::role("app"));
        let reader = AccountId::role("reader");
        let error = model.toggle_membership(&reader).expect_err("not ours to take back");
        assert!(error.to_string().contains("granted by someone"), "{error}");
        assert!(model.is_member_of(&reader) && !model.dirty(), "it stays, and is no edit");
        // Joining another role is still an edit that can be put back.
        let boss = AccountId::role("boss");
        model.toggle_membership(&boss).unwrap();
        assert!(model.dirty());
        model.toggle_membership(&boss).unwrap();
        assert!(!model.dirty());
    }

    #[test]
    fn privilege_edits_become_one_revoke_and_one_grant_per_kind_of_change() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        with_privileges(&mut model, &[("SELECT", false), ("DELETE", true), ("UPDATE", false)]);
        model.set_privilege(&orders(), priv_of("INSERT"), true);
        model.set_privilege(&orders(), priv_of("DELETE"), false);
        model.set_grant_option(&orders(), priv_of("SELECT"), true);
        model.set_grant_option(&orders(), priv_of("UPDATE"), false);
        // No grant option on a privilege that is not held.
        model.set_grant_option(&orders(), priv_of("TRIGGER"), true);
        let titles: Vec<String> = model.changes().iter().map(Change::title).collect();
        assert_eq!(
            titles,
            [
                "Revoke DELETE on public.orders from app",
                "Grant INSERT on public.orders to app",
                "Grant SELECT on public.orders to app"
            ]
        );
        // Everything back to what it was: nothing is pending.
        model.set_privilege(&orders(), priv_of("INSERT"), false);
        model.set_privilege(&orders(), priv_of("DELETE"), true);
        model.set_grant_option(&orders(), priv_of("DELETE"), true);
        model.set_grant_option(&orders(), priv_of("SELECT"), false);
        assert!(model.changes().is_empty() && !model.dirty(), "{:?}", model.changes());
    }

    #[test]
    fn losing_a_grant_option_keeps_the_privilege_and_all_clears_and_sets_every_privilege() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        with_privileges(&mut model, &[("SELECT", true)]);
        model.set_grant_option(&orders(), priv_of("SELECT"), false);
        let titles: Vec<String> = model.changes().iter().map(Change::title).collect();
        assert_eq!(titles, ["Revoke the grant option of SELECT on public.orders from app"]);
        model.set_all_privileges(&orders(), true);
        assert_eq!(model.detail().unwrap().wanted(&orders()).len(), privilege::PG_TABLE.len());
        model.set_all_privileges(&orders(), false);
        assert_eq!(model.changes().iter().map(Change::title).collect::<Vec<_>>(), ["Revoke SELECT on public.orders from app"]);
    }

    #[test]
    fn grant_edits_make_no_change_while_the_privileges_are_read_again() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        with_privileges(&mut model, &[("SELECT", false), ("DELETE", false)]);
        model.set_privilege(&orders(), priv_of("DELETE"), false);
        model.set_privilege(&orders(), priv_of("INSERT"), true);
        let before: Vec<String> = model.changes().iter().map(Change::title).collect();
        assert_eq!(before, ["Revoke DELETE on public.orders from app", "Grant INSERT on public.orders to app"]);
        // After an apply or a Refresh the privileges are read again; the edits stay.
        model.forget_privileges();
        assert!(model.dirty() && model.changes().is_empty(), "not a grant of everything: {:?}", model.changes());
        model.detail_mut().unwrap().privileges = Load::Loading;
        assert!(model.changes().is_empty());
        with_privileges(&mut model, &[("SELECT", false), ("DELETE", false)]);
        assert_eq!(model.changes().iter().map(Change::title).collect::<Vec<_>>(), before, "the revoke is back once they are read");
    }

    #[test]
    fn nothing_is_edited_before_the_privileges_are_read() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        model.set_privilege(&orders(), priv_of("SELECT"), true);
        assert!(!model.dirty());
    }

    #[test]
    fn the_login_account_is_marked_on_the_changes_that_could_lock_it_out() {
        let mut model = pg_model();
        model.select(&AccountId::role("boss"));
        assert!(model.is_login(&AccountId::role("boss")));
        let Some(Attributes::Postgres(mut attributes)) = model.detail().map(|d| d.working.attributes.clone()) else { panic!() };
        attributes.login = false;
        model.set_attributes(Attributes::Postgres(attributes));
        model.toggle_membership(&AccountId::role("reader")).unwrap();
        let owns: Vec<bool> = model
            .changes()
            .iter()
            .map(|change| match change {
                Change::Account(AccountChange::Alter(alter)) => alter.own,
                Change::Account(AccountChange::GrantRole(m)) => m.own,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(owns, [true, true]);
    }

    #[test]
    fn discarding_wipes_the_passwords_and_dropping_the_catalog_keeps_the_edits_of_accounts_that_remain() {
        let mut model = pg_model();
        model.select(&AccountId::role("app"));
        let password = crate::sql_account::Password::new("x-needle");
        model.set_password(PasswordChange::Set(password.clone()));
        assert!(model.dirty());
        let mut smaller = catalog();
        smaller.accounts.retain(|row| row.id.name != "reader");
        model.set_catalog(smaller);
        assert!(model.dirty(), "app is still there");
        assert_eq!(model.shown.len(), 2);
        model.discard_edits();
        assert!(!model.dirty() && !password.is_present());
        // Releasing hands the password on without wiping it.
        model.select(&AccountId::role("app"));
        let again = crate::sql_account::Password::new("y-needle");
        model.set_password(PasswordChange::Set(again.clone()));
        model.release_passwords();
        assert!(again.is_present() && !model.dirty());
    }

    #[test]
    fn mysql_edits_make_one_alter_with_the_attributes_and_the_grant_option_is_a_privilege() {
        let mut model = AccountsModel::new(AccountEngine::Mysql);
        let id = AccountId::account("app", "%");
        model.set_catalog(AccountCatalog {
            engine: AccountEngine::Mysql,
            accounts: vec![AccountRow {
                id: id.clone(),
                kind: AccountKind::User,
                attributes: Attributes::Mysql(MyAttributes::default()),
                plugin: Some("caching_sha2_password".into()),
                password_expired: false,
            }],
            memberships: Vec::new(),
            login: None,
            database: String::new(),
            databases: Vec::new(),
            sql_mode: Some(String::new()),
            version: String::new(),
        });
        model.select(&id);
        model.set_attributes(Attributes::Mysql(MyAttributes { locked: true, ..MyAttributes::default() }));
        let database = GrantTarget::Database { name: "shop".into() };
        model.detail_mut().unwrap().privileges = Load::Ready(Arc::new(PrivilegeCatalog::default()));
        model.set_privilege(&database, Privilege::find(privilege::MYSQL_DATABASE, "SELECT").unwrap(), true);
        model.set_privilege(&database, Privilege::GRANT_OPTION, true);
        let titles: Vec<String> = model.changes().iter().map(Change::title).collect();
        assert_eq!(titles, ["Alter app@%", "Grant SELECT, GRANT OPTION on database shop to app@%"]);
    }
}
