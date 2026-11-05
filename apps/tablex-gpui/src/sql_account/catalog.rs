//! Reading the roles and accounts of a server, the roles they belong to, and the privileges they hold (no GPUI, no driver).
//!
//! The queries are plain SELECTs on the system catalogs, run through a [`CatalogSession`] (the driver's session in the
//! app, a script in the tests) that answers rows of text. Nothing here reads a password or a password hash: PostgreSQL
//! is read from `pg_roles` (which masks them), MySQL and MariaDB from `mysql.user` with explicit columns that leave
//! the hash out.
//!
//! * [`load_accounts`]: every role / account with its attributes, the memberships, who the connection logs in as and,
//!   for MySQL and MariaDB, the server's `sql_mode` (the review quotes text as that mode reads it);
//! * [`load_privileges`]: what one account holds, and the objects it could be granted something on.

use std::collections::BTreeMap;

use super::privilege::{PrivSet, MYSQL_DATABASE, MYSQL_GLOBAL, MYSQL_TABLE, PG_DATABASE, PG_SCHEMA, PG_TABLE};
use super::{AccountEngine, AccountId, AccountKind, Attributes, GrantTarget, MyAttributes, PgAttributes, Privilege};
use crate::sql_ddl::DdlDialect;

/// The most tables listed to grant on; the ones that hold a privilege are listed whatever the count.
pub const TABLE_LIMIT: usize = 5000;

/// Why the catalog could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The account the connection logs in as may not read the catalog (the server's own words).
    Denied(String),
    /// The query failed for another reason (the server's own words).
    Failed(String),
    /// A row did not have the shape the query asks for.
    Malformed(String),
}

impl ReadError {
    /// Sorts a server's message: a refusal of the privilege to read is `Denied`.
    pub fn from_server(message: String) -> Self {
        let lower = message.to_ascii_lowercase();
        if lower.contains("permission denied") || lower.contains("command denied") || lower.contains("access denied") || lower.contains("insufficient privilege") {
            Self::Denied(message)
        } else {
            Self::Failed(message)
        }
    }
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied(message) | Self::Failed(message) | Self::Malformed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ReadError {}

/// One row of text per row of the result, `None` for NULL.
pub type TextRows = Vec<Vec<Option<String>>>;

/// Runs one read and answers its rows as text.
pub trait CatalogSession {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError>;
}

/// One role or account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: AccountId,
    pub kind: AccountKind,
    pub attributes: Attributes,
    /// MySQL, MariaDB: the authentication plugin, shown read-only.
    pub plugin: Option<String>,
    /// MySQL, MariaDB: the password has expired (the state; changing it is an action).
    pub password_expired: bool,
}

/// `member` belongs to `role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipRow {
    pub role: AccountId,
    pub member: AccountId,
    pub admin_option: bool,
    /// PostgreSQL: who granted it. A grant is taken back only by the role that made it (a superuser takes back the ones the bootstrap
    /// superuser made).
    pub grantor: String,
    /// The account the connection logs in as can take it back (`REVOKE` finds it). Always true where the server keeps no grantor.
    pub revocable: bool,
}

/// Every role / account of the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountCatalog {
    pub engine: AccountEngine,
    pub accounts: Vec<AccountRow>,
    pub memberships: Vec<MembershipRow>,
    /// Who the connection logs in as.
    pub login: Option<AccountId>,
    /// PostgreSQL: the database the connection is in.
    pub database: String,
    /// PostgreSQL: the databases a connection can be made to.
    pub databases: Vec<String>,
    /// MySQL, MariaDB: `@@sql_mode`.
    pub sql_mode: Option<String>,
    /// The server's version text.
    pub version: String,
}

impl AccountCatalog {
    pub fn account(&self, id: &AccountId) -> Option<&AccountRow> {
        self.accounts.iter().find(|row| row.id == *id)
    }

    /// The roles `id` belongs to.
    pub fn roles_of<'a>(&'a self, id: &'a AccountId) -> impl Iterator<Item = &'a MembershipRow> {
        self.memberships.iter().filter(move |membership| membership.member == *id)
    }

    /// The members of `id`.
    pub fn members_of<'a>(&'a self, id: &'a AccountId) -> impl Iterator<Item = &'a MembershipRow> {
        self.memberships.iter().filter(move |membership| membership.role == *id)
    }
}

/// A privilege granted by a role whose grants the current role cannot take back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignGrant {
    pub target: GrantTarget,
    pub privilege: Privilege,
    pub grantor: String,
    pub grantable: bool,
}

/// What one account holds, and the objects privileges can be granted on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrivilegeCatalog {
    /// The privileges the editor offers, held per object.
    pub held: BTreeMap<GrantTarget, PrivSet>,
    /// Held privileges the editor does not offer (shown, never changed): the object and the privilege's name.
    pub other: Vec<(GrantTarget, String)>,
    /// Privileges the account holds that other roles granted: a `REVOKE` of the current role does not reach them (shown, never changed).
    pub foreign: Vec<ForeignGrant>,
    /// Column-level privileges (shown, never changed): `schema.table.column` and the privilege.
    pub columns: Vec<String>,
    /// The objects privileges can be granted on, in the order they are listed: the server (MySQL), databases, schemas, then tables.
    pub objects: Vec<GrantTarget>,
    /// More tables exist than [`TABLE_LIMIT`].
    pub tables_truncated: bool,
}

fn cell(row: &[Option<String>], at: usize) -> Result<Option<&str>, ReadError> {
    row.get(at).map(|cell| cell.as_deref()).ok_or_else(|| ReadError::Malformed(format!("a row has {} columns, not {}", row.len(), at + 1)))
}

fn text(row: &[Option<String>], at: usize) -> Result<String, ReadError> {
    Ok(cell(row, at)?.unwrap_or_default().to_owned())
}

fn flag(row: &[Option<String>], at: usize) -> Result<bool, ReadError> {
    Ok(cell(row, at)?.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "y" | "yes" | "on")))
}

fn number(row: &[Option<String>], at: usize) -> Result<i64, ReadError> {
    let value = cell(row, at)?.unwrap_or("0").trim();
    value.parse().map_err(|_| ReadError::Malformed(format!("\"{value}\" is not a number")))
}

fn limit(row: &[Option<String>], at: usize) -> Result<u32, ReadError> {
    Ok(number(row, at)?.clamp(0, i64::from(u32::MAX)) as u32)
}

// ---- accounts --------------------------------------------------------------------------------------------

const PG_ROLES: &str = "SELECT rolname, rolcanlogin::text, rolsuper::text, rolcreatedb::text, rolcreaterole::text, rolinherit::text, rolreplication::text, \
    rolbypassrls::text, rolconnlimit::text, rolvaliduntil::text FROM pg_catalog.pg_roles ORDER BY rolname";
/// What decides whether a `REVOKE role` of the current role reaches a membership: who granted it, and what the current role is. Whether it does
/// depends on the server's version ([`membership_revocable`]), so the facts are read, and the rule is applied in Rust.
const PG_MEMBERS: &str = "SELECT g.rolname, m.rolname, a.admin_option::text, (SELECT rolname FROM pg_catalog.pg_roles WHERE oid = a.grantor), \
    (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user)::text, \
    (a.grantor = (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = current_user))::text, \
    (a.grantor = 10)::text, \
    pg_catalog.pg_has_role(current_user, a.roleid, 'MEMBER WITH ADMIN OPTION')::text \
    FROM pg_catalog.pg_auth_members a \
    JOIN pg_catalog.pg_roles g ON g.oid = a.roleid JOIN pg_catalog.pg_roles m ON m.oid = a.member ORDER BY 1, 2";

/// Whether `REVOKE role FROM member` by the current role takes back a membership.
///
/// PostgreSQL 16 and later record who granted a membership and revoke only the current role's own grants (a superuser acts as the bootstrap
/// superuser, oid 10, and as itself where the server records the real grantor). Before 16 the grantor plays no part: a superuser, or a role
/// with the admin option on the group, takes any membership back.
fn membership_revocable(server_version_num: u32, superuser: bool, granted_by_current: bool, granted_by_bootstrap: bool, has_admin_option: bool) -> bool {
    if server_version_num >= 160_000 {
        if superuser {
            granted_by_bootstrap || granted_by_current
        } else {
            granted_by_current
        }
    } else {
        superuser || has_admin_option
    }
}

const PG_DATABASES: &str = "SELECT datname FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname";
const PG_SESSION: &str = "SELECT session_user::text, current_database()::text, version(), current_setting('server_version_num')";

const MYSQL_SESSION: &str = "SELECT CURRENT_USER(), @@sql_mode, VERSION()";
const MYSQL_USERS: &str = "SELECT User, Host, account_locked, password_expired, plugin, max_questions, max_updates, max_connections, max_user_connections, \
    (account_locked = 'Y' AND password_expired = 'Y' AND LENGTH(authentication_string) = 0) FROM mysql.user ORDER BY User, Host";
const MYSQL_EDGES: &str = "SELECT FROM_USER, FROM_HOST, TO_USER, TO_HOST, WITH_ADMIN_OPTION FROM mysql.role_edges ORDER BY 1, 2, 3, 4";
const MARIADB_USERS: &str = "SELECT User, Host, '', password_expired, plugin, max_questions, max_updates, max_connections, max_user_connections, is_role \
    FROM mysql.user ORDER BY User, Host";
const MARIADB_LOCKED: &str = "SELECT User, Host FROM mysql.global_priv WHERE JSON_VALUE(Priv, '$.account_locked') IN ('1', 'true')";
const MARIADB_EDGES: &str = "SELECT Role, User, Host, Admin_option FROM mysql.roles_mapping ORDER BY 1, 2, 3";

/// Every role / account, the memberships, and who the connection logs in as.
pub fn load_accounts(session: &mut dyn CatalogSession, engine: AccountEngine) -> Result<AccountCatalog, ReadError> {
    match engine {
        AccountEngine::Postgres => load_pg_accounts(session),
        AccountEngine::Mysql | AccountEngine::Mariadb => load_mysql_accounts(session, engine),
    }
}

fn load_pg_accounts(session: &mut dyn CatalogSession) -> Result<AccountCatalog, ReadError> {
    let accounts = session
        .rows(PG_ROLES)?
        .iter()
        .map(|row| {
            Ok(AccountRow {
                id: AccountId::role(&text(row, 0)?),
                kind: AccountKind::Role,
                attributes: Attributes::Postgres(PgAttributes {
                    login: flag(row, 1)?,
                    superuser: flag(row, 2)?,
                    create_db: flag(row, 3)?,
                    create_role: flag(row, 4)?,
                    inherit: flag(row, 5)?,
                    replication: flag(row, 6)?,
                    bypass_rls: flag(row, 7)?,
                    connection_limit: number(row, 8)?.clamp(-1, i64::from(i32::MAX)) as i32,
                    // `infinity` is "never": the same as none.
                    valid_until: cell(row, 9)?.map(str::to_owned).filter(|until| !until.eq_ignore_ascii_case("infinity")),
                }),
                plugin: None,
                password_expired: false,
            })
        })
        .collect::<Result<Vec<_>, ReadError>>()?;
    let session_row = session.rows(PG_SESSION)?;
    let first = session_row.first().ok_or_else(|| ReadError::Malformed("the server did not say who this connection is".to_owned()))?;
    let version_num = u32::try_from(number(first, 3)?).unwrap_or(0);
    let memberships = session
        .rows(PG_MEMBERS)?
        .iter()
        .map(|row| {
            Ok(MembershipRow {
                role: AccountId::role(&text(row, 0)?),
                member: AccountId::role(&text(row, 1)?),
                admin_option: flag(row, 2)?,
                grantor: text(row, 3)?,
                revocable: membership_revocable(version_num, flag(row, 4)?, flag(row, 5)?, flag(row, 6)?, flag(row, 7)?),
            })
        })
        .collect::<Result<Vec<_>, ReadError>>()?;
    let databases = session.rows(PG_DATABASES)?.iter().map(|row| text(row, 0)).collect::<Result<Vec<_>, ReadError>>()?;
    Ok(AccountCatalog {
        engine: AccountEngine::Postgres,
        accounts,
        memberships,
        login: Some(AccountId::role(&text(first, 0)?)),
        database: text(first, 1)?,
        databases,
        sql_mode: None,
        version: text(first, 2)?,
    })
}

/// `user@host` as `CURRENT_USER()` prints it: the host is after the last `@`.
fn split_current_user(current: &str) -> AccountId {
    match current.rsplit_once('@') {
        Some((name, host)) => AccountId::account(name, host),
        None => AccountId::role(current),
    }
}

fn load_mysql_accounts(session: &mut dyn CatalogSession, engine: AccountEngine) -> Result<AccountCatalog, ReadError> {
    let mariadb = engine == AccountEngine::Mariadb;
    let info = session.rows(MYSQL_SESSION)?;
    let first = info.first().ok_or_else(|| ReadError::Malformed("the server did not say who this connection is".to_owned()))?;
    let login = split_current_user(&text(first, 0)?);
    let (sql_mode, version) = (text(first, 1)?, text(first, 2)?);
    // MariaDB keeps the lock in the privilege document, not in a column of the user view (before 10.4 there is no lock).
    let locked: Vec<(String, String)> = if mariadb {
        match session.rows(MARIADB_LOCKED) {
            Ok(rows) => rows.iter().filter_map(|row| Some((row.first()?.clone()?, row.get(1)?.clone()?))).collect(),
            Err(ReadError::Denied(_)) | Err(ReadError::Failed(_)) => Vec::new(),
            Err(other) => return Err(other),
        }
    } else {
        Vec::new()
    };
    let accounts = session
        .rows(if mariadb { MARIADB_USERS } else { MYSQL_USERS })?
        .iter()
        .map(|row| {
            let (name, host) = (text(row, 0)?, text(row, 1)?);
            let is_role = flag(row, 9)?;
            // A MariaDB role has no host.
            let id = if mariadb && is_role { AccountId::role(&name) } else { AccountId::account(&name, &host) };
            let locked = if mariadb { locked.iter().any(|(user, at)| *user == name && *at == host) } else { flag(row, 2)? };
            Ok(AccountRow {
                kind: if is_role { AccountKind::Role } else { AccountKind::User },
                attributes: Attributes::Mysql(MyAttributes {
                    locked,
                    expire_password: false,
                    max_queries_per_hour: limit(row, 5)?,
                    max_updates_per_hour: limit(row, 6)?,
                    max_connections_per_hour: limit(row, 7)?,
                    max_user_connections: limit(row, 8)?,
                }),
                plugin: cell(row, 4)?.filter(|plugin| !plugin.is_empty()).map(str::to_owned),
                password_expired: flag(row, 3)?,
                id,
            })
        })
        .collect::<Result<Vec<_>, ReadError>>()?;
    let roles: Vec<&AccountId> = accounts.iter().filter(|row| row.kind == AccountKind::Role).map(|row| &row.id).collect();
    let memberships = if mariadb {
        session
            .rows(MARIADB_EDGES)?
            .iter()
            .map(|row| {
                let user = AccountId::account(&text(row, 1)?, &text(row, 2)?);
                // A role can be a member of a role: its row has no host.
                let member = if roles.iter().any(|role| role.name == user.name && user.host.as_deref() == Some("")) { AccountId::role(&user.name) } else { user };
                Ok(MembershipRow { role: AccountId::role(&text(row, 0)?), member, admin_option: flag(row, 3)?, grantor: String::new(), revocable: true })
            })
            .collect::<Result<Vec<_>, ReadError>>()?
    } else {
        match session.rows(MYSQL_EDGES) {
            Ok(rows) => rows
                .iter()
                .map(|row| {
                    Ok(MembershipRow {
                        role: AccountId::account(&text(row, 0)?, &text(row, 1)?),
                        member: AccountId::account(&text(row, 2)?, &text(row, 3)?),
                        admin_option: flag(row, 4)?,
                        grantor: String::new(),
                        revocable: true,
                    })
                })
                .collect::<Result<Vec<_>, ReadError>>()?,
            // Before 8.0 there are no roles.
            Err(ReadError::Failed(_)) => Vec::new(),
            Err(other) => return Err(other),
        }
    };
    Ok(AccountCatalog { engine, accounts, memberships, login: Some(login), database: String::new(), databases: Vec::new(), sql_mode: Some(sql_mode), version })
}

// ---- privileges ------------------------------------------------------------------------------------------

/// What `rows` of the held-privileges queries mean: the kind of object, its schema and name, the privilege, whether it can be passed on.
struct Held {
    target: GrantTarget,
    privilege: String,
    grantable: bool,
    /// Who granted it (PostgreSQL), and whether the current role can take it back.
    grantor: String,
    revocable: bool,
}

fn pg_held(rows: &TextRows) -> Result<Vec<Held>, ReadError> {
    rows.iter()
        .map(|row| {
            let (kind, schema, name) = (text(row, 0)?, text(row, 1)?, text(row, 2)?);
            let target = match kind.as_str() {
                "D" => GrantTarget::Database { name },
                "S" => GrantTarget::Schema { name },
                "T" => GrantTarget::Table { schema, name },
                other => return Err(ReadError::Malformed(format!("\"{other}\" is not a kind of object"))),
            };
            Ok(Held { target, privilege: text(row, 3)?, grantable: flag(row, 4)?, grantor: text(row, 5)?, revocable: flag(row, 6)? })
        })
        .collect()
}

fn mysql_held(kind: char, rows: &TextRows) -> Result<Vec<Held>, ReadError> {
    rows.iter()
        .map(|row| {
            let (target, at) = match kind {
                'G' => (GrantTarget::Global, 0),
                'D' => (GrantTarget::Database { name: text(row, 0)? }, 1),
                _ => (GrantTarget::Table { schema: text(row, 0)?, name: text(row, 1)? }, 2),
            };
            Ok(Held { target, privilege: text(row, at)?, grantable: flag(row, at + 1)?, grantor: String::new(), revocable: true })
        })
        .collect()
}

/// The privilege lists the held rows are matched against for `target`.
fn offered(engine: AccountEngine, target: &GrantTarget) -> &'static [Privilege] {
    match (engine.is_mysql(), target) {
        (true, GrantTarget::Global) => MYSQL_GLOBAL,
        (true, GrantTarget::Database { .. }) => MYSQL_DATABASE,
        (true, _) => MYSQL_TABLE,
        (false, GrantTarget::Database { .. }) => PG_DATABASE,
        (false, GrantTarget::Schema { .. }) => PG_SCHEMA,
        (false, _) => PG_TABLE,
    }
}

impl PrivilegeCatalog {
    /// Records one held privilege. MySQL and MariaDB report the grant option as `IS_GRANTABLE` of every privilege of the
    /// object: here it is the privilege `GRANT OPTION`, and `USAGE` (no privilege) is nothing.
    fn hold(&mut self, engine: AccountEngine, held: Held) {
        let mysql = engine.is_mysql();
        if mysql && held.grantable {
            self.held.entry(held.target.clone()).or_default().insert(Privilege::GRANT_OPTION, false);
        }
        if mysql && held.privilege.eq_ignore_ascii_case("USAGE") {
            return;
        }
        match Privilege::find(offered(engine, &held.target), &held.privilege) {
            // Granted by a role whose grants this one cannot take back: told, not editable.
            Some(privilege) if !held.revocable => {
                self.foreign.push(ForeignGrant { target: held.target, privilege, grantor: held.grantor, grantable: held.grantable });
            }
            Some(privilege) => {
                // Several grantors may have granted the same privilege: it is held with the grant option if any of the grants has it,
                // whatever order the rows come in.
                let option = self.held.entry(held.target).or_default().entry(privilege).or_insert(false);
                *option |= held.grantable && !mysql;
            }
            None => self.other.push((held.target, held.privilege)),
        }
    }
}

fn pg_target_lists(session: &mut dyn CatalogSession) -> Result<(Vec<GrantTarget>, bool), ReadError> {
    let mut objects = Vec::new();
    for row in session.rows("SELECT 'D', '', datname FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname")? {
        objects.push(GrantTarget::Database { name: text(&row, 2)? });
    }
    for row in session.rows(
        "SELECT 'S', '', nspname FROM pg_catalog.pg_namespace WHERE left(nspname, 3) <> 'pg_' AND nspname <> 'information_schema' ORDER BY nspname",
    )? {
        objects.push(GrantTarget::Schema { name: text(&row, 2)? });
    }
    let tables = session.rows(&format!(
        "SELECT 'T', n.nspname, c.relname FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND left(n.nspname, 3) <> 'pg_' AND n.nspname <> 'information_schema' \
         ORDER BY n.nspname, c.relname LIMIT {}",
        TABLE_LIMIT + 1
    ))?;
    let truncated = tables.len() > TABLE_LIMIT;
    for row in tables.iter().take(TABLE_LIMIT) {
        objects.push(GrantTarget::Table { schema: text(row, 1)?, name: text(row, 2)? });
    }
    Ok((objects, truncated))
}

/// A string literal PostgreSQL reads the same whatever `standard_conforming_strings` is on the session that runs it: a name with a backslash
/// is an escape string (`E'...'`, backslashes doubled), as a plain literal would take the backslash as an escape when that setting is off.
fn pg_literal(text: &str) -> String {
    if text.contains('\\') {
        format!("E'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
    } else {
        format!("'{}'", text.replace('\'', "''"))
    }
}

/// The columns of a held privilege: its grantor's name, and whether a `REVOKE` of the current role reaches it. A superuser's `REVOKE` acts as
/// the owner of the object, so it reaches the owner's grants only; any other role's reaches the grants of the roles it has the privileges of.
/// (For a role that is not a superuser this is an approximation: PostgreSQL picks the "best grantor" among the roles the current one has the
/// privileges of and that hold the grant option, which is not modelled; a grant made by a role the current one inherits is taken as reachable.)
fn grantor_columns(owner: &str) -> String {
    format!(
        "(SELECT rolname FROM pg_catalog.pg_roles WHERE oid = a.grantor), \
         (CASE WHEN (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) \
            THEN a.grantor = {owner} ELSE pg_catalog.pg_has_role(current_user, a.grantor, 'USAGE') END)::text"
    )
}

fn pg_held_sql(literal: &str) -> String {
    let grantee = format!("(SELECT oid FROM pg_catalog.pg_roles WHERE rolname = {literal})");
    let (database, schema, table) = (grantor_columns("d.datdba"), grantor_columns("n.nspowner"), grantor_columns("c.relowner"));
    format!(
        "SELECT 'D', '', d.datname, a.privilege_type, a.is_grantable::text, {database} FROM pg_catalog.pg_database d, \
           LATERAL aclexplode(COALESCE(d.datacl, acldefault('d', d.datdba))) a WHERE a.grantee = {grantee} AND NOT d.datistemplate \
         UNION ALL \
         SELECT 'S', '', n.nspname, a.privilege_type, a.is_grantable::text, {schema} FROM pg_catalog.pg_namespace n, \
           LATERAL aclexplode(COALESCE(n.nspacl, acldefault('n', n.nspowner))) a WHERE a.grantee = {grantee} \
           AND left(n.nspname, 3) <> 'pg_' AND n.nspname <> 'information_schema' \
         UNION ALL \
         SELECT 'T', n.nspname, c.relname, a.privilege_type, a.is_grantable::text, {table} FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace, \
           LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a \
         WHERE a.grantee = {grantee} AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND left(n.nspname, 3) <> 'pg_' AND n.nspname <> 'information_schema' \
         ORDER BY 1, 2, 3, 4, 6"
    )
}

/// What `id` holds, and the objects to grant on. `dialect` quotes the text the queries hold (an account name).
pub fn load_privileges(
    session: &mut dyn CatalogSession,
    dialect: &dyn DdlDialect,
    engine: AccountEngine,
    id: &AccountId,
) -> Result<PrivilegeCatalog, ReadError> {
    let mut catalog = PrivilegeCatalog::default();
    if engine == AccountEngine::Postgres {
        let literal = pg_literal(&id.name);
        for held in pg_held(&session.rows(&pg_held_sql(&literal))?)? {
            catalog.hold(engine, held);
        }
        let (objects, truncated) = pg_target_lists(session)?;
        catalog.objects = objects;
        catalog.tables_truncated = truncated;
    } else {
        let host = id.host.as_deref().unwrap_or("");
        let grantee = format!(
            "CONCAT('''', {}, '''@''', {}, '''')",
            crate::sql_ddl::literal(dialect, &id.name),
            crate::sql_ddl::literal(dialect, host)
        );
        for (kind, sql) in [
            ('G', format!("SELECT PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.USER_PRIVILEGES WHERE GRANTEE = {grantee}")),
            ('D', format!("SELECT TABLE_SCHEMA, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.SCHEMA_PRIVILEGES WHERE GRANTEE = {grantee}")),
            ('T', format!("SELECT TABLE_SCHEMA, TABLE_NAME, PRIVILEGE_TYPE, IS_GRANTABLE FROM information_schema.TABLE_PRIVILEGES WHERE GRANTEE = {grantee}")),
        ] {
            for held in mysql_held(kind, &session.rows(&sql)?)? {
                catalog.hold(engine, held);
            }
        }
        for row in session.rows(&format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, PRIVILEGE_TYPE FROM information_schema.COLUMN_PRIVILEGES WHERE GRANTEE = {grantee} ORDER BY 1, 2, 3, 4"
        ))? {
            catalog.columns.push(format!("{}.{}.{}: {}", text(&row, 0)?, text(&row, 1)?, text(&row, 2)?, text(&row, 3)?));
        }
        // The objects: the server, every database, the tables outside the system databases.
        catalog.objects.push(GrantTarget::Global);
        for row in session.rows("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME")? {
            catalog.objects.push(GrantTarget::Database { name: super::privilege::escape_pattern(&text(&row, 0)?) });
        }
        // A grant on a database pattern (`app\_%`) is an object of its own.
        let held_databases: Vec<GrantTarget> = catalog.held.keys().filter(|target| matches!(target, GrantTarget::Database { .. })).cloned().collect();
        for target in held_databases {
            if !catalog.objects.contains(&target) {
                catalog.objects.push(target);
            }
        }
        let tables = session.rows(&format!(
            "SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA NOT IN ('information_schema', 'performance_schema', 'mysql', 'sys') \
             ORDER BY TABLE_SCHEMA, TABLE_NAME LIMIT {}",
            TABLE_LIMIT + 1
        ))?;
        catalog.tables_truncated = tables.len() > TABLE_LIMIT;
        for row in tables.iter().take(TABLE_LIMIT) {
            catalog.objects.push(GrantTarget::Table { schema: text(row, 0)?, name: text(row, 1)? });
        }
    }
    // What is held on an object the lists above cut off is listed too.
    let missing: Vec<GrantTarget> = catalog.held.keys().filter(|target| !catalog.objects.contains(target)).cloned().collect();
    catalog.objects.extend(missing);
    Ok(catalog)
}
