//! Runtime connection status shared by the navigator and query tabs: one entry
//! per saved connection, reused until disconnect. The driver session itself
//! lives in `AppRoot::driver_runtime`.

use std::collections::{HashMap, HashSet};

use crate::connections::SavedConnection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

/// Shown for a session that dropped while connected.
pub const CONNECTION_LOST_REASON: &str = "Connection to the database was lost";

/// Whether a stringified driver failure says the session is gone
/// (`driver rejected the request: <area>.not_connected: ...`).
pub fn is_connection_lost_error(error: &str) -> bool {
    crate::error_log::DriverRejection::parse(error)
        .is_some_and(|rejection| is_connection_lost_code(rejection.code))
}

/// Whether a driver error code says the session is gone.
pub fn is_connection_lost_code(code: &str) -> bool {
    code.ends_with(".not_connected")
}

/// Pure session map: saved-connection UUID -> runtime status.
#[derive(Debug, Default)]
pub struct ConnectionSessions {
    statuses: HashMap<String, ConnectionStatus>,
    generations: HashMap<String, u64>,
    next_generation: u64,
    /// Connections that were live and then disconnected (user action or
    /// teardown), until the next connect attempt. Tells "was connected, now
    /// gone" from "never connected" (both read `Disconnected`).
    dropped: HashSet<String>,
}

impl ConnectionSessions {
    pub fn status(&self, connection: &str) -> ConnectionStatus {
        self.statuses
            .get(connection)
            .cloned()
            .unwrap_or(ConnectionStatus::Disconnected)
    }

    pub fn is_connected(&self, connection: &str) -> bool {
        self.status(connection) == ConnectionStatus::Connected
    }

    pub fn begin_connect(&mut self, connection: &str) -> Option<u64> {
        match self.status(connection) {
            ConnectionStatus::Disconnected | ConnectionStatus::Failed(_) => {
                self.dropped.remove(connection);
                self.next_generation = self.next_generation.wrapping_add(1);
                let generation = self.next_generation;
                self.generations.insert(connection.to_string(), generation);
                self.statuses
                    .insert(connection.to_string(), ConnectionStatus::Connecting);
                Some(generation)
            }
            _ => None,
        }
    }

    pub fn finish_connect(
        &mut self,
        connection: &str,
        generation: u64,
        result: Result<(), String>,
    ) -> bool {
        if self.generations.get(connection).copied() != Some(generation) {
            return false;
        }
        match self.statuses.get_mut(connection) {
            Some(status) if matches!(status, ConnectionStatus::Connecting) => {
                *status = match result {
                    Ok(()) => ConnectionStatus::Connected,
                    Err(error) => ConnectionStatus::Failed(redact_secrets(&error)),
                };
                true
            }
            _ => false,
        }
    }

    #[cfg(test)]
    /// Flips a live session to `Failed` after a driver reported
    /// `*.not_connected`, so every tab of the connection shows the
    /// reconnect state. `false` when the session was not connected (nothing
    /// changes then).
    pub fn mark_lost(&mut self, connection: &str) -> bool {
        self.mark_lost_because(connection, CONNECTION_LOST_REASON.to_string())
    }

    /// [`Self::mark_lost`] with the reason the banner shows (a dead SSH tunnel names itself).
    pub fn mark_lost_because(&mut self, connection: &str, reason: String) -> bool {
        if !self.is_connected(connection) {
            return false;
        }
        self.generations.remove(connection);
        self.statuses
            .insert(connection.to_string(), ConnectionStatus::Failed(reason));
        true
    }

    /// The connection was live and has been disconnected since.
    pub fn was_dropped(&self, connection: &str) -> bool {
        self.dropped.contains(connection)
    }

    pub fn generation(&self, connection: &str) -> Option<u64> {
        self.generations.get(connection).copied()
    }

    /// Forgets the connection's status; `true` when it was live (it is then
    /// remembered as dropped).
    pub fn disconnect(&mut self, connection: &str) -> bool {
        self.generations.remove(connection);
        let was_connected = self.statuses.remove(connection) == Some(ConnectionStatus::Connected);
        if was_connected {
            self.dropped.insert(connection.to_string());
        }
        was_connected
    }

    /// The saved-connection ids of every live session.
    pub fn connected(&self) -> impl Iterator<Item = &str> {
        self.statuses
            .iter()
            .filter(|(_, status)| **status == ConnectionStatus::Connected)
            .map(|(key, _)| key.as_str())
    }

    pub fn status_keys(&self) -> Vec<String> {
        self.statuses.keys().cloned().collect()
    }
}

pub use engine_connect::records::redact_secrets;

/// Chooses the engine URI for a connection from its persisted state,
/// rehydrating the Keychain password when the user saved one.
pub fn engine_uri(conn: &SavedConnection) -> Result<engine_connect::records::RuntimeUri, String> {
    crate::connections::resolve_runtime_uri(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONN: &str = "conn-1";

    #[test]
    fn begin_connect_only_from_disconnected_or_failed() {
        let mut sessions = ConnectionSessions::default();

        let generation = sessions
            .begin_connect(CONN)
            .expect("first connect should start");
        assert!(
            sessions.begin_connect(CONN).is_none(),
            "connecting blocks re-entry"
        );

        sessions.finish_connect(CONN, generation, Ok(()));
        assert!(
            sessions.begin_connect(CONN).is_none(),
            "connected blocks re-entry"
        );

        sessions.disconnect(CONN);
        assert!(
            sessions.begin_connect(CONN).is_some(),
            "disconnected allows reconnect"
        );
    }

    #[test]
    fn finish_connect_applies_only_to_connecting_sessions() {
        let mut sessions = ConnectionSessions::default();

        assert!(!sessions.finish_connect(CONN, 1, Ok(())), "never began");
        assert_eq!(sessions.status(CONN), ConnectionStatus::Disconnected);

        let generation = sessions.begin_connect(CONN).expect("connect should start");
        assert!(sessions.finish_connect(CONN, generation, Ok(())));
        assert_eq!(sessions.status(CONN), ConnectionStatus::Connected);
    }

    #[test]
    fn failures_allow_retry_and_are_redacted() {
        let mut sessions = ConnectionSessions::default();

        let generation = sessions.begin_connect(CONN).expect("connect should start");
        let applied = sessions.finish_connect(
            CONN,
            generation,
            Err("could not connect: postgres://user:secret@host/db refused".into()),
        );
        assert!(applied);

        let status = sessions.status(CONN);
        let ConnectionStatus::Failed(message) = status else {
            panic!("expected failure, got {status:?}");
        };
        assert!(!message.contains("secret"), "password redacted: {message}");
        assert!(
            !message.contains("postgres://user"),
            "uri redacted: {message}"
        );
        assert!(
            sessions.begin_connect(CONN).is_some(),
            "failure allows retry"
        );
    }

    #[test]
    fn mark_lost_fails_only_connected_sessions() {
        let mut sessions = ConnectionSessions::default();
        assert!(!sessions.mark_lost(CONN), "never connected");
        let generation = sessions.begin_connect(CONN).expect("connect should start");
        assert!(!sessions.mark_lost(CONN), "connecting is untouched");
        sessions.finish_connect(CONN, generation, Ok(()));

        assert!(sessions.mark_lost(CONN));
        assert_eq!(
            sessions.status(CONN),
            ConnectionStatus::Failed(CONNECTION_LOST_REASON.into())
        );
        assert!(!sessions.mark_lost(CONN), "already lost");
        assert!(sessions.begin_connect(CONN).is_some(), "lost allows reconnect");
        assert!(!sessions.was_dropped(CONN));
    }

    #[test]
    fn not_connected_driver_codes_are_detected_even_when_wrapped() {
        for text in [
            "driver rejected the request: query.not_connected: gone",
            "driver rejected the request: session.not_connected: gone",
            "Could not list: driver rejected the request: session.not_connected: gone",
        ] {
            assert!(is_connection_lost_error(text), "{text}");
        }
        for text in [
            "driver rejected the request: query.cancelled: stopped",
            "driver rejected the request: query.not_pageable: needs sort",
            "connection refused",
            "session.not_connected: no prefix",
        ] {
            assert!(!is_connection_lost_error(text), "{text}");
        }
    }

    #[test]
    fn disconnect_reports_a_live_session_once() {
        let mut sessions = ConnectionSessions::default();
        let generation = sessions.begin_connect(CONN).expect("connect should start");
        sessions.finish_connect(CONN, generation, Ok(()));

        assert!(!sessions.was_dropped(CONN));
        assert!(sessions.disconnect(CONN));
        assert!(sessions.was_dropped(CONN), "a live session that was disconnected");
        assert!(!sessions.disconnect(CONN));
        assert_eq!(sessions.status(CONN), ConnectionStatus::Disconnected);
    }

    #[test]
    fn connected_lists_every_live_session() {
        let mut sessions = ConnectionSessions::default();
        let a_generation = sessions.begin_connect("a").expect("connect should start");
        sessions.finish_connect("a", a_generation, Ok(()));
        let b_generation = sessions.begin_connect("b").expect("connect should start");
        sessions.finish_connect("b", b_generation, Ok(()));
        sessions.begin_connect("c");

        let mut ids: Vec<_> = sessions.connected().collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
        assert!(
            sessions.status_keys().contains(&"c".to_string()),
            "refresh must cancel pending connections too"
        );
    }

    #[test]
    fn stale_connect_completion_cannot_replace_a_newer_attempt() {
        let mut sessions = ConnectionSessions::default();

        let first_attempt = sessions
            .begin_connect(CONN)
            .expect("first attempt should start");
        sessions.disconnect(CONN);
        let second_attempt = sessions
            .begin_connect(CONN)
            .expect("second attempt should start");

        assert_ne!(first_attempt, second_attempt);
        assert!(!sessions.finish_connect(CONN, first_attempt, Ok(())));
        assert_eq!(sessions.status(CONN), ConnectionStatus::Connecting);
        assert!(sessions.finish_connect(CONN, second_attempt, Ok(())));
        assert_eq!(sessions.status(CONN), ConnectionStatus::Connected);
    }
}
