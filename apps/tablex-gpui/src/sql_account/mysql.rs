//! MySQL and MariaDB accounts: `CREATE USER`, `ALTER USER`, `DROP USER`, `CREATE ROLE`, `DROP ROLE`, `GRANT role TO user`
//! and `GRANT` / `REVOKE` at the global, database and table level. None of it is transactional: each statement commits
//! as it runs.
//!
//! Account names are string literals (`'user'@'host'`) quoted as the server reads them: a backslash is doubled unless
//! `sql_mode` has `NO_BACKSLASH_ESCAPES` (the dialect knows). Database and table names are backtick identifiers.

use super::privilege::{Privilege, MYSQL_DATABASE, MYSQL_GLOBAL, MYSQL_TABLE};
use super::{
    password_forgotten, AccountChange, AccountId, AccountKind, AlterAccount, Attributes, CreateAccount, Grant, GrantTarget, Membership, MyAttributes,
    PasswordChange, Reveal, Revoke, REDACTED_LITERAL,
};
use crate::sql_ddl::{literal, DdlDialect, DdlError};

/// The longest user or role name MySQL accepts.
const MYSQL_NAME_LIMIT: usize = 32;

/// `'user'@'host'`; a MariaDB role (no host) is `'name'`, an account of MySQL without a host is `'name'@'%'`.
fn account(d: &dyn DdlDialect, id: &AccountId, mariadb: bool) -> Result<String, DdlError> {
    if id.name.contains('\0') || id.host.as_deref().is_some_and(|host| host.contains('\0')) {
        return Err(DdlError::Invalid(format!("The account name \"{}\" contains a NUL character.", id.to_string().escape_debug())));
    }
    Ok(match (&id.host, mariadb) {
        (Some(host), _) => format!("{}@{}", literal(d, &id.name), literal(d, host)),
        (None, true) => literal(d, &id.name),
        (None, false) => format!("{}@{}", literal(d, &id.name), literal(d, "%")),
    })
}

/// A name a person typed for a new account.
fn checked_new(id: &AccountId, kind: AccountKind, mariadb: bool) -> Result<(), DdlError> {
    if id.name.is_empty() {
        return Err(DdlError::Invalid(format!("The {} name is empty.", kind.noun())));
    }
    if id.name.chars().any(char::is_control) {
        return Err(DdlError::Invalid(format!("The {} name \"{}\" contains a control character.", kind.noun(), id.name.escape_debug())));
    }
    // MySQL refuses a longer name (MariaDB's limit depends on its version: its server says).
    if !mariadb && id.name.chars().count() > MYSQL_NAME_LIMIT {
        return Err(DdlError::Invalid(format!("The {} name is longer than the {MYSQL_NAME_LIMIT} characters MySQL allows.", kind.noun())));
    }
    match id.host.as_deref() {
        Some("") => return Err(DdlError::Invalid("The host is empty: use % for any host.".to_owned())),
        Some(host) if host.chars().any(char::is_control) => {
            return Err(DdlError::Invalid(format!("The host \"{}\" contains a control character.", host.escape_debug())));
        }
        Some(host) if host.chars().count() > 255 => return Err(DdlError::Invalid("The host is longer than 255 characters.".to_owned())),
        None if kind == AccountKind::User && mariadb => return Err(DdlError::Invalid("A user needs a host: use % for any host.".to_owned())),
        _ => {}
    }
    Ok(())
}

fn attributes(attributes: &Attributes) -> Result<&MyAttributes, DdlError> {
    match attributes {
        Attributes::Mysql(attributes) => Ok(attributes),
        Attributes::Postgres(_) => Err(DdlError::Invalid("These are not the attributes of a MySQL account.".to_owned())),
    }
}

/// `IDENTIFIED BY '...'`: the placeholder, or the password as the server's statement needs it.
fn identified(d: &dyn DdlDialect, change: &PasswordChange, reveal: Reveal) -> Result<Option<String>, DdlError> {
    Ok(match change {
        PasswordChange::Keep => None,
        PasswordChange::Clear => {
            return Err(DdlError::Unsupported("MySQL and MariaDB accounts cannot have the password removed here: set a new password.".to_owned()));
        }
        PasswordChange::Set(password) => {
            let text = password
                .with(|text| match (text.is_empty(), reveal) {
                    (true, _) => Err(DdlError::Invalid("The password is empty. Leave the password out to create an account without one.".to_owned())),
                    (false, Reveal::Redacted) => Ok(REDACTED_LITERAL.to_owned()),
                    (false, Reveal::Exec) => Ok(literal(d, text)),
                })
                .ok_or_else(password_forgotten)??;
            let clause = format!("IDENTIFIED BY {text}");
            // `text` is the password quoted as a literal: a copy of it that only this function holds.
            let mut text = text;
            super::password::wipe_string(&mut text);
            Some(clause)
        }
    })
}

/// `WITH MAX_QUERIES_PER_HOUR n ...` for the limits that `changed` picks.
fn limits(to: &MyAttributes, changed: impl Fn(u32, u32) -> bool, from: &MyAttributes) -> Option<String> {
    let parts: Vec<String> = [
        ("MAX_QUERIES_PER_HOUR", from.max_queries_per_hour, to.max_queries_per_hour),
        ("MAX_UPDATES_PER_HOUR", from.max_updates_per_hour, to.max_updates_per_hour),
        ("MAX_CONNECTIONS_PER_HOUR", from.max_connections_per_hour, to.max_connections_per_hour),
        ("MAX_USER_CONNECTIONS", from.max_user_connections, to.max_user_connections),
    ]
    .into_iter()
    .filter(|(_, was, now)| changed(*was, *now))
    .map(|(keyword, _, now)| format!("{keyword} {now}"))
    .collect();
    (!parts.is_empty()).then(|| format!("WITH {}", parts.join(" ")))
}

fn create(d: &dyn DdlDialect, create: &CreateAccount, mariadb: bool, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    checked_new(&create.id, create.kind, mariadb)?;
    let name = account(d, &create.id, mariadb)?;
    if create.kind == AccountKind::Role {
        return Ok(vec![format!("CREATE ROLE {name}")]);
    }
    let attributes = attributes(&create.attributes)?;
    let mut parts = vec![format!("CREATE USER {name}")];
    parts.extend(identified(d, &create.password, reveal)?);
    parts.extend(limits(attributes, |_, now| now != 0, &MyAttributes::default()));
    // MariaDB reads the lock before the password option; MySQL takes them in either order.
    if attributes.locked {
        parts.push("ACCOUNT LOCK".to_owned());
    }
    if attributes.expire_password {
        parts.push("PASSWORD EXPIRE".to_owned());
    }
    let sql = parts.join(" ");
    wipe_parts(&mut parts);
    Ok(vec![sql])
}

/// The clauses of a statement are copies of what the statement says (a password among them): wiped once the statement is joined.
fn wipe_parts(parts: &mut [String]) {
    parts.iter_mut().for_each(super::password::wipe_string);
}

fn alter(d: &dyn DdlDialect, alter: &AlterAccount, mariadb: bool, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    let (from, to) = (attributes(&alter.from)?, attributes(&alter.to)?);
    let mut parts = Vec::new();
    parts.extend(identified(d, &alter.password, reveal)?);
    parts.extend(limits(to, |was, now| was != now, from));
    if from.locked != to.locked {
        parts.push(if to.locked { "ACCOUNT LOCK" } else { "ACCOUNT UNLOCK" }.to_owned());
    }
    if to.expire_password {
        parts.push("PASSWORD EXPIRE".to_owned());
    }
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!("ALTER USER {} {}", account(d, &alter.id, mariadb)?, parts.join(" "));
    wipe_parts(&mut parts);
    Ok(vec![sql])
}

fn target(d: &dyn DdlDialect, target: &GrantTarget) -> Result<String, DdlError> {
    let named = |what: &str, name: &str| {
        if name.is_empty() || name.contains('\0') {
            return Err(DdlError::Invalid(format!("The {what} name is empty or has a NUL character.")));
        }
        Ok(d.quote(name))
    };
    Ok(match target {
        GrantTarget::Global => "*.*".to_owned(),
        // The pattern is kept as the server keeps it (`app\_%`): quoted as a name, its backslashes are the server's.
        GrantTarget::Database { name } => format!("{}.*", named("database", name)?),
        GrantTarget::Table { schema, name } => format!("{}.{}", named("database", schema)?, named("table", name)?),
        GrantTarget::Schema { .. } => return Err(DdlError::Unsupported("MySQL and MariaDB have databases, not schemas.".to_owned())),
    })
}

fn offered(on: &GrantTarget) -> &'static [Privilege] {
    match on {
        GrantTarget::Global => MYSQL_GLOBAL,
        GrantTarget::Database { .. } => MYSQL_DATABASE,
        _ => MYSQL_TABLE,
    }
}

fn check(on: &GrantTarget, list: &[Privilege]) -> Result<(), DdlError> {
    if list.is_empty() {
        return Err(DdlError::Invalid("No privilege is chosen.".to_owned()));
    }
    match list.iter().find(|privilege| !offered(on).contains(privilege)) {
        Some(unknown) => Err(DdlError::Invalid(format!("{unknown} is not a privilege MySQL grants on this kind of object."))),
        None => Ok(()),
    }
}

fn grant(d: &dyn DdlDialect, grant: &Grant, mariadb: bool) -> Result<Vec<String>, DdlError> {
    check(&grant.target, &grant.privileges)?;
    // `GRANT OPTION` is not in the list of a GRANT: it is the clause after it.
    let listed: Vec<&str> = grant.privileges.iter().filter(|privilege| **privilege != Privilege::GRANT_OPTION).map(|privilege| privilege.name()).collect();
    let option = grant.with_grant_option || grant.privileges.contains(&Privilege::GRANT_OPTION);
    Ok(vec![format!(
        "GRANT {} ON {} TO {}{}",
        if listed.is_empty() { "USAGE".to_owned() } else { listed.join(", ") },
        target(d, &grant.target)?,
        account(d, &grant.grantee, mariadb)?,
        if option { " WITH GRANT OPTION" } else { "" }
    )])
}

fn revoke(d: &dyn DdlDialect, revoke: &Revoke, mariadb: bool) -> Result<Vec<String>, DdlError> {
    if revoke.grant_option_only {
        return Err(DdlError::Unsupported("MySQL and MariaDB revoke GRANT OPTION as a privilege of its own.".to_owned()));
    }
    check(&revoke.target, &revoke.privileges)?;
    let list: Vec<&str> = revoke.privileges.iter().map(|privilege| privilege.name()).collect();
    Ok(vec![format!("REVOKE {} ON {} FROM {}", list.join(", "), target(d, &revoke.target)?, account(d, &revoke.grantee, mariadb)?)])
}

fn membership(d: &dyn DdlDialect, membership: &Membership, mariadb: bool, grant: bool) -> Result<Vec<String>, DdlError> {
    let (role, member) = (account(d, &membership.role, mariadb)?, account(d, &membership.member, mariadb)?);
    Ok(vec![if grant {
        format!("GRANT {role} TO {member}{}", if membership.admin_option { " WITH ADMIN OPTION" } else { "" })
    } else {
        format!("REVOKE {role} FROM {member}")
    }])
}

pub(crate) fn statements(d: &dyn DdlDialect, mariadb: bool, change: &AccountChange, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    match change {
        AccountChange::Create(create_account) => create(d, create_account, mariadb, reveal),
        AccountChange::Alter(alter_account) => alter(d, alter_account, mariadb, reveal),
        AccountChange::Drop(drop) => {
            let name = account(d, &drop.id, mariadb)?;
            Ok(vec![match drop.kind {
                AccountKind::User => format!("DROP USER {name}"),
                AccountKind::Role => format!("DROP ROLE {name}"),
            }])
        }
        AccountChange::ReassignOwned { .. } | AccountChange::DropOwned { .. } => {
            Err(DdlError::Unsupported("MySQL and MariaDB have no objects owned by an account to reassign: drop the account.".to_owned()))
        }
        AccountChange::GrantRole(m) => membership(d, m, mariadb, true),
        AccountChange::RevokeRole(m) => membership(d, m, mariadb, false),
        AccountChange::Grant(g) => grant(d, g, mariadb),
        AccountChange::Revoke(r) => revoke(d, r, mariadb),
    }
}
