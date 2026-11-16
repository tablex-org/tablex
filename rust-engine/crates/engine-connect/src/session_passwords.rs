//! The passwords typed for connections that do not save theirs: kept in memory for this run of the
//! app, never written anywhere, wiped when dropped ([`engine_net::Secret`]). The app hands the same
//! store to every [`crate::connect::Connector`] it makes, so a connect, a tab's own session or the
//! assistant's connection all read the one password. A front end that never sets one (`tablex-mcp`)
//! sees no change.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use engine_net::Secret;

use crate::records::{resolve_runtime_uri_from, RuntimeUri, SavedConnection};

/// Connection id to the password typed for it. Cheap to clone: clones share the passwords.
#[derive(Clone, Default)]
pub struct SessionPasswords(Arc<Mutex<HashMap<String, Secret>>>);

impl SessionPasswords {
    /// Ids are compared in their canonical lowercase form (the store spells them in upper case).
    fn key(connection: &str) -> String {
        connection.to_ascii_lowercase()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Secret>> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keeps `password` for `connection` (an empty one says "this connection has no password").
    pub fn set(&self, connection: &str, password: &str) {
        self.map().insert(Self::key(connection), Secret::new(password));
    }

    pub fn get(&self, connection: &str) -> Option<Secret> {
        self.map().get(&Self::key(connection)).cloned()
    }

    pub fn contains(&self, connection: &str) -> bool {
        self.map().contains_key(&Self::key(connection))
    }

    pub fn clear(&self, connection: &str) {
        self.map().remove(&Self::key(connection));
    }

    /// The connectable URI of `conn` with the password typed for it, when it saves none and one
    /// was typed. `None`: nothing was typed (the caller reads the credential store, or connects
    /// without a password).
    pub fn runtime_uri(&self, conn: &SavedConnection) -> Option<Result<RuntimeUri, String>> {
        if conn.save_password {
            return None;
        }
        let password = self.get(&conn.id.to_string())?;
        Some(
            resolve_runtime_uri_from(conn, Some(password.expose()))
                .map(RuntimeUri::new)
                .map_err(|error| error.to_string()),
        )
    }
}

impl std::fmt::Debug for SessionPasswords {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionPasswords(<hidden>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::ConnectionKind;

    fn saved(uri: &str, save_password: bool) -> SavedConnection {
        SavedConnection {
            id: uuid::Uuid::new_v4(),
            name: "n".into(),
            uri: uri.into(),
            kind: ConnectionKind::Postgres,
            save_password,
            is_pinned: false,
            disabled: false,
            ssh_tunnel: false,
        }
    }

    #[test]
    fn a_typed_password_is_used_only_by_a_connection_that_saves_none() {
        let passwords = SessionPasswords::default();
        let conn = saved("postgresql://alice@db.example.com/app", false);
        assert!(passwords.runtime_uri(&conn).is_none(), "nothing typed yet");
        passwords.set(&conn.id.to_string().to_ascii_uppercase(), "s3 cret");
        let uri = passwords.runtime_uri(&conn).unwrap().unwrap();
        assert!(uri.expose().starts_with("postgresql://alice:s3%20cret@db.example.com"), "{}", uri.expose());
        let mut saving = conn.clone();
        saving.save_password = true;
        assert!(passwords.runtime_uri(&saving).is_none(), "a saving connection reads the credential store");
        passwords.clear(&conn.id.to_string());
        assert!(!passwords.contains(&conn.id.to_string()));
        assert_eq!(format!("{passwords:?}"), "SessionPasswords(<hidden>)");
    }

    #[test]
    fn expects_password_means_no_saved_password_and_a_user_name() {
        let mut conn = saved("postgresql://alice@db.example.com/app", false);
        assert!(conn.expects_password());
        conn.uri = "mongodb://root@h1:27017,h2:27017/?replicaSet=rs".into();
        assert!(conn.expects_password());
        conn.uri = "redis://:@127.0.0.1:6379".into();
        assert!(!conn.expects_password(), "a password-only form has no user");
        conn.uri = "mysql://db.example.com/app".into();
        assert!(!conn.expects_password(), "no user");
        conn.uri = "postgresql://alice@db.example.com/app".into();
        conn.save_password = true;
        assert!(!conn.expects_password(), "the credential store has it");
        conn.save_password = false;
        conn.kind = ConnectionKind::Sqlite;
        conn.uri = "sqlite:///tmp/a@b.db".into();
        assert!(!conn.expects_password());
    }

    #[test]
    fn auth_failures_are_told_from_other_failures() {
        use crate::records::is_auth_failure;
        for text in [
            "password authentication failed for user \"bob\"",
            "Access denied for user 'root'@'localhost'",
            "Redis connection failed: WRONGPASS invalid username-password pair",
            "Authentication failed.",
        ] {
            assert!(is_auth_failure(text), "{text}");
        }
        for text in ["connection refused", "timed out", "could not translate host name"] {
            assert!(!is_auth_failure(text), "{text}");
        }
    }
}
