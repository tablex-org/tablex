//! Roles (PostgreSQL) and accounts (MySQL, MariaDB) as data, and the statements that change them (no GPUI).
//!
//! An [`AccountChange`] is one edit of the server's users and privileges: create, alter (attributes, password), drop,
//! reassign or drop what a role owns, membership in a role, grant and revoke on an object. It travels as
//! [`Change::Account`](crate::sql_ddl::Change::Account) through the same review, plan and apply path as every structure
//! change, and each dialect turns it into SQL with `DdlDialect::account_statements` ([`postgres`], [`mysql`]).
//!
//! **Passwords.** The text the review shows, copies and logs is generated with [`Reveal::Redacted`] and carries
//! `'********'` where a password goes. The statement with the real value ([`Reveal::Exec`]) is made on the apply's thread
//! only (PostgreSQL: a client-side SCRAM-SHA-256 verifier, see [`scram`]; MySQL and MariaDB: the plain text, which their
//! `IDENTIFIED BY` needs, so the connection should use TLS) and goes to the driver and nowhere else. See [`password`].
//!
//! * [`privilege`]: privileges, the objects they are granted on, and the grant / revoke difference of two sets;
//! * [`catalog`]: the queries that read roles, memberships and privileges, and what they return.

pub mod catalog;
mod mysql;
pub mod password;
mod postgres;
pub mod privilege;
mod saslprep;
pub mod scram;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::sql_ddl::{DdlDialect, DdlError, Severity};
pub(crate) use mysql::statements as mysql_statements;
pub use password::{Password, PasswordChange, Reveal, Scrubber, REDACTED_LITERAL};
pub(crate) use postgres::statements as postgres_statements;
pub use privilege::{GrantTarget, Privilege};

/// The servers whose users and privileges the editor handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountEngine {
    Postgres,
    Mysql,
    Mariadb,
}

impl AccountEngine {
    pub fn of(kind: crate::connections::ConnectionKind) -> Option<Self> {
        use crate::connections::ConnectionKind;
        match kind {
            ConnectionKind::Postgres => Some(Self::Postgres),
            ConnectionKind::Mysql => Some(Self::Mysql),
            ConnectionKind::Mariadb => Some(Self::Mariadb),
            ConnectionKind::Sqlite | ConnectionKind::Clickhouse | ConnectionKind::Mongodb | ConnectionKind::Redis => None,
        }
    }

    /// MySQL or MariaDB (`user@host` accounts, `IDENTIFIED BY`).
    pub fn is_mysql(self) -> bool {
        !matches!(self, Self::Postgres)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Mariadb => "MariaDB",
        }
    }

    /// What this server calls what an account is.
    pub fn noun(self) -> &'static str {
        if self.is_mysql() {
            "user"
        } else {
            "role"
        }
    }
}

/// A PostgreSQL role (`host` is `None`) or a MySQL / MariaDB account. A MariaDB role has no host either.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountId {
    pub name: String,
    pub host: Option<String>,
}

impl AccountId {
    pub fn role(name: &str) -> Self {
        Self { name: name.to_owned(), host: None }
    }

    pub fn account(name: &str, host: &str) -> Self {
        Self { name: name.to_owned(), host: Some(host.to_owned()) }
    }
}

impl std::fmt::Display for AccountId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.host {
            Some(host) => write!(f, "{}@{host}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// What an account is (MySQL and MariaDB keep roles beside users).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    #[default]
    User,
    Role,
}

impl AccountKind {
    pub fn noun(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Role => "role",
        }
    }
}

/// The attributes of a PostgreSQL role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PgAttributes {
    pub login: bool,
    pub superuser: bool,
    pub create_db: bool,
    pub create_role: bool,
    pub inherit: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    /// `-1`: no limit.
    pub connection_limit: i32,
    /// A timestamp as typed (`2027-01-01 00:00:00+00`); `None`: never expires.
    pub valid_until: Option<String>,
}

impl Default for PgAttributes {
    /// What `CREATE ROLE` makes without options.
    fn default() -> Self {
        Self {
            login: false,
            superuser: false,
            create_db: false,
            create_role: false,
            inherit: true,
            replication: false,
            bypass_rls: false,
            connection_limit: -1,
            valid_until: None,
        }
    }
}

/// The attributes of a MySQL or MariaDB account that the editor changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MyAttributes {
    /// `ACCOUNT LOCK`.
    pub locked: bool,
    /// `PASSWORD EXPIRE`: the account must choose a new password at its next login. An action, not a state: a
    /// change that sets it expires the password, and the catalog's own state is only shown.
    pub expire_password: bool,
    /// `MAX_QUERIES_PER_HOUR`, `0`: no limit.
    pub max_queries_per_hour: u32,
    pub max_updates_per_hour: u32,
    pub max_connections_per_hour: u32,
    pub max_user_connections: u32,
}

/// The attributes of an account, of the engine of the connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "engine", rename_all = "snake_case")]
pub enum Attributes {
    Postgres(PgAttributes),
    Mysql(MyAttributes),
}

/// `CREATE ROLE` / `CREATE USER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAccount {
    pub id: AccountId,
    pub kind: AccountKind,
    pub attributes: Attributes,
    /// Never serialized: a draft saved to disk must not hold a password.
    #[serde(skip)]
    pub password: PasswordChange,
}

/// `ALTER ROLE` / `ALTER USER`: the attributes `from` (what the catalog holds) `to` (what they should be).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlterAccount {
    pub id: AccountId,
    pub from: Attributes,
    pub to: Attributes,
    #[serde(skip)]
    pub password: PasswordChange,
    /// The account is the one the connection logs in as.
    pub own: bool,
}

/// `DROP ROLE` / `DROP USER`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropAccount {
    pub id: AccountId,
    pub kind: AccountKind,
    pub own: bool,
}

/// `member` in `role`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub role: AccountId,
    pub member: AccountId,
    /// `WITH ADMIN OPTION` (a grant only).
    pub admin_option: bool,
    pub own: bool,
}

/// `GRANT privileges ON target TO grantee`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub grantee: AccountId,
    pub target: GrantTarget,
    pub privileges: Vec<Privilege>,
    /// `WITH GRANT OPTION` (PostgreSQL; MySQL has `GRANT OPTION` among the privileges).
    pub with_grant_option: bool,
}

/// `REVOKE privileges ON target FROM grantee`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoke {
    pub grantee: AccountId,
    pub target: GrantTarget,
    pub privileges: Vec<Privilege>,
    /// `REVOKE GRANT OPTION FOR` (PostgreSQL): the privileges stay, the right to pass them on goes.
    pub grant_option_only: bool,
    pub own: bool,
}

/// One edit of the users and privileges of a server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "account_op", rename_all = "snake_case")]
pub enum AccountChange {
    Create(CreateAccount),
    Alter(AlterAccount),
    Drop(DropAccount),
    /// `REASSIGN OWNED BY from TO to` in `database` (PostgreSQL).
    ReassignOwned { from: AccountId, to: AccountId, database: String },
    /// `DROP OWNED BY role` in `database` (PostgreSQL): drops what the role owns there and revokes its privileges there.
    DropOwned { role: AccountId, database: String },
    GrantRole(Membership),
    RevokeRole(Membership),
    Grant(Grant),
    Revoke(Revoke),
}

/// Where an account change runs in a change-set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AccountPhase {
    Create,
    Alter,
    ReassignOwned,
    DropOwned,
    Drop,
}

impl AccountChange {
    pub fn phase(&self) -> AccountPhase {
        match self {
            Self::Create(_) => AccountPhase::Create,
            Self::Alter(_) | Self::GrantRole(_) | Self::RevokeRole(_) | Self::Grant(_) | Self::Revoke(_) => AccountPhase::Alter,
            Self::ReassignOwned { .. } => AccountPhase::ReassignOwned,
            Self::DropOwned { .. } => AccountPhase::DropOwned,
            Self::Drop(_) => AccountPhase::Drop,
        }
    }

    /// The password this change sets, if it does.
    pub fn password(&self) -> Option<&Password> {
        match self {
            Self::Create(create) => create.password.password(),
            Self::Alter(alter) => alter.password.password(),
            _ => None,
        }
    }

    /// One line for a list of changes.
    pub fn title(&self) -> String {
        let privileges = |privileges: &[Privilege]| privileges.iter().map(|privilege| privilege.name()).collect::<Vec<_>>().join(", ");
        match self {
            Self::Create(create) => format!("Create {} {}", create.kind.noun(), create.id),
            Self::Alter(alter) => format!("Alter {}", alter.id),
            Self::Drop(drop) => format!("Drop {} {}", drop.kind.noun(), drop.id),
            Self::ReassignOwned { from, to, .. } => format!("Reassign what {from} owns to {to}"),
            Self::DropOwned { role, .. } => format!("Drop what {role} owns"),
            Self::GrantRole(membership) => format!("Add {} to {}", membership.member, membership.role),
            Self::RevokeRole(membership) => format!("Remove {} from {}", membership.member, membership.role),
            Self::Grant(grant) => format!("Grant {} on {} to {}", privileges(&grant.privileges), target_title(&grant.target), grant.grantee),
            Self::Revoke(revoke) if revoke.grant_option_only => {
                format!("Revoke the grant option of {} on {} from {}", privileges(&revoke.privileges), target_title(&revoke.target), revoke.grantee)
            }
            Self::Revoke(revoke) => format!("Revoke {} on {} from {}", privileges(&revoke.privileges), target_title(&revoke.target), revoke.grantee),
        }
    }

    /// What to read before applying it. `mysql`: the server is MySQL or MariaDB (else PostgreSQL).
    pub fn warnings(&self, mysql: bool) -> Vec<(Severity, String)> {
        let own = |severity, what: &str| (severity, format!("This is the account this connection logs in as: {what}"));
        let mut found = Vec::new();
        match self {
            Self::Create(create) => {
                if mysql && create.kind == AccountKind::User && create.password == PasswordChange::Keep {
                    found.push((Severity::Caution, "The account is created without a password: anyone who can reach the server can log in as it.".to_owned()));
                }
                found.extend(password_warning(create.password.password().is_some(), mysql));
            }
            Self::Alter(alter) => {
                if alter.own {
                    found.push(own(Severity::Destructive, "locking it, removing its login, or changing its password can lock you out of the server."));
                }
                found.extend(password_warning(alter.password.password().is_some(), mysql));
                if mysql && alter.to == alter.from && alter.password == PasswordChange::Keep {
                    return found;
                }
            }
            Self::Drop(drop) => {
                found.push((
                    Severity::Destructive,
                    if mysql {
                        format!("Drops {} and its privileges. Sessions it has open stay connected until they end; new logins are refused.", drop.id)
                    } else {
                        format!("Drops the role {}. It fails while the role still owns objects or holds privileges in any database: reassign or drop what it owns first.", drop.id)
                    },
                ));
                if drop.own {
                    found.push(own(Severity::Destructive, "dropping it ends your access once this session closes."));
                }
            }
            Self::ReassignOwned { from, database, .. } => found.push((
                Severity::Caution,
                format!("Moves every object {from} owns in {database} (and the shared objects it owns) to another role. It does not reach the other databases of the server: repeat it in each."),
            )),
            Self::DropOwned { role, database } => found.push((
                Severity::Destructive,
                format!("Drops every object {role} owns in {database}, tables and their rows included, and revokes its privileges there. It does not reach the other databases of the server."),
            )),
            Self::Revoke(revoke) if revoke.own => found.push(own(Severity::Destructive, "revoking its privileges can remove your own access.")),
            Self::RevokeRole(membership) if membership.own => found.push(own(Severity::Destructive, "leaving the role can remove your own access.")),
            _ => {}
        }
        found
    }
}

fn password_warning(sets_password: bool, mysql: bool) -> Option<(Severity, String)> {
    (sets_password && mysql).then(|| {
        (
            Severity::Caution,
            "MySQL and MariaDB take the password as text in the statement: it travels to the server as typed. Use a TLS connection (the statement shown here hides it).".to_owned(),
        )
    })
}

fn target_title(target: &GrantTarget) -> String {
    match target {
        GrantTarget::Global => "all databases".to_owned(),
        GrantTarget::Database { name } => format!("database {name}"),
        GrantTarget::Schema { name } => format!("schema {name}"),
        GrantTarget::Table { schema, name } => format!("{schema}.{name}"),
    }
}

/// The statements of a change-set as the driver runs them: the same plan as the review's, with the statements of every
/// change that sets a password made again with [`Reveal::Exec`]. The text it holds is wiped when it is dropped.
pub struct Executable {
    planned: crate::sql_ddl::Planned,
    /// The entries whose statements hold a password (or its verifier).
    secret_entries: Vec<usize>,
    scrubber: Scrubber,
}

impl Executable {
    /// The plan to run.
    pub fn planned(&self) -> &crate::sql_ddl::Planned {
        &self.planned
    }

    /// Hides the passwords in the text of a server's answer.
    pub fn scrub(&self, text: &str) -> String {
        self.scrubber.scrub(text)
    }

    /// The server's complaint about `change` (the one whose statement failed), as it may be shown and logged: a change that carries a
    /// password does not have the server's echo of its statement shown at all, and whatever is left is scrubbed.
    pub fn clean_failure(&self, change: Option<usize>, text: &str) -> String {
        let carries = change.is_some_and(|change| self.secret_entries.iter().any(|&at| self.planned.entries[at].change == change));
        if carries {
            self.scrubber.scrub(&Scrubber::without_echo(text))
        } else {
            self.scrubber.scrub(text)
        }
    }
}

impl Drop for Executable {
    fn drop(&mut self) {
        for &at in &self.secret_entries {
            if let Ok(statements) = &mut self.planned.entries[at].statements {
                statements.iter_mut().for_each(password::wipe_string);
            }
        }
    }
}

/// The plan to run for `changes`, whose review plan is `planned`: nothing differs unless a change sets a password.
pub fn executable(dialect: &dyn DdlDialect, changes: &[crate::sql_ddl::Change], planned: &crate::sql_ddl::Planned) -> Result<Executable, DdlError> {
    let mut exec = Executable { planned: planned.clone(), secret_entries: Vec::new(), scrubber: Scrubber::default() };
    for at in 0..exec.planned.entries.len() {
        let entry = &exec.planned.entries[at];
        let Some(crate::sql_ddl::Change::Account(account)) = changes.get(entry.change) else { continue };
        let Some(password) = account.password() else { continue };
        let Ok(redacted) = &entry.statements else { continue };
        let revealed = dialect.account_statements(account, Reveal::Exec)?;
        if revealed.len() != redacted.len() {
            return Err(DdlError::Invalid("The statements of a password change differ in number between their shown and their executed text.".to_owned()));
        }
        password.with(|text| exec.scrubber.hide(text));
        for (shown, run) in redacted.iter().zip(&revealed) {
            exec.scrubber.hide_difference(shown, run);
        }
        exec.planned.entries[at].statements = Ok(revealed);
        exec.secret_entries.push(at);
    }
    Ok(exec)
}

/// The error of a password that was wiped before the statement was made.
pub(crate) fn password_forgotten() -> DdlError {
    DdlError::Invalid("The password was cleared after the last Apply or when the review was closed: type it again.".to_owned())
}
