//! PostgreSQL roles: `CREATE ROLE`, `ALTER ROLE`, `DROP ROLE`, `REASSIGN OWNED`, `DROP OWNED`, `GRANT role TO role` and
//! `GRANT` / `REVOKE` on databases, schemas and tables. Every statement is transactional.

use super::privilege::{Privilege, PG_DATABASE, PG_SCHEMA, PG_TABLE};
use super::{
    password_forgotten, AccountChange, AccountId, AlterAccount, Attributes, CreateAccount, Grant, GrantTarget, Membership, PasswordChange, PgAttributes,
    Revoke, Reveal, REDACTED_LITERAL,
};
use crate::sql_ddl::{checked_name, ident, literal, DdlDialect, DdlError};

/// A role name as an identifier. A new role's name is also checked as a name a person typed.
fn role(d: &dyn DdlDialect, id: &AccountId, new: bool) -> Result<String, DdlError> {
    if id.host.is_some() {
        return Err(DdlError::Invalid(format!("{id} is not a PostgreSQL role name: a role has no host.")));
    }
    if new {
        if id.name.starts_with("pg_") {
            return Err(DdlError::Invalid(format!("The role name \"{}\" is reserved: names starting with pg_ belong to the system.", id.name)));
        }
        return checked_name(d, "role", &id.name);
    }
    ident(d, "role", &id.name)
}

fn attributes(attributes: &Attributes) -> Result<&PgAttributes, DdlError> {
    match attributes {
        Attributes::Postgres(attributes) => Ok(attributes),
        Attributes::Mysql(_) => Err(DdlError::Invalid("These are not the attributes of a PostgreSQL role.".to_owned())),
    }
}

fn flag(on: bool, keyword: &str) -> String {
    if on {
        keyword.to_owned()
    } else {
        format!("NO{keyword}")
    }
}

/// `VALID UNTIL 'text'`. The text is a timestamp as typed; a backslash is refused (a server with
/// `standard_conforming_strings` off would read it as an escape, and no timestamp has one).
fn valid_until(d: &dyn DdlDialect, until: Option<&str>) -> Result<String, DdlError> {
    let text = until.map(str::trim).filter(|text| !text.is_empty()).unwrap_or("infinity");
    if text.contains('\\') || text.contains('\0') {
        return Err(DdlError::Invalid(format!("\"{}\" is not a timestamp: use a form like 2027-01-01 00:00:00+00, or infinity.", text.escape_debug())));
    }
    Ok(format!("VALID UNTIL {}", literal(d, text)))
}

fn limit(connection_limit: i32) -> Result<String, DdlError> {
    if connection_limit < -1 {
        return Err(DdlError::Invalid("The connection limit is -1 (none) or a number from 0.".to_owned()));
    }
    Ok(format!("CONNECTION LIMIT {connection_limit}"))
}

/// `PASSWORD '...'`: the placeholder, or the SCRAM-SHA-256 verifier of the password (the plain text never
/// goes into a statement).
fn password(d: &dyn DdlDialect, change: &PasswordChange, reveal: Reveal) -> Result<Option<String>, DdlError> {
    Ok(match change {
        PasswordChange::Keep => None,
        PasswordChange::Clear => Some("PASSWORD NULL".to_owned()),
        PasswordChange::Set(password) => {
            let text = password
                .with(|text| match (text.is_empty(), reveal) {
                    (true, _) => Err(DdlError::Invalid("The password is empty: PostgreSQL treats that as no password. Remove the password instead.".to_owned())),
                    (false, Reveal::Redacted) => Ok(REDACTED_LITERAL.to_owned()),
                    (false, Reveal::Exec) => Ok(literal(d, &super::scram::new_verifier(text))),
                })
                .ok_or_else(password_forgotten)??;
            Some(format!("PASSWORD {text}"))
        }
    })
}

fn create(d: &dyn DdlDialect, create: &CreateAccount, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    let attributes = attributes(&create.attributes)?;
    let mut options = vec![flag(attributes.login, "LOGIN")];
    let defaults = PgAttributes::default();
    for (on, default, keyword) in [
        (attributes.superuser, defaults.superuser, "SUPERUSER"),
        (attributes.create_db, defaults.create_db, "CREATEDB"),
        (attributes.create_role, defaults.create_role, "CREATEROLE"),
        (attributes.inherit, defaults.inherit, "INHERIT"),
        (attributes.replication, defaults.replication, "REPLICATION"),
        (attributes.bypass_rls, defaults.bypass_rls, "BYPASSRLS"),
    ] {
        if on != default {
            options.push(flag(on, keyword));
        }
    }
    if attributes.connection_limit != defaults.connection_limit {
        options.push(limit(attributes.connection_limit)?);
    }
    options.extend(password(d, &create.password, reveal)?);
    if attributes.valid_until.as_deref().is_some_and(|until| !until.trim().is_empty()) {
        options.push(valid_until(d, attributes.valid_until.as_deref())?);
    }
    Ok(vec![format!("CREATE ROLE {} WITH {}", role(d, &create.id, true)?, options.join(" "))])
}

fn alter(d: &dyn DdlDialect, alter: &AlterAccount, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    let (from, to) = (attributes(&alter.from)?, attributes(&alter.to)?);
    let mut options = Vec::new();
    for (was, now, keyword) in [
        (from.login, to.login, "LOGIN"),
        (from.superuser, to.superuser, "SUPERUSER"),
        (from.create_db, to.create_db, "CREATEDB"),
        (from.create_role, to.create_role, "CREATEROLE"),
        (from.inherit, to.inherit, "INHERIT"),
        (from.replication, to.replication, "REPLICATION"),
        (from.bypass_rls, to.bypass_rls, "BYPASSRLS"),
    ] {
        if was != now {
            options.push(flag(now, keyword));
        }
    }
    if from.connection_limit != to.connection_limit {
        options.push(limit(to.connection_limit)?);
    }
    options.extend(password(d, &alter.password, reveal)?);
    let until = |attributes: &PgAttributes| attributes.valid_until.as_deref().map(str::trim).filter(|text| !text.is_empty()).map(str::to_owned);
    if until(from) != until(to) {
        options.push(valid_until(d, until(to).as_deref())?);
    }
    if options.is_empty() {
        return Ok(Vec::new());
    }
    Ok(vec![format!("ALTER ROLE {} WITH {}", role(d, &alter.id, false)?, options.join(" "))])
}

fn target(d: &dyn DdlDialect, target: &GrantTarget) -> Result<String, DdlError> {
    Ok(match target {
        GrantTarget::Global => return Err(DdlError::Unsupported("PostgreSQL has no server-wide grant: grant on a database, a schema or a table.".to_owned())),
        GrantTarget::Database { name } => format!("DATABASE {}", ident(d, "database", name)?),
        GrantTarget::Schema { name } => format!("SCHEMA {}", ident(d, "schema", name)?),
        GrantTarget::Table { schema, name } => format!("TABLE {}.{}", ident(d, "schema", schema)?, ident(d, "table", name)?),
    })
}

fn privileges(on: &GrantTarget, list: &[Privilege]) -> Result<String, DdlError> {
    let offered = match on {
        GrantTarget::Database { .. } => PG_DATABASE,
        GrantTarget::Schema { .. } => PG_SCHEMA,
        _ => PG_TABLE,
    };
    if list.is_empty() {
        return Err(DdlError::Invalid("No privilege is chosen.".to_owned()));
    }
    if let Some(unknown) = list.iter().find(|privilege| !offered.contains(privilege)) {
        return Err(DdlError::Invalid(format!("{unknown} is not a privilege PostgreSQL grants on this kind of object.")));
    }
    Ok(list.iter().map(|privilege| privilege.name()).collect::<Vec<_>>().join(", "))
}

fn grant(d: &dyn DdlDialect, grant: &Grant) -> Result<Vec<String>, DdlError> {
    Ok(vec![format!(
        "GRANT {} ON {} TO {}{}",
        privileges(&grant.target, &grant.privileges)?,
        target(d, &grant.target)?,
        role(d, &grant.grantee, false)?,
        if grant.with_grant_option { " WITH GRANT OPTION" } else { "" }
    )])
}

fn revoke(d: &dyn DdlDialect, revoke: &Revoke) -> Result<Vec<String>, DdlError> {
    Ok(vec![format!(
        "REVOKE {}{} ON {} FROM {}",
        if revoke.grant_option_only { "GRANT OPTION FOR " } else { "" },
        privileges(&revoke.target, &revoke.privileges)?,
        target(d, &revoke.target)?,
        role(d, &revoke.grantee, false)?
    )])
}

fn membership(d: &dyn DdlDialect, membership: &Membership, grant: bool) -> Result<Vec<String>, DdlError> {
    let (role_sql, member_sql) = (role(d, &membership.role, false)?, role(d, &membership.member, false)?);
    Ok(vec![if grant {
        format!("GRANT {role_sql} TO {member_sql}{}", if membership.admin_option { " WITH ADMIN OPTION" } else { "" })
    } else {
        format!("REVOKE {role_sql} FROM {member_sql}")
    }])
}

pub(crate) fn statements(d: &dyn DdlDialect, change: &AccountChange, reveal: Reveal) -> Result<Vec<String>, DdlError> {
    match change {
        AccountChange::Create(create_account) => create(d, create_account, reveal),
        AccountChange::Alter(alter_account) => alter(d, alter_account, reveal),
        AccountChange::Drop(drop) => Ok(vec![format!("DROP ROLE {}", role(d, &drop.id, false)?)]),
        AccountChange::ReassignOwned { from, to, .. } => Ok(vec![format!("REASSIGN OWNED BY {} TO {}", role(d, from, false)?, role(d, to, false)?)]),
        AccountChange::DropOwned { role: owner, .. } => Ok(vec![format!("DROP OWNED BY {}", role(d, owner, false)?)]),
        AccountChange::GrantRole(m) => membership(d, m, true),
        AccountChange::RevokeRole(m) => membership(d, m, false),
        AccountChange::Grant(g) => grant(d, g),
        AccountChange::Revoke(r) => revoke(d, r),
    }
}
