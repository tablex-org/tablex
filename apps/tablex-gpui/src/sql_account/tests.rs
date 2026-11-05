use super::catalog::{load_accounts, load_privileges, CatalogSession, ReadError, TextRows, TABLE_LIMIT};
use super::privilege::{diff, escape_pattern, unescape_pattern, PrivSet, MYSQL_TABLE, PG_TABLE};
use super::*;
use crate::connections::ConnectionKind::{self, Mariadb, Mysql, Postgres};
use crate::sql_ddl::{dialect_for, dialect_in_mode, plan, warnings, Change, Severity};

const NEEDLE: &str = "hunter2-needle";

fn pg_attrs() -> PgAttributes {
    PgAttributes { login: true, ..PgAttributes::default() }
}

fn create_role(name: &str, password: PasswordChange) -> AccountChange {
    AccountChange::Create(CreateAccount { id: AccountId::role(name), kind: AccountKind::Role, attributes: Attributes::Postgres(pg_attrs()), password })
}

fn create_user(name: &str, host: &str, password: PasswordChange) -> AccountChange {
    AccountChange::Create(CreateAccount {
        id: AccountId::account(name, host),
        kind: AccountKind::User,
        attributes: Attributes::Mysql(MyAttributes::default()),
        password,
    })
}

fn set(text: &str) -> PasswordChange {
    PasswordChange::Set(Password::new(text))
}

/// The statements of `change` for `kind` in `sql_mode`, as the review shows them.
fn shown(kind: ConnectionKind, sql_mode: &str, change: &AccountChange) -> Vec<String> {
    dialect_in_mode(kind, sql_mode).expect("a SQL kind").account_statements(change, Reveal::Redacted).unwrap_or_else(|error| panic!("{change:?}: {error}"))
}

fn run(kind: ConnectionKind, sql_mode: &str, change: &AccountChange) -> Vec<String> {
    dialect_in_mode(kind, sql_mode).expect("a SQL kind").account_statements(change, Reveal::Exec).unwrap_or_else(|error| panic!("{change:?}: {error}"))
}

fn refused(kind: ConnectionKind, change: &AccountChange) -> String {
    dialect_for(kind).expect("a SQL kind").account_statements(change, Reveal::Redacted).expect_err("is refused").to_string()
}

fn pg(change: &AccountChange) -> Vec<String> {
    shown(Postgres, "", change)
}

fn my(change: &AccountChange) -> Vec<String> {
    shown(Mysql, "", change)
}

fn grant(grantee: AccountId, target: GrantTarget, privileges: &[&str], option: bool) -> AccountChange {
    let offered: Vec<Privilege> = [privilege::PG_DATABASE, privilege::PG_SCHEMA, privilege::PG_TABLE, privilege::MYSQL_GLOBAL, privilege::MYSQL_DATABASE, privilege::MYSQL_TABLE]
        .concat();
    AccountChange::Grant(Grant {
        grantee,
        target,
        privileges: privileges.iter().map(|name| Privilege::find(&offered, name).unwrap_or_else(|| panic!("{name}"))).collect(),
        with_grant_option: option,
    })
}

fn revoke(grantee: AccountId, target: GrantTarget, privileges: &[&str], option_only: bool) -> AccountChange {
    let AccountChange::Grant(Grant { privileges, .. }) = grant(grantee.clone(), target.clone(), privileges, false) else { unreachable!() };
    AccountChange::Revoke(Revoke { grantee, target, privileges, grant_option_only: option_only, own: false })
}

fn table(schema: &str, name: &str) -> GrantTarget {
    GrantTarget::Table { schema: schema.into(), name: name.into() }
}

// ---- PostgreSQL ---------------------------------------------------------------------------------------------

#[test]
fn a_new_postgres_role_lists_what_differs_from_the_defaults_and_hides_the_password() {
    let change = create_role("reporting", set(NEEDLE));
    assert_eq!(pg(&change), ["CREATE ROLE \"reporting\" WITH LOGIN PASSWORD '********'"]);
    let full = AccountChange::Create(CreateAccount {
        id: AccountId::role("admin"),
        kind: AccountKind::Role,
        attributes: Attributes::Postgres(PgAttributes {
            login: false,
            superuser: true,
            create_db: true,
            create_role: true,
            inherit: false,
            replication: true,
            bypass_rls: true,
            connection_limit: 5,
            valid_until: Some("2027-01-01 00:00:00+00".into()),
        }),
        password: PasswordChange::Keep,
    });
    assert_eq!(
        pg(&full),
        ["CREATE ROLE \"admin\" WITH NOLOGIN SUPERUSER CREATEDB CREATEROLE NOINHERIT REPLICATION BYPASSRLS CONNECTION LIMIT 5 VALID UNTIL '2027-01-01 00:00:00+00'"]
    );
    let plain = create_role("plain", PasswordChange::Keep);
    assert_eq!(pg(&plain), ["CREATE ROLE \"plain\" WITH LOGIN"]);
}

#[test]
fn the_postgres_password_that_runs_is_a_scram_verifier_and_never_the_text_typed() {
    let change = create_role("reporting", set(NEEDLE));
    let statements = run(Postgres, "", &change);
    assert_eq!(statements.len(), 1);
    assert!(!statements[0].contains(NEEDLE), "{}", statements[0]);
    let literal = statements[0].split("PASSWORD ").nth(1).expect("a password clause");
    let verifier = literal.trim_matches('\'');
    assert!(verifier.starts_with("SCRAM-SHA-256$4096:"), "{verifier}");
    // The verifier is the one that password and that salt make.
    let salt = verifier.split(['$', ':']).nth(2).expect("a salt");
    use base64::Engine as _;
    let decoded = base64::engine::general_purpose::STANDARD.decode(salt).expect("base64");
    assert_eq!(decoded.len(), scram::SALT_LEN);
    assert_eq!(scram::verifier(NEEDLE, &decoded, scram::ITERATIONS), verifier);
    // A salt of its own every time.
    assert_ne!(statements, run(Postgres, "", &change));
}

#[test]
fn scram_matches_the_rfc_7677_exchange() {
    // RFC 7677 section 3: user "user", password "pencil".
    use base64::Engine as _;
    let salt = base64::engine::general_purpose::STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
    let verifier = scram::verifier("pencil", &salt, 4096);
    let mut parts = verifier.strip_prefix("SCRAM-SHA-256$4096:").unwrap().split(['$', ':']);
    assert_eq!(parts.next(), Some("W22ZaJ0SNY7soEsUEjb6gQ=="));
    let stored = base64::engine::general_purpose::STANDARD.decode(parts.next().unwrap()).unwrap();
    let server_key = base64::engine::general_purpose::STANDARD.decode(parts.next().unwrap()).unwrap();
    let auth = "n=user,r=rOprNGfwEbeRWgbNEkqO,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096,c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0";
    use pbkdf2::hmac::{Hmac, Mac};
    let signature = |key: &[u8]| -> Vec<u8> {
        let mut mac = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).unwrap();
        mac.update(auth.as_bytes());
        mac.finalize().into_bytes().to_vec()
    };
    // The server's proof of itself ("v=") and the client's proof ("p=") of the RFC.
    assert_eq!(base64::engine::general_purpose::STANDARD.encode(signature(&server_key)), "6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=");
    let client_signature = signature(&stored);
    let proof = base64::engine::general_purpose::STANDARD.decode("dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ=").unwrap();
    let client_key: Vec<u8> = proof.iter().zip(&client_signature).map(|(p, s)| p ^ s).collect();
    use sha2::Digest as _;
    assert_eq!(sha2::Sha256::digest(&client_key).to_vec(), stored, "the stored key is the hash of the client key");
}

/// Whether the verifier made for `typed` is the one of exactly `hashed` (what PostgreSQL stores for the text it hashes).
fn hashes_as(typed: &str, hashed: &str) -> bool {
    scram::verifier(typed, &[7; 16], 4096) == scram::verifier_of_exactly(hashed, &[7; 16], 4096)
}

#[test]
fn saslprep_maps_and_normalizes_what_postgres_normalizes() {
    // ASCII, and text the preparation leaves as it is: as typed.
    assert!(hashes_as("plain-ASCII 123", "plain-ASCII 123"));
    assert!(hashes_as("p\u{e4}ssw\u{f6}rd", "p\u{e4}ssw\u{f6}rd"));
    // A no-break space is a space; a soft hyphen is nothing; a decomposed e-acute is the composed one (all assigned in Unicode 3.2).
    assert!(hashes_as("a\u{a0}b", "a b"));
    assert!(hashes_as("a\u{ad}b", "ab"));
    assert!(hashes_as("e\u{301}", "\u{e9}"));
    assert!(hashes_as("\u{fb01}x", "fix"));
    // A pure right-to-left string passes the bidirectional rule: normalized (the space between two Hebrew letters).
    assert!(hashes_as("\u{5e9}\u{a0}\u{5dc}", "\u{5e9} \u{5dc}"));
    // Arabic with a vowel mark (a non-spacing mark inside, letters at both ends): a right-to-left string like any other.
    assert!(hashes_as("\u{627}\u{a0}\u{64e}\u{628}", "\u{627} \u{64e}\u{628}"));
    // Full-width letters and a ligature are compatibility forms NFKC folds; so is the circled digit.
    assert!(hashes_as("\u{ff21}\u{ff22}\u{2460}", "AB1"));
}

#[test]
fn saslprep_falls_back_to_the_text_as_typed_where_postgres_does() {
    // Unassigned in Unicode 3.2 (a lock emoji): PostgreSQL hashes the text as typed, trade mark sign and all.
    let emoji = "Secure\u{2122}\u{1f512}";
    assert!(hashes_as(emoji, emoji));
    assert!(!hashes_as(emoji, "SecureTM\u{1f512}"));
    // Right-to-left mixed with left-to-right (the no-break space makes the text change, so the checks run): as typed.
    let mixed = "abc\u{a0}\u{5e9}\u{5dc}\u{5d5}\u{5dd}";
    assert!(hashes_as(mixed, mixed));
    assert!(!hashes_as(mixed, "abc \u{5e9}\u{5dc}\u{5d5}\u{5dd}"));
    // Left-to-right letters between right-to-left ones: the boundaries pass, the rule does not.
    let inside = "\u{5e9}\u{a0}abc\u{a0}\u{5e9}";
    assert!(hashes_as(inside, inside));
    assert!(!hashes_as(inside, "\u{5e9} abc \u{5e9}"));
    // A right-to-left string that does not end in a right-to-left character: as typed.
    let ends_wrong = "\u{5e9}\u{5dc}\u{5d5}\u{5dd}\u{a0}";
    assert!(hashes_as(ends_wrong, ends_wrong));
    assert!(!hashes_as(ends_wrong, "\u{5e9}\u{5dc}\u{5d5}\u{5dd} "));
    // A prohibited character after the mapping (a bell): as typed.
    let bell = "a\u{7}\u{a0}b";
    assert!(hashes_as(bell, bell));
    assert!(!hashes_as(bell, "a\u{7} b"));
}

#[test]
fn postgres_names_are_quoted_whatever_they_hold() {
    for name in ["we\"ird", "back\\slash", "100%", "with space", "ünï©ode ✓", "semi;colon--", "'quoted'"] {
        let quoted = format!("\"{}\"", name.replace('"', "\"\""));
        assert_eq!(pg(&create_role(name, PasswordChange::Keep)), [format!("CREATE ROLE {quoted} WITH LOGIN")], "{name}");
        let drop = AccountChange::Drop(DropAccount { id: AccountId::role(name), kind: AccountKind::Role, own: false });
        assert_eq!(pg(&drop), [format!("DROP ROLE {quoted}")], "{name}");
    }
    let member = Membership { role: AccountId::role("a\"b"), member: AccountId::role("c d"), admin_option: true, own: false };
    assert_eq!(pg(&AccountChange::GrantRole(member.clone())), ["GRANT \"a\"\"b\" TO \"c d\" WITH ADMIN OPTION"]);
    assert_eq!(pg(&AccountChange::RevokeRole(member)), ["REVOKE \"a\"\"b\" FROM \"c d\""]);
}

#[test]
fn a_postgres_alter_names_only_what_changed() {
    let from = pg_attrs();
    let alter = |to: PgAttributes, password: PasswordChange| {
        AccountChange::Alter(AlterAccount { id: AccountId::role("r"), from: Attributes::Postgres(from.clone()), to: Attributes::Postgres(to), password, own: false })
    };
    assert!(pg(&alter(from.clone(), PasswordChange::Keep)).is_empty(), "nothing to change, nothing to run");
    assert_eq!(pg(&alter(PgAttributes { login: false, create_db: true, ..from.clone() }, PasswordChange::Keep)), ["ALTER ROLE \"r\" WITH NOLOGIN CREATEDB"]);
    assert_eq!(pg(&alter(PgAttributes { connection_limit: 3, ..from.clone() }, PasswordChange::Keep)), ["ALTER ROLE \"r\" WITH CONNECTION LIMIT 3"]);
    assert_eq!(pg(&alter(PgAttributes { inherit: false, ..from.clone() }, PasswordChange::Keep)), ["ALTER ROLE \"r\" WITH NOINHERIT"]);
    assert_eq!(pg(&alter(from.clone(), set(NEEDLE))), ["ALTER ROLE \"r\" WITH PASSWORD '********'"]);
    assert_eq!(pg(&alter(from.clone(), PasswordChange::Clear)), ["ALTER ROLE \"r\" WITH PASSWORD NULL"]);
    let until = PgAttributes { valid_until: Some("2030-05-06 07:08:09+00".into()), ..from.clone() };
    assert_eq!(pg(&alter(until.clone(), PasswordChange::Keep)), ["ALTER ROLE \"r\" WITH VALID UNTIL '2030-05-06 07:08:09+00'"]);
    // Back to never expiring.
    let back = AccountChange::Alter(AlterAccount {
        id: AccountId::role("r"),
        from: Attributes::Postgres(until),
        to: Attributes::Postgres(from.clone()),
        password: PasswordChange::Keep,
        own: false,
    });
    assert_eq!(pg(&back), ["ALTER ROLE \"r\" WITH VALID UNTIL 'infinity'"]);
}

#[test]
fn postgres_privileges_are_granted_and_revoked_on_the_three_kinds_of_object() {
    let role = AccountId::role("app \"user\"");
    let quoted = "\"app \"\"user\"\"\"";
    assert_eq!(
        pg(&grant(role.clone(), GrantTarget::Database { name: "shop".into() }, &["CONNECT", "TEMPORARY"], false)),
        [format!("GRANT CONNECT, TEMPORARY ON DATABASE \"shop\" TO {quoted}")]
    );
    assert_eq!(pg(&grant(role.clone(), GrantTarget::Schema { name: "my schema".into() }, &["USAGE"], true)), [format!("GRANT USAGE ON SCHEMA \"my schema\" TO {quoted} WITH GRANT OPTION")]);
    assert_eq!(
        pg(&grant(role.clone(), table("public", "or\"ders"), &["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE", "REFERENCES", "TRIGGER"], false)),
        [format!("GRANT SELECT, INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON TABLE \"public\".\"or\"\"ders\" TO {quoted}")]
    );
    assert_eq!(pg(&revoke(role.clone(), table("public", "t"), &["SELECT"], false)), [format!("REVOKE SELECT ON TABLE \"public\".\"t\" FROM {quoted}")]);
    assert_eq!(
        pg(&revoke(role, table("public", "t"), &["SELECT", "INSERT"], true)),
        [format!("REVOKE GRANT OPTION FOR SELECT, INSERT ON TABLE \"public\".\"t\" FROM {quoted}")]
    );
}

#[test]
fn postgres_drops_reassigns_and_drops_what_a_role_owns() {
    let (old, new) = (AccountId::role("old"), AccountId::role("new"));
    assert_eq!(pg(&AccountChange::ReassignOwned { from: old.clone(), to: new, database: "shop".into() }), ["REASSIGN OWNED BY \"old\" TO \"new\""]);
    assert_eq!(pg(&AccountChange::DropOwned { role: old.clone(), database: "shop".into() }), ["DROP OWNED BY \"old\""]);
    assert_eq!(pg(&AccountChange::Drop(DropAccount { id: old, kind: AccountKind::Role, own: false })), ["DROP ROLE \"old\""]);
}

#[test]
fn postgres_refuses_what_it_cannot_say() {
    assert!(refused(Postgres, &create_role("", PasswordChange::Keep)).contains("empty"));
    assert!(refused(Postgres, &create_role("pg_monitor2", PasswordChange::Keep)).contains("reserved"));
    assert!(refused(Postgres, &create_role(&"x".repeat(64), PasswordChange::Keep)).contains("longer than the 63 bytes"));
    assert!(refused(Postgres, &create_role(" lead", PasswordChange::Keep)).contains("space"));
    assert!(refused(Postgres, &AccountChange::Create(CreateAccount { id: AccountId::account("u", "h"), kind: AccountKind::User, attributes: Attributes::Postgres(pg_attrs()), password: PasswordChange::Keep }))
        .contains("no host"));
    assert!(refused(Postgres, &create_role("r", set(""))).contains("empty"));
    assert!(refused(Postgres, &grant(AccountId::role("r"), GrantTarget::Global, &["SELECT"], false)).contains("no server-wide"));
    // A privilege the kind of object does not have; none at all.
    let AccountChange::Grant(mut bad) = grant(AccountId::role("r"), GrantTarget::Schema { name: "s".into() }, &["USAGE"], false) else { unreachable!() };
    bad.privileges = vec![PG_TABLE[0]];
    assert!(refused(Postgres, &AccountChange::Grant(bad.clone())).contains("not a privilege"));
    bad.privileges.clear();
    assert!(refused(Postgres, &AccountChange::Grant(bad)).contains("No privilege"));
    let limit = AccountChange::Create(CreateAccount {
        id: AccountId::role("r"),
        kind: AccountKind::Role,
        attributes: Attributes::Postgres(PgAttributes { connection_limit: -5, ..pg_attrs() }),
        password: PasswordChange::Keep,
    });
    assert!(refused(Postgres, &limit).contains("connection limit"));
    let until = AccountChange::Create(CreateAccount {
        id: AccountId::role("r"),
        kind: AccountKind::Role,
        attributes: Attributes::Postgres(PgAttributes { valid_until: Some("x\\' ; DROP".into()), ..pg_attrs() }),
        password: PasswordChange::Keep,
    });
    assert!(refused(Postgres, &until).contains("not a timestamp"));
    assert!(refused(Postgres, &AccountChange::Alter(AlterAccount {
        id: AccountId::role("r"),
        from: Attributes::Mysql(MyAttributes::default()),
        to: Attributes::Postgres(pg_attrs()),
        password: PasswordChange::Keep,
        own: false
    }))
    .contains("not the attributes"));
}

#[test]
fn a_valid_until_with_a_quote_is_a_literal_not_sql() {
    let until = AccountChange::Create(CreateAccount {
        id: AccountId::role("r"),
        kind: AccountKind::Role,
        attributes: Attributes::Postgres(PgAttributes { valid_until: Some("2030-01-01'; DROP ROLE x; --".into()), ..pg_attrs() }),
        password: PasswordChange::Keep,
    });
    assert_eq!(pg(&until), ["CREATE ROLE \"r\" WITH LOGIN VALID UNTIL '2030-01-01''; DROP ROLE x; --'"]);
}

// ---- MySQL and MariaDB --------------------------------------------------------------------------------------

#[test]
fn a_mysql_account_is_a_literal_per_the_servers_backslash_mode() {
    let drop = |name: &str, host: &str| AccountChange::Drop(DropAccount { id: AccountId::account(name, host), kind: AccountKind::User, own: false });
    for (name, host, default_mode, no_backslash) in [
        ("plain", "%", "'plain'@'%'", "'plain'@'%'"),
        ("o'brien", "10.0.%", "'o''brien'@'10.0.%'", "'o''brien'@'10.0.%'"),
        ("back\\slash", "h\\x", "'back\\\\slash'@'h\\\\x'", "'back\\slash'@'h\\x'"),
        ("100%", "local host", "'100%'@'local host'", "'100%'@'local host'"),
        ("ünï©ode ✓", "例え.jp", "'ünï©ode ✓'@'例え.jp'", "'ünï©ode ✓'@'例え.jp'"),
        ("`tick` \"dq\"", "h", "'`tick` \"dq\"'@'h'", "'`tick` \"dq\"'@'h'"),
        ("a\\'; DROP USER x; --", "h", "'a\\\\''; DROP USER x; --'@'h'", "'a\\''; DROP USER x; --'@'h'"),
    ] {
        for kind in [Mysql, Mariadb] {
            assert_eq!(shown(kind, "", &drop(name, host)), [format!("DROP USER {default_mode}")], "{kind:?} {name}");
            assert_eq!(shown(kind, "STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES", &drop(name, host)), [format!("DROP USER {no_backslash}")], "{kind:?} {name}");
        }
    }
}

#[test]
fn a_new_mysql_user_has_its_password_hidden_and_its_options_in_the_servers_order() {
    let create = AccountChange::Create(CreateAccount {
        id: AccountId::account("app", "%"),
        kind: AccountKind::User,
        attributes: Attributes::Mysql(MyAttributes { locked: true, expire_password: true, max_queries_per_hour: 100, max_updates_per_hour: 0, max_connections_per_hour: 20, max_user_connections: 3 }),
        password: set("p'w\\d"),
    });
    for kind in [Mysql, Mariadb] {
        assert_eq!(
            shown(kind, "", &create),
            ["CREATE USER 'app'@'%' IDENTIFIED BY '********' WITH MAX_QUERIES_PER_HOUR 100 MAX_CONNECTIONS_PER_HOUR 20 MAX_USER_CONNECTIONS 3 ACCOUNT LOCK PASSWORD EXPIRE"]
        );
        // What runs has the password, quoted for the mode.
        assert_eq!(
            run(kind, "", &create),
            ["CREATE USER 'app'@'%' IDENTIFIED BY 'p''w\\\\d' WITH MAX_QUERIES_PER_HOUR 100 MAX_CONNECTIONS_PER_HOUR 20 MAX_USER_CONNECTIONS 3 ACCOUNT LOCK PASSWORD EXPIRE"]
        );
        assert_eq!(
            run(kind, "NO_BACKSLASH_ESCAPES", &create),
            ["CREATE USER 'app'@'%' IDENTIFIED BY 'p''w\\d' WITH MAX_QUERIES_PER_HOUR 100 MAX_CONNECTIONS_PER_HOUR 20 MAX_USER_CONNECTIONS 3 ACCOUNT LOCK PASSWORD EXPIRE"]
        );
    }
    assert_eq!(my(&create_user("nopass", "localhost", PasswordChange::Keep)), ["CREATE USER 'nopass'@'localhost'"]);
}

#[test]
fn a_mysql_role_is_created_and_dropped_the_way_each_server_names_it() {
    let role = |id: AccountId| AccountChange::Create(CreateAccount { id, kind: AccountKind::Role, attributes: Attributes::Mysql(MyAttributes::default()), password: PasswordChange::Keep });
    assert_eq!(shown(Mysql, "", &role(AccountId::account("analyst", "%"))), ["CREATE ROLE 'analyst'@'%'"]);
    assert_eq!(shown(Mariadb, "", &role(AccountId::role("analyst"))), ["CREATE ROLE 'analyst'"]);
    let drop = |id: AccountId| AccountChange::Drop(DropAccount { id, kind: AccountKind::Role, own: false });
    assert_eq!(shown(Mysql, "", &drop(AccountId::account("analyst", "%"))), ["DROP ROLE 'analyst'@'%'"]);
    assert_eq!(shown(Mariadb, "", &drop(AccountId::role("analyst"))), ["DROP ROLE 'analyst'"]);
    // A MySQL account written without a host is any host; MariaDB users need one.
    assert_eq!(shown(Mysql, "", &AccountChange::Drop(DropAccount { id: AccountId::role("x"), kind: AccountKind::User, own: false })), ["DROP USER 'x'@'%'"]);
    let hostless = AccountChange::Create(CreateAccount { id: AccountId::role("x"), kind: AccountKind::User, attributes: Attributes::Mysql(MyAttributes::default()), password: PasswordChange::Keep });
    assert!(refused(Mariadb, &hostless).contains("needs a host"));
}

#[test]
fn a_mysql_alter_names_only_what_changed() {
    let from = MyAttributes { max_queries_per_hour: 10, ..MyAttributes::default() };
    let alter = |to: MyAttributes, password: PasswordChange| {
        AccountChange::Alter(AlterAccount { id: AccountId::account("u", "h"), from: Attributes::Mysql(from.clone()), to: Attributes::Mysql(to), password, own: false })
    };
    assert!(my(&alter(from.clone(), PasswordChange::Keep)).is_empty());
    assert_eq!(my(&alter(MyAttributes { locked: true, ..from.clone() }, PasswordChange::Keep)), ["ALTER USER 'u'@'h' ACCOUNT LOCK"]);
    assert_eq!(
        my(&alter(MyAttributes { max_queries_per_hour: 0, max_user_connections: 4, ..from.clone() }, set("x"))),
        ["ALTER USER 'u'@'h' IDENTIFIED BY '********' WITH MAX_QUERIES_PER_HOUR 0 MAX_USER_CONNECTIONS 4"]
    );
    assert_eq!(my(&alter(MyAttributes { expire_password: true, ..from.clone() }, PasswordChange::Keep)), ["ALTER USER 'u'@'h' PASSWORD EXPIRE"]);
    // Unlocking.
    let unlock = AccountChange::Alter(AlterAccount {
        id: AccountId::account("u", "h"),
        from: Attributes::Mysql(MyAttributes { locked: true, ..from.clone() }),
        to: Attributes::Mysql(from),
        password: PasswordChange::Keep,
        own: false,
    });
    assert_eq!(my(&unlock), ["ALTER USER 'u'@'h' ACCOUNT UNLOCK"]);
}

#[test]
fn mysql_privileges_are_granted_at_the_three_levels_with_the_grant_option_as_a_clause() {
    let user = AccountId::account("app", "10.%");
    assert_eq!(my(&grant(user.clone(), GrantTarget::Global, &["SELECT", "PROCESS"], false)), ["GRANT SELECT, PROCESS ON *.* TO 'app'@'10.%'"]);
    assert_eq!(
        my(&grant(user.clone(), GrantTarget::Database { name: escape_pattern("my_db") }, &["SELECT", "GRANT OPTION"], false)),
        ["GRANT SELECT ON `my\\_db`.* TO 'app'@'10.%' WITH GRANT OPTION"]
    );
    assert_eq!(my(&grant(user.clone(), table("sh`op", "or`ders"), &["INSERT"], false)), ["GRANT INSERT ON `sh``op`.`or``ders` TO 'app'@'10.%'"]);
    // Only the grant option: USAGE carries it.
    assert_eq!(my(&grant(user.clone(), GrantTarget::Database { name: "d".into() }, &["GRANT OPTION"], false)), ["GRANT USAGE ON `d`.* TO 'app'@'10.%' WITH GRANT OPTION"]);
    assert_eq!(my(&revoke(user.clone(), table("shop", "t"), &["SELECT", "GRANT OPTION"], false)), ["REVOKE SELECT, GRANT OPTION ON `shop`.`t` FROM 'app'@'10.%'"]);
    assert_eq!(my(&revoke(user, GrantTarget::Global, &["FILE"], false)), ["REVOKE FILE ON *.* FROM 'app'@'10.%'"]);
}

#[test]
fn mysql_roles_are_granted_to_accounts() {
    let membership = Membership { role: AccountId::account("analyst", "%"), member: AccountId::account("ann", "h"), admin_option: true, own: false };
    assert_eq!(my(&AccountChange::GrantRole(membership.clone())), ["GRANT 'analyst'@'%' TO 'ann'@'h' WITH ADMIN OPTION"]);
    assert_eq!(my(&AccountChange::RevokeRole(membership)), ["REVOKE 'analyst'@'%' FROM 'ann'@'h'"]);
    let maria = Membership { role: AccountId::role("analyst"), member: AccountId::account("ann", "h"), admin_option: false, own: false };
    assert_eq!(shown(Mariadb, "", &AccountChange::GrantRole(maria)), ["GRANT 'analyst' TO 'ann'@'h'"]);
}

#[test]
fn mysql_refuses_what_it_cannot_say() {
    let user = AccountId::account("u", "h");
    assert!(refused(Mysql, &create_user("", "h", PasswordChange::Keep)).contains("empty"));
    assert!(refused(Mysql, &create_user("u", "", PasswordChange::Keep)).contains("host is empty"));
    assert!(refused(Mysql, &create_user("u\n", "h", PasswordChange::Keep)).contains("control"));
    assert!(refused(Mysql, &create_user(&"u".repeat(33), "h", PasswordChange::Keep)).contains("longer than the 32"));
    assert!(dialect_for(Mysql).unwrap().account_statements(&create_user(&"u".repeat(32), "h", PasswordChange::Keep), Reveal::Redacted).is_ok());
    assert!(dialect_for(Mariadb).unwrap().account_statements(&create_user(&"u".repeat(40), "h", PasswordChange::Keep), Reveal::Redacted).is_ok(), "MariaDB says its own limit");
    assert!(refused(Mysql, &create_user("u", "h", set(""))).contains("password is empty"));
    assert!(refused(Mysql, &create_user("u", "h", PasswordChange::Clear)).contains("cannot have the password removed"));
    assert!(refused(Mysql, &AccountChange::ReassignOwned { from: user.clone(), to: user.clone(), database: String::new() }).contains("no objects owned"));
    assert!(refused(Mysql, &AccountChange::DropOwned { role: user.clone(), database: String::new() }).contains("no objects owned"));
    assert!(refused(Mysql, &grant(user.clone(), GrantTarget::Schema { name: "s".into() }, &["SELECT"], false)).contains("not schemas"));
    let AccountChange::Grant(mut bad) = grant(user.clone(), table("d", "t"), &["SELECT"], false) else { unreachable!() };
    bad.privileges = vec![Privilege::find(privilege::MYSQL_GLOBAL, "SHUTDOWN").unwrap()];
    assert!(refused(Mysql, &AccountChange::Grant(bad)).contains("not a privilege"));
    assert!(refused(Mysql, &revoke(user, table("d", "t"), &["SELECT"], true)).contains("GRANT OPTION as a privilege"));
    assert!(MYSQL_TABLE.contains(&Privilege::GRANT_OPTION));
}

#[test]
fn sqlite_and_clickhouse_have_no_accounts() {
    for kind in [ConnectionKind::Sqlite, ConnectionKind::Clickhouse] {
        let message = refused(kind, &create_role("r", PasswordChange::Keep));
        assert!(message.contains("no users and privileges"), "{kind:?}: {message}");
        assert!(AccountEngine::of(kind).is_none());
    }
}

// ---- passwords never show -----------------------------------------------------------------------------------

#[test]
fn nothing_that_shows_carries_the_password() {
    for (kind, change) in [(Postgres, create_role("r", set(NEEDLE))), (Mysql, create_user("u", "h", set(NEEDLE))), (Mariadb, create_user("u", "h", set(NEEDLE)))] {
        let dialect = dialect_in_mode(kind, "").unwrap();
        let changes = vec![Change::Account(change.clone())];
        let planned = plan(&*dialect, &changes);
        let mut seen = vec![format!("{change:?}"), format!("{changes:?}"), format!("{planned:?}"), planned.script(), change.title()];
        seen.extend(warnings(&*dialect, &changes).into_iter().map(|warning| format!("{warning:?}")));
        seen.extend(planned.steps().into_iter().map(|step| format!("{step:?}")));
        for text in seen {
            assert!(!text.contains(NEEDLE), "{kind:?}: {text}");
        }
        // Serialized (a saved draft) it is not there either.
        let json = serde_json::to_string(&changes).unwrap();
        assert!(!json.contains(NEEDLE), "{json}");
        assert!(planned.script().contains(REDACTED_LITERAL));
    }
}

#[test]
fn a_wiped_password_cannot_be_planned_so_a_retry_never_creates_an_account_without_one() {
    let password = Password::new(NEEDLE);
    let clone = password.clone();
    let change = create_role("r", PasswordChange::Set(password.clone()));
    assert!(dialect_for(Postgres).unwrap().account_statements(&change, Reveal::Redacted).is_ok());
    clone.wipe();
    assert!(!password.is_present(), "every clone shares the cell");
    for reveal in [Reveal::Redacted, Reveal::Exec] {
        let error = dialect_for(Postgres).unwrap().account_statements(&change, reveal).expect_err("wiped");
        assert!(error.to_string().contains("type it again"), "{error}");
    }
    assert_eq!(format!("{password:?}"), "Password(<wiped>)");
    let mysql = create_user("u", "h", PasswordChange::Set(password));
    assert!(dialect_for(Mysql).unwrap().account_statements(&mysql, Reveal::Exec).is_err());
}

#[test]
fn the_executable_plan_replaces_only_the_password_statements_and_scrubs_the_servers_echo() {
    let dialect = dialect_in_mode(Mysql, "").unwrap();
    let changes = vec![
        Change::Account(create_user("app", "%", set(NEEDLE))),
        Change::Account(grant(AccountId::account("app", "%"), GrantTarget::Global, &["SELECT"], false)),
    ];
    let planned = plan(&*dialect, &changes);
    let exec = executable(&*dialect, &changes, &planned).expect("plans");
    let (shown, run) = (planned.steps(), exec.planned().steps());
    assert_eq!(shown.len(), run.len());
    assert_eq!(run[0].sql, format!("CREATE USER 'app'@'%' IDENTIFIED BY '{NEEDLE}'"));
    assert_eq!(shown[0].sql, "CREATE USER 'app'@'%' IDENTIFIED BY '********'");
    assert_eq!(shown[1], run[1], "a statement without a password is the same text");
    // A syntax error repeats the text around it.
    let echoed = format!("You have an error in your SQL syntax near ''{NEEDLE}' WITH' at line 1");
    let cleaned = exec.scrub(&echoed);
    assert!(!cleaned.contains(NEEDLE), "{cleaned}");
    assert!(cleaned.contains("near"), "{cleaned}");
    // The same for the PostgreSQL verifier.
    let pg_dialect = dialect_in_mode(Postgres, "").unwrap();
    let pg_changes = vec![Change::Account(create_role("r", set(NEEDLE)))];
    let pg_planned = plan(&*pg_dialect, &pg_changes);
    let pg_exec = executable(&*pg_dialect, &pg_changes, &pg_planned).unwrap();
    let verifier = pg_exec.planned().steps()[0].sql.split("PASSWORD ").nth(1).unwrap().to_owned();
    let cleaned = pg_exec.scrub(&format!("syntax error at or near {verifier}"));
    assert!(!cleaned.contains("SCRAM-SHA-256"), "{cleaned}");
}

#[test]
fn scrubbing_hides_the_escaped_form_too_and_the_longest_secret_first() {
    let mut scrubber = Scrubber::default();
    scrubber.hide_difference("ALTER USER 'u' IDENTIFIED BY '********' ACCOUNT LOCK", "ALTER USER 'u' IDENTIFIED BY 'it''s \\\\ ok' ACCOUNT LOCK");
    scrubber.hide("it's \\ ok");
    let message = "near 'it''s \\\\ ok' ACCOUNT and it's \\ ok again";
    assert_eq!(scrubber.scrub(message), "near ******** ACCOUNT and ******** again");
    // A password that starts like the placeholder is still found whole.
    let mut starred = Scrubber::default();
    starred.hide_difference("PASSWORD '********' X", "PASSWORD '**abc' X");
    assert_eq!(starred.scrub("see '**abc' there"), "see ******** there");
}

#[test]
fn only_whole_secrets_are_hidden_so_a_message_that_names_the_account_stays_readable() {
    let mut scrubber = Scrubber::default();
    scrubber.hide_difference("CREATE USER 'alice'@'%' IDENTIFIED BY '********'", "CREATE USER 'alice'@'%' IDENTIFIED BY 'alice2024'");
    scrubber.hide("alice2024");
    // The user is called alice and the password starts like it: the account name is not a secret.
    let message = "Operation CREATE USER failed for 'alice'@'%'";
    assert_eq!(scrubber.scrub(message), message);
    // The whole password, quoted or not, is hidden.
    assert_eq!(scrubber.scrub("bad value 'alice2024' for alice2024"), "bad value ******** for ********");
    // A start of it is not.
    assert_eq!(scrubber.scrub("alice20 and alice"), "alice20 and alice");
    assert!(format!("{scrubber:?}").contains("secrets") && !format!("{scrubber:?}").contains("alice2024"), "{scrubber:?}");
}

#[test]
fn a_servers_echo_of_the_statement_is_not_shown() {
    let echoed = "You have an error in your SQL syntax; check the manual near ''Zq9!kLm' at line 1 (error 1064, SQLSTATE 42000)";
    let shown = Scrubber::without_echo(echoed);
    assert_eq!(shown, "You have an error in your SQL syntax; check the manual near <the statement is not shown> (error 1064, SQLSTATE 42000)");
    assert_eq!(Scrubber::without_echo("syntax error at or near \"PASSWORD 'x'\""), "syntax error at or near <the statement is not shown>");
    assert_eq!(Scrubber::without_echo("connection reset"), "connection reset");
}

#[test]
fn only_the_failure_of_a_statement_that_carries_a_password_loses_its_echo() {
    let dialect = dialect_in_mode(Mysql, "").unwrap();
    let changes = vec![Change::Account(create_user("app", "%", set(NEEDLE))), Change::Account(grant(AccountId::account("app", "%"), GrantTarget::Global, &["SELECT"], false))];
    let planned = plan(&*dialect, &changes);
    let exec = executable(&*dialect, &changes, &planned).expect("plans");
    let message = "You have an error near 'ACCOUNT LOCK' at line 1 (error 1064, SQLSTATE 42000)";
    assert_eq!(planned.change_of_step(0), Some(0));
    assert_eq!(planned.change_of_step(1), Some(1));
    assert_eq!(exec.clean_failure(Some(0), message), "You have an error near <the statement is not shown> (error 1064, SQLSTATE 42000)");
    assert_eq!(exec.clean_failure(Some(1), message), message, "a statement without a password may be quoted");
    assert_eq!(exec.clean_failure(None, message), message);
}

// ---- change-sets --------------------------------------------------------------------------------------------

#[test]
fn a_change_set_runs_creations_then_alterations_then_the_drops_in_order() {
    let dialect = dialect_in_mode(Postgres, "").unwrap();
    let old = AccountId::role("old");
    let changes = vec![
        Change::Account(AccountChange::Drop(DropAccount { id: old.clone(), kind: AccountKind::Role, own: false })),
        Change::Account(AccountChange::DropOwned { role: old.clone(), database: "d".into() }),
        Change::Account(grant(AccountId::role("new"), table("public", "t"), &["SELECT"], false)),
        Change::Account(AccountChange::ReassignOwned { from: old, to: AccountId::role("new"), database: "d".into() }),
        Change::Account(create_role("new", PasswordChange::Keep)),
    ];
    let order: Vec<String> = plan(&*dialect, &changes).steps().into_iter().map(|step| step.sql).collect();
    assert_eq!(
        order,
        [
            "CREATE ROLE \"new\" WITH LOGIN",
            "GRANT SELECT ON TABLE \"public\".\"t\" TO \"new\"",
            "REASSIGN OWNED BY \"old\" TO \"new\"",
            "DROP OWNED BY \"old\"",
            "DROP ROLE \"old\""
        ]
    );
    assert!(plan(&*dialect, &changes).transactional, "PostgreSQL runs role changes in one transaction");
    assert!(!plan(&*dialect_in_mode(Mysql, "").unwrap(), &changes[..1]).transactional);
}

#[test]
fn the_warnings_say_what_dropping_and_the_login_account_cost() {
    let dialect = dialect_in_mode(Postgres, "").unwrap();
    let id = AccountId::role("old");
    let drop = Change::Account(AccountChange::Drop(DropAccount { id: id.clone(), kind: AccountKind::Role, own: true }));
    let found = warnings(&*dialect, &[drop]);
    assert!(found.iter().all(|warning| warning.severity == Severity::Destructive));
    assert!(found.iter().any(|warning| warning.message.contains("owns objects")), "{found:?}");
    assert!(found.iter().any(|warning| warning.message.contains("logs in as")), "{found:?}");
    let owned = warnings(&*dialect, &[Change::Account(AccountChange::DropOwned { role: id.clone(), database: "shop".into() })]);
    assert!(owned[0].message.contains("shop") && owned[0].message.contains("tables and their rows"), "{owned:?}");
    let mysql = dialect_in_mode(Mysql, "").unwrap();
    let dropped = warnings(&*mysql, &[Change::Account(AccountChange::Drop(DropAccount { id: AccountId::account("u", "h"), kind: AccountKind::User, own: false }))]);
    assert!(dropped[0].message.contains("Sessions it has open stay connected"), "{dropped:?}");
    let created = warnings(&*mysql, &[Change::Account(create_user("u", "h", set("x")))]);
    assert!(created.iter().any(|warning| warning.message.contains("TLS")), "{created:?}");
    let none = warnings(&*mysql, &[Change::Account(create_user("u", "h", PasswordChange::Keep))]);
    assert!(none.iter().any(|warning| warning.message.contains("without a password")), "{none:?}");
    let pg = warnings(&*dialect, &[Change::Account(create_role("r", set("x")))]);
    assert!(pg.is_empty(), "the PostgreSQL password never leaves the app: nothing to warn about, {pg:?}");
    let revoke_own = Change::Account(AccountChange::Revoke(Revoke {
        grantee: id.clone(),
        target: table("public", "t"),
        privileges: vec![PG_TABLE[0]],
        grant_option_only: false,
        own: true,
    }));
    assert!(warnings(&*dialect, &[revoke_own]).iter().any(|warning| warning.message.contains("logs in as")));
}

#[test]
fn a_change_serializes_without_its_password_and_comes_back_without_one() {
    let change = Change::Account(create_user("u", "h", set(NEEDLE)));
    let json = serde_json::to_string(&change).unwrap();
    assert!(json.contains("\"op\":\"account\"") && !json.contains(NEEDLE), "{json}");
    let back: Change = serde_json::from_str(&json).unwrap();
    let Change::Account(AccountChange::Create(create)) = back else { panic!("{back:?}") };
    assert_eq!(create.password, PasswordChange::Keep);
}

// ---- privileges ----------------------------------------------------------------------------------------------

fn held(entries: &[(&str, bool)]) -> PrivSet {
    entries.iter().map(|(name, option)| (Privilege::find(PG_TABLE, name).unwrap(), *option)).collect()
}

#[test]
fn the_difference_of_two_privilege_sets_is_a_grant_and_a_revoke() {
    let delta = diff(PG_TABLE, &held(&[("SELECT", false), ("DELETE", true), ("UPDATE", false)]), &held(&[("SELECT", true), ("INSERT", false), ("UPDATE", false), ("TRIGGER", true)]));
    let names = |list: &[Privilege]| list.iter().map(|privilege| privilege.name()).collect::<Vec<_>>();
    assert_eq!(names(&delta.grant), ["INSERT"]);
    assert_eq!(names(&delta.grant_with_option), ["SELECT", "TRIGGER"]);
    assert_eq!(names(&delta.revoke), ["DELETE"]);
    assert!(delta.revoke_option.is_empty());
    let lost = diff(PG_TABLE, &held(&[("SELECT", true)]), &held(&[("SELECT", false)]));
    assert_eq!(names(&lost.revoke_option), ["SELECT"]);
    assert!(diff(PG_TABLE, &held(&[("SELECT", true)]), &held(&[("SELECT", true)])).is_empty());
    // A privilege the editor does not offer is never touched.
    let other = Privilege::find(privilege::MYSQL_GLOBAL, "SHUTDOWN").unwrap();
    let mut with_other = held(&[]);
    with_other.insert(other, false);
    assert!(diff(PG_TABLE, &with_other, &held(&[])).is_empty());
}

#[test]
fn a_database_pattern_escapes_what_would_match_other_names() {
    for name in ["plain", "my_db", "100%", "back\\slash", "a_b%c\\d", "ünï_©"] {
        assert_eq!(unescape_pattern(&escape_pattern(name)), name);
    }
    assert_eq!(escape_pattern("my_db"), "my\\_db");
    assert_eq!(unescape_pattern("cf\\_%"), "cf_%", "an unescaped % stays a wildcard in the label");
    assert_eq!(GrantTarget::Database { name: "my\\_db".into() }.label(true), "my_db");
    assert_eq!(table("my\\_db", "t").label(true), "my_db.t");
}

// ---- the catalog --------------------------------------------------------------------------------------------

/// Answers the queries whose text contains the key.
struct Script(Vec<(&'static str, Result<TextRows, ReadError>)>);

impl CatalogSession for Script {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        self.0.iter().find(|(key, _)| sql.contains(key)).map(|(_, answer)| answer.clone()).unwrap_or_else(|| Err(ReadError::Failed(format!("unexpected query: {sql}"))))
    }
}

fn row(cells: &[&str]) -> Vec<Option<String>> {
    cells.iter().map(|cell| (*cell != "NULL").then(|| (*cell).to_owned())).collect()
}

#[test]
fn postgres_roles_memberships_and_the_login_are_read() {
    let mut script = Script(vec![
        (
            "FROM pg_catalog.pg_roles ORDER BY rolname",
            Ok(vec![
                row(&["app", "true", "false", "false", "false", "true", "false", "false", "-1", "NULL"]),
                row(&["boss", "t", "t", "t", "t", "f", "t", "t", "5", "2030-01-01 00:00:00+00"]),
                row(&["limited", "f", "f", "f", "f", "t", "f", "f", "-1", "infinity"]),
            ]),
        ),
        ("pg_auth_members", Ok(vec![row(&["boss", "app", "true", "tablex", "true", "true", "false", "true"])])),
        ("SELECT datname FROM", Ok(vec![row(&["other"]), row(&["shop"])])),
        ("session_user", Ok(vec![row(&["app", "shop", "PostgreSQL 17.10", "170010"])])),
    ]);
    let catalog = load_accounts(&mut script, AccountEngine::Postgres).expect("reads");
    assert_eq!(catalog.accounts.len(), 3);
    assert_eq!(catalog.login, Some(AccountId::role("app")));
    assert_eq!((catalog.database.as_str(), catalog.databases.as_slice()), ("shop", ["other".to_owned(), "shop".to_owned()].as_slice()));
    let Attributes::Postgres(boss) = &catalog.account(&AccountId::role("boss")).unwrap().attributes else { panic!() };
    assert!(boss.superuser && boss.create_db && boss.create_role && !boss.inherit && boss.replication && boss.bypass_rls);
    assert_eq!((boss.connection_limit, boss.valid_until.as_deref()), (5, Some("2030-01-01 00:00:00+00")));
    let Attributes::Postgres(limited) = &catalog.account(&AccountId::role("limited")).unwrap().attributes else { panic!() };
    assert_eq!(limited.valid_until, None, "infinity is never");
    assert_eq!(catalog.roles_of(&AccountId::role("app")).map(|m| (m.role.name.as_str(), m.admin_option)).collect::<Vec<_>>(), [("boss", true)]);
    assert_eq!(catalog.members_of(&AccountId::role("boss")).count(), 1);
}

#[test]
fn a_membership_is_revocable_by_the_rule_of_the_servers_version() {
    // The facts of a membership granted by `stranger`, as read by the current role (not the grantor):
    // [role, member, admin option, grantor, is superuser, granted by the current role, granted by the bootstrap superuser, has the admin option]
    let read = |version: &str, facts: [&str; 4]| {
        let mut script = Script(vec![
            ("FROM pg_catalog.pg_roles ORDER BY rolname", Ok(Vec::new())),
            ("pg_auth_members", Ok(vec![row(&["boss", "app", "false", "stranger", facts[0], facts[1], facts[2], facts[3]])])),
            ("SELECT datname FROM", Ok(Vec::new())),
            ("session_user", Ok(vec![row(&["me", "shop", "PostgreSQL", version])])),
        ]);
        load_accounts(&mut script, AccountEngine::Postgres).expect("reads").memberships[0].revocable
    };
    // 16 and later: only the current role's own grants; a superuser's also the bootstrap superuser's.
    assert!(!read("160000", ["false", "false", "false", "true"]), "an admin option does not reach another role's grant");
    assert!(read("170010", ["false", "true", "false", "false"]));
    assert!(!read("170010", ["true", "false", "false", "false"]), "a superuser leaves what another role granted");
    assert!(read("170010", ["true", "false", "true", "false"]), "and takes back what the bootstrap superuser granted");
    // Before 16 the grantor plays no part: a superuser, or a role with the admin option on the group.
    assert!(read("150000", ["true", "false", "false", "false"]));
    assert!(read("140005", ["false", "false", "false", "true"]), "the admin option is enough, whoever granted it");
    assert!(!read("150000", ["false", "true", "false", "false"]), "no admin option, no superuser: nothing to revoke with");
}

#[test]
fn a_catalog_the_account_may_not_read_is_denied_with_the_servers_words() {
    let mut script = Script(vec![("FROM mysql.user", Err(ReadError::from_server("SELECT command denied to user 'tablex'@'172.17.0.1' for table 'user'".into()))), ("CURRENT_USER", Ok(vec![row(&["tablex@%", "STRICT_TRANS_TABLES", "8.4.11"])]))]);
    let error = load_accounts(&mut script, AccountEngine::Mysql).expect_err("denied");
    assert!(matches!(&error, ReadError::Denied(message) if message.contains("command denied")), "{error:?}");
    assert!(matches!(ReadError::from_server("permission denied for table x".into()), ReadError::Denied(_)));
    assert!(matches!(ReadError::from_server("connection reset".into()), ReadError::Failed(_)));
}

#[test]
fn mysql_users_roles_limits_and_the_mode_are_read() {
    let mut script = Script(vec![
        ("CURRENT_USER", Ok(vec![row(&["app@10.0.0.%", "STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES", "8.4.11"])])),
        (
            "FROM mysql.user",
            Ok(vec![
                row(&["app", "10.0.0.%", "N", "N", "caching_sha2_password", "100", "0", "20", "3", "0"]),
                row(&["analyst", "%", "Y", "Y", "caching_sha2_password", "0", "0", "0", "0", "1"]),
                row(&["old", "localhost", "Y", "N", "mysql_native_password", "0", "0", "0", "0", "0"]),
            ]),
        ),
        ("role_edges", Ok(vec![row(&["analyst", "%", "app", "10.0.0.%", "Y"])])),
    ]);
    let catalog = load_accounts(&mut script, AccountEngine::Mysql).expect("reads");
    assert_eq!(catalog.sql_mode.as_deref(), Some("STRICT_TRANS_TABLES,NO_BACKSLASH_ESCAPES"));
    assert_eq!(catalog.login, Some(AccountId::account("app", "10.0.0.%")));
    let app = catalog.account(&AccountId::account("app", "10.0.0.%")).unwrap();
    assert_eq!((app.kind, app.plugin.as_deref(), app.password_expired), (AccountKind::User, Some("caching_sha2_password"), false));
    let Attributes::Mysql(app_attrs) = &app.attributes else { panic!() };
    assert_eq!((app_attrs.max_queries_per_hour, app_attrs.max_connections_per_hour, app_attrs.max_user_connections, app_attrs.locked), (100, 20, 3, false));
    let analyst = catalog.account(&AccountId::account("analyst", "%")).unwrap();
    assert_eq!(analyst.kind, AccountKind::Role);
    let old = catalog.account(&AccountId::account("old", "localhost")).unwrap();
    assert!(matches!(&old.attributes, Attributes::Mysql(attrs) if attrs.locked));
    assert_eq!(catalog.roles_of(&app.id).map(|m| (m.role.to_string(), m.admin_option)).collect::<Vec<_>>(), [("analyst@%".to_owned(), true)]);
}

#[test]
fn mariadb_roles_have_no_host_and_the_lock_comes_from_the_privilege_document() {
    let mut script = Script(vec![
        ("CURRENT_USER", Ok(vec![row(&["tablex@%", "", "11.8.9-MariaDB"])])),
        ("mysql.global_priv", Ok(vec![row(&["locked", "%"])])),
        (
            "FROM mysql.user",
            Ok(vec![
                row(&["locked", "%", "", "N", "mysql_native_password", "0", "0", "0", "0", "N"]),
                row(&["analyst", "", "", "N", "", "0", "0", "0", "0", "Y"]),
                row(&["tablex", "%", "", "N", "mysql_native_password", "0", "0", "0", "0", "N"]),
            ]),
        ),
        ("roles_mapping", Ok(vec![row(&["analyst", "tablex", "%", "N"])])),
    ]);
    let catalog = load_accounts(&mut script, AccountEngine::Mariadb).expect("reads");
    let ids: Vec<String> = catalog.accounts.iter().map(|row| row.id.to_string()).collect();
    assert_eq!(ids, ["locked@%", "analyst", "tablex@%"]);
    assert!(matches!(&catalog.accounts[0].attributes, Attributes::Mysql(attrs) if attrs.locked));
    assert!(matches!(&catalog.accounts[2].attributes, Attributes::Mysql(attrs) if !attrs.locked));
    assert_eq!(catalog.accounts[1].kind, AccountKind::Role);
    assert_eq!(catalog.roles_of(&AccountId::account("tablex", "%")).map(|m| m.role.to_string()).collect::<Vec<_>>(), ["analyst"]);
}

#[test]
fn postgres_privileges_are_read_per_object_with_the_ones_the_editor_does_not_offer_apart() {
    let dialect = dialect_in_mode(Postgres, "").unwrap();
    let mut script = Script(vec![
        (
            "aclexplode",
            Ok(vec![
                row(&["D", "", "shop", "CONNECT", "false", "tablex", "true"]),
                row(&["S", "", "public", "USAGE", "true", "tablex", "true"]),
                row(&["T", "public", "orders", "SELECT", "false", "tablex", "true"]),
                row(&["T", "public", "orders", "INSERT", "true", "tablex", "true"]),
                row(&["T", "public", "orders", "MAINTAIN", "false", "tablex", "true"]),
                row(&["T", "audit", "log", "SELECT", "false", "tablex", "true"]),
            ]),
        ),
        ("FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn", Ok(vec![row(&["D", "", "shop"]), row(&["D", "", "other"])])),
        ("FROM pg_catalog.pg_namespace WHERE", Ok(vec![row(&["S", "", "audit"]), row(&["S", "", "public"])])),
        ("FROM pg_catalog.pg_class c", Ok(vec![row(&["T", "public", "orders"]), row(&["T", "public", "items"])])),
    ]);
    let catalog = load_privileges(&mut script, &*dialect, AccountEngine::Postgres, &AccountId::role("app")).expect("reads");
    let orders = &catalog.held[&table("public", "orders")];
    assert_eq!(orders.iter().map(|(p, g)| (p.name(), *g)).collect::<Vec<_>>(), [("INSERT", true), ("SELECT", false)]);
    assert_eq!(catalog.other, [(table("public", "orders"), "MAINTAIN".to_owned())]);
    assert_eq!(catalog.held[&GrantTarget::Schema { name: "public".into() }].get(&Privilege::find(privilege::PG_SCHEMA, "USAGE").unwrap()), Some(&true));
    // The objects: databases, schemas, tables; a table that holds a privilege is listed though the table list lacks it.
    assert_eq!(catalog.objects.len(), 2 + 2 + 2 + 1);
    assert!(catalog.objects.contains(&table("audit", "log")));
    assert!(!catalog.tables_truncated);
}

#[test]
fn privileges_other_roles_granted_are_told_apart_and_the_merge_does_not_depend_on_the_order_of_rows() {
    let dialect = dialect_in_mode(Postgres, "").unwrap();
    let rows = [
        row(&["T", "public", "orders", "SELECT", "false", "tablex", "true"]),
        row(&["T", "public", "orders", "SELECT", "true", "reader", "true"]),
        row(&["T", "public", "orders", "UPDATE", "false", "stranger", "false"]),
        row(&["T", "public", "orders", "SELECT", "true", "stranger", "false"]),
    ];
    let load = |rows: Vec<Vec<Option<String>>>| {
        let mut script = Script(vec![
            ("aclexplode", Ok(rows)),
            ("FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn", Ok(Vec::new())),
            ("FROM pg_catalog.pg_namespace WHERE", Ok(Vec::new())),
            ("FROM pg_catalog.pg_class c", Ok(Vec::new())),
        ]);
        load_privileges(&mut script, &*dialect, AccountEngine::Postgres, &AccountId::role("app")).expect("reads")
    };
    let forward = load(rows.to_vec());
    let backward = load(rows.iter().rev().cloned().collect());
    for catalog in [&forward, &backward] {
        let select = Privilege::find(privilege::PG_TABLE, "SELECT").unwrap();
        // Two grants this role can take back: held with the option, whichever came last.
        assert_eq!(catalog.held[&table("public", "orders")].get(&select), Some(&true));
        // What a stranger granted is not held (it cannot be revoked here): it is shown, with whose it is.
        assert!(!catalog.held[&table("public", "orders")].contains_key(&Privilege::find(privilege::PG_TABLE, "UPDATE").unwrap()));
        assert_eq!(catalog.foreign.len(), 2);
        assert!(catalog.foreign.iter().all(|grant| grant.grantor == "stranger"));
    }
    assert_eq!(forward.held, backward.held);
    // The query asks the server who may take each back, as a superuser (the owner's grants) and as anyone else (the roles it acts as).
    let mut recorder = Recorded(Vec::new());
    load_privileges(&mut recorder, &*dialect, AccountEngine::Postgres, &AccountId::role("app")).unwrap();
    let sql = &recorder.0[0];
    assert!(sql.contains("rolsuper") && sql.contains("pg_has_role(current_user, a.grantor, 'USAGE')") && sql.contains("a.grantor = c.relowner"), "{sql}");
}

struct Recorded(Vec<String>);

impl CatalogSession for Recorded {
    fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
        self.0.push(sql.to_owned());
        Ok(Vec::new())
    }
}

#[test]
fn the_account_name_in_a_privilege_query_is_a_literal_for_the_servers_mode() {
    let dialect = dialect_in_mode(Mysql, "NO_BACKSLASH_ESCAPES").unwrap();
    struct Recorder(Vec<String>);
    impl CatalogSession for Recorder {
        fn rows(&mut self, sql: &str) -> Result<TextRows, ReadError> {
            self.0.push(sql.to_owned());
            Ok(Vec::new())
        }
    }
    let mut recorder = Recorder(Vec::new());
    load_privileges(&mut recorder, &*dialect, AccountEngine::Mysql, &AccountId::account("o'b\\x", "h%")).expect("reads");
    let grantee = "CONCAT('''', 'o''b\\x', '''@''', 'h%', '''')";
    assert!(recorder.0.iter().filter(|sql| sql.contains("GRANTEE")).all(|sql| sql.contains(grantee)), "{:?}", recorder.0);
    let default_mode = dialect_in_mode(Mysql, "").unwrap();
    let mut recorder = Recorder(Vec::new());
    load_privileges(&mut recorder, &*default_mode, AccountEngine::Mysql, &AccountId::account("o'b\\x", "h%")).expect("reads");
    assert!(recorder.0.iter().any(|sql| sql.contains("'o''b\\\\x'")), "{:?}", recorder.0);
    // PostgreSQL: the role name is a literal of its own dialect.
    let mut recorder = Recorder(Vec::new());
    load_privileges(&mut recorder, &*dialect_in_mode(Postgres, "").unwrap(), AccountEngine::Postgres, &AccountId::role("a'b\\c")).expect("reads");
    assert!(recorder.0[0].contains("rolname = E'a''b\\\\c'"), "a name with a backslash is an escape string: {}", recorder.0[0]);
    let mut recorder = Recorder(Vec::new());
    load_privileges(&mut recorder, &*dialect_in_mode(Postgres, "").unwrap(), AccountEngine::Postgres, &AccountId::role("plain's")).expect("reads");
    assert!(recorder.0[0].contains("rolname = 'plain''s'"), "{}", recorder.0[0]);
}

#[test]
fn mysql_privileges_become_sets_with_the_grant_option_as_a_member() {
    let dialect = dialect_in_mode(Mysql, "").unwrap();
    let mut script = Script(vec![
        ("information_schema.USER_PRIVILEGES", Ok(vec![row(&["USAGE", "NO"])])),
        ("information_schema.SCHEMA_PRIVILEGES", Ok(vec![row(&["shop", "SELECT", "YES"]), row(&["shop", "INSERT", "YES"]), row(&["app\\_%", "SELECT", "NO"])])),
        ("information_schema.TABLE_PRIVILEGES", Ok(vec![row(&["shop", "orders", "UPDATE", "NO"]), row(&["shop", "orders", "SET_USER_ID", "NO"])])),
        ("information_schema.COLUMN_PRIVILEGES", Ok(vec![row(&["shop", "orders", "note", "SELECT"])])),
        ("information_schema.SCHEMATA", Ok(vec![row(&["shop"]), row(&["sys"])])),
        ("information_schema.TABLES", Ok(vec![row(&["shop", "orders"])])),
    ]);
    let catalog = load_privileges(&mut script, &*dialect, AccountEngine::Mysql, &AccountId::account("app", "%")).expect("reads");
    assert!(!catalog.held.contains_key(&GrantTarget::Global), "USAGE is no privilege");
    let shop = &catalog.held[&GrantTarget::Database { name: "shop".into() }];
    assert_eq!(shop.keys().map(|p| p.name()).collect::<Vec<_>>(), ["GRANT OPTION", "INSERT", "SELECT"]);
    assert!(shop.values().all(|option| !option));
    assert_eq!(catalog.other, [(table("shop", "orders"), "SET_USER_ID".to_owned())]);
    assert_eq!(catalog.columns, ["shop.orders.note: SELECT"]);
    // The pattern the account holds is an object although no database has that name.
    assert!(catalog.objects.contains(&GrantTarget::Database { name: "app\\_%".into() }));
    assert_eq!(catalog.objects[0], GrantTarget::Global);
}

#[test]
fn a_grant_on_a_database_pattern_is_told_apart_from_the_database_with_the_same_name() {
    let dialect = dialect_in_mode(Mysql, "").unwrap();
    let mut script = Script(vec![
        ("information_schema.USER_PRIVILEGES", Ok(Vec::new())),
        // The account holds SELECT on the pattern `app_db` (the `_` matches any character), not on the database `app\_db`.
        ("information_schema.SCHEMA_PRIVILEGES", Ok(vec![row(&["app_db", "SELECT", "NO"])])),
        ("information_schema.TABLE_PRIVILEGES", Ok(Vec::new())),
        ("information_schema.COLUMN_PRIVILEGES", Ok(Vec::new())),
        ("information_schema.SCHEMATA", Ok(vec![row(&["app_db"])])),
        ("information_schema.TABLES", Ok(Vec::new())),
    ]);
    let catalog = load_privileges(&mut script, &*dialect, AccountEngine::Mysql, &AccountId::account("u", "%")).expect("reads");
    let labels: Vec<String> = catalog.objects.iter().map(|target| target.label(true)).collect();
    assert_eq!(labels, ["All databases (*.*)", "app_db", "app_db (pattern)"], "two rows that can be told apart");
    assert!(catalog.held.contains_key(&GrantTarget::Database { name: "app_db".into() }));
    assert!(!catalog.held.contains_key(&GrantTarget::Database { name: "app\\_db".into() }), "nothing is held on the database itself");
    assert!(privilege::has_wildcard("app_db") && privilege::has_wildcard("a%") && !privilege::has_wildcard("app\\_db") && !privilege::has_wildcard("a\\\\b"));
}

#[test]
fn a_long_table_list_is_cut_and_says_so() {
    let dialect = dialect_in_mode(Postgres, "").unwrap();
    let tables: TextRows = (0..=TABLE_LIMIT).map(|n| row(&["T", "public", &format!("t{n:05}")])).collect();
    let mut script = Script(vec![
        ("aclexplode", Ok(Vec::new())),
        ("FROM pg_catalog.pg_database WHERE NOT datistemplate AND datallowconn", Ok(Vec::new())),
        ("FROM pg_catalog.pg_namespace WHERE", Ok(Vec::new())),
        ("FROM pg_catalog.pg_class c", Ok(tables)),
    ]);
    let catalog = load_privileges(&mut script, &*dialect, AccountEngine::Postgres, &AccountId::role("app")).expect("reads");
    assert!(catalog.tables_truncated);
    assert_eq!(catalog.objects.len(), TABLE_LIMIT);
}
