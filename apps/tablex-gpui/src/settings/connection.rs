//! Connection types (Development / Test / Production / custom) and the
//! per-connection overrides of the settings, and the effective values a
//! connection ends up with.
//!
//! The model (types, overrides, the effective-value function, the agent access level) lives in
//! `engine-connect::profile`, shared with `tablex-mcp`, and is re-exported here under the old path.
//! This module keeps what needs the settings registry (the UI descriptors) and the whole
//! [`Settings`]: checking an override against its descriptor, showing it as a registry value, and
//! the `Settings` methods that hand the global values to the shared resolver.
//!
//! Precedence, strongest first: the connection's own override, the preset of
//! its connection type, the global setting. Only the settings flagged
//! `overridable` in the registry can be overridden; the safety presets (confirm
//! execution, confirm data changes, read-only, auto-commit, auto-close idle
//! transactions) belong to the type and have no global counterpart, so a
//! connection without a type has none of them on.
//!
//! A saved connection without a type (every connection saved before this
//! existed) behaves exactly as before: global settings only, no badge.

use super::registry::{self, Kind, SettingDescriptor, Value};
use super::Settings;
pub use engine_connect::profile::*;

/// An override as a registry value, and its check against the setting's descriptor.
pub trait OverrideValueExt {
    /// The registry value this override stands for; `None` when it does not
    /// fit the setting's kind (a stale or hand-edited value).
    fn to_value(&self, descriptor: &SettingDescriptor) -> Option<Value>;
}

impl OverrideValueExt for OverrideValue {
    fn to_value(&self, descriptor: &SettingDescriptor) -> Option<Value> {
        match (&descriptor.kind, self) {
            (Kind::Toggle, Self::Bool(value)) => Some(Value::Bool(*value)),
            (Kind::Number { .. }, Self::Number(value)) => Some(Value::Number(*value)),
            (Kind::Text { .. } | Kind::Font, Self::Text(value)) => Some(Value::Text(value.clone())),
            (Kind::Choice { options }, Self::Text(id)) => options
                .iter()
                .find(|(option, _)| option == id)
                .map(|(option, _)| Value::Choice(option)),
            _ => None,
        }
    }
}

pub trait OverridesExt {
    /// Every override must be a valid value of an overridable setting (the agent access level is
    /// checked by the connection dialog's own select, which can only produce a valid one).
    fn validate(&self) -> Result<(), String>;
}

impl OverridesExt for Overrides {
    fn validate(&self) -> Result<(), String> {
        for (id, value) in self.iter() {
            let Some(descriptor) = registry::find(id).filter(|d| d.overridable) else {
                continue;
            };
            let valid = value
                .to_value(descriptor)
                .ok_or_else(|| format!("{}: wrong kind of value", descriptor.title))
                .and_then(|value| {
                    descriptor
                        .validate(value)
                        .map(|_| ())
                        .map_err(|error| format!("{}: {error}", descriptor.title))
                });
            valid?;
        }
        Ok(())
    }
}

impl Settings {
    pub fn connection_type(&self, id: &str) -> Option<&ConnectionType> {
        find_type(&self.connection_types, id)
    }

    /// Makes sure the built-in types exist (a hand-edited file may have lost
    /// one) and that no id repeats.
    pub fn normalize_connection_types(&mut self) {
        normalize_connection_types(&mut self.connection_types);
    }

    /// The effective values of a connection: override > type preset > global.
    pub fn for_connection(&self, profile: &ConnectionProfile) -> Effective {
        Effective::resolve(
            Globals {
                query_timeout_secs: self.query.query_timeout_secs,
                connect_timeout_secs: self.query.connect_timeout_secs,
                keep_alive_secs: self.query.keep_alive_secs,
                page_size: self.results.page_size,
                confirm_dangerous: self.editor.confirm_dangerous,
            },
            &self.connection_types,
            profile,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn profile(type_id: &str, overrides: &[(&str, OverrideValue)]) -> ConnectionProfile {
        let mut profile = ConnectionProfile {
            type_id: type_id.into(),
            ..Default::default()
        };
        for (id, value) in overrides {
            profile.overrides.set(id, value.clone());
        }
        profile
    }

    #[test]
    fn a_connection_without_a_type_follows_the_global_settings_only() {
        let settings = Settings::default();
        let effective = settings.for_connection(&ConnectionProfile::default());
        assert_eq!(effective.connection_type, None);
        assert!(!effective.confirm_execute && !effective.confirm_data_change && !effective.read_only);
        assert!(effective.auto_commit);
        assert_eq!(effective.page_size, settings.results.page_size);
        assert_eq!(effective.query_timeout, None);
        assert!(effective.confirm_dangerous);
        // A type id nobody defines (a deleted custom type) is the same.
        assert_eq!(settings.for_connection(&profile("gone", &[])), effective);
    }

    #[test]
    fn types_saved_before_the_idle_limit_existed_resolve_their_builtin_limit() {
        // A settings file as an older build wrote it: no `idle_transaction_secs` anywhere.
        let mut settings = Settings {
            connection_types: serde_json::from_str(
                r##"[{"id":"development","name":"Development","color":"#22c55e","auto_close_idle_transactions":true},
                     {"id":"test","name":"Test","color":"#3b82f6","auto_close_idle_transactions":true},
                     {"id":"production","name":"Production","color":"#ef4444","confirm_execute":true,"auto_close_idle_transactions":true}]"##,
            )
            .unwrap(),
            ..Settings::default()
        };
        settings.normalize_connection_types();
        let secs = |id: &str| settings.for_connection(&profile(id, &[])).idle_transaction_secs;
        assert_eq!((secs(DEVELOPMENT), secs(TEST), secs(PRODUCTION)), (1800, 900, 600));
        assert!(settings.for_connection(&profile(PRODUCTION, &[])).auto_close_idle_transactions);
    }

    #[test]
    fn the_type_preset_applies_to_its_connections() {
        let mut settings = Settings::default();
        let production = settings.for_connection(&profile(PRODUCTION, &[]));
        assert!(production.confirm_execute && production.confirm_data_change && !production.read_only);
        assert_eq!(production.connection_type.as_ref().map(|ty| ty.name.as_str()), Some("Production"));
        settings
            .connection_types
            .iter_mut()
            .find(|ty| ty.id == PRODUCTION)
            .unwrap()
            .read_only = true;
        assert!(settings.for_connection(&profile(PRODUCTION, &[])).read_only);
        assert!(!settings.for_connection(&profile(DEVELOPMENT, &[])).confirm_execute);
    }

    #[test]
    fn a_connection_override_wins_over_the_global_value() {
        let mut settings = Settings::default();
        settings.query.query_timeout_secs = 10;
        settings.results.page_size = 500;
        let global = settings.for_connection(&ConnectionProfile::default());
        assert_eq!(global.query_timeout, Some(Duration::from_secs(10)));
        assert_eq!(global.page_size, 500);

        let own = profile(
            "",
            &[
                ("query.query_timeout_secs", OverrideValue::Number(90.0)),
                ("results.page_size", OverrideValue::Number(25.0)),
                ("editor.confirm_dangerous", OverrideValue::Bool(false)),
                ("query.connect_timeout_secs", OverrideValue::Number(7.0)),
            ],
        );
        let effective = settings.for_connection(&own);
        assert_eq!(effective.query_timeout, Some(Duration::from_secs(90)));
        assert_eq!(effective.page_size, 25);
        assert!(!effective.confirm_dangerous);
        assert_eq!(effective.connect_timeout_secs, 7);
    }

    #[test]
    fn use_global_falls_back_when_the_override_is_cleared() {
        let mut settings = Settings::default();
        settings.results.page_size = 300;
        let mut own = profile("", &[("results.page_size", OverrideValue::Number(25.0))]);
        assert_eq!(settings.for_connection(&own).page_size, 25);
        own.overrides.clear("results.page_size");
        assert!(own.overrides.is_empty());
        assert_eq!(settings.for_connection(&own).page_size, 300);
        // The global value moving is seen by a connection that has no override.
        settings.results.page_size = 400;
        assert_eq!(settings.for_connection(&own).page_size, 400);
    }

    #[test]
    fn stale_or_invalid_overrides_are_ignored_not_fatal() {
        let settings = Settings::default();
        let own = profile(
            "",
            &[
                ("results.page_size", OverrideValue::Number(0.0)),
                ("results.null_text", OverrideValue::Text("x".into())),
                ("no.such_setting", OverrideValue::Bool(true)),
                ("query.query_timeout_secs", OverrideValue::Bool(true)),
            ],
        );
        let effective = settings.for_connection(&own);
        assert_eq!(effective.page_size, settings.results.page_size, "out of range");
        assert_eq!(effective.query_timeout, None, "wrong kind");
        assert!(own.overrides.validate().is_err());
        assert!(profile("", &[("results.page_size", OverrideValue::Number(10.0))])
            .overrides
            .validate()
            .is_ok());
    }

    #[test]
    fn normalizing_restores_missing_built_ins_and_drops_duplicates() {
        let mut settings = Settings::default();
        settings.connection_types.retain(|ty| ty.id != TEST);
        settings.connection_types.push(ConnectionType {
            id: "custom-1".into(),
            name: "Staging".into(),
            ..Default::default()
        });
        settings.connection_types.push(ConnectionType {
            id: "custom-1".into(),
            name: "Dup".into(),
            ..Default::default()
        });
        settings.normalize_connection_types();
        let ids: Vec<_> = settings.connection_types.iter().map(|ty| ty.id.as_str()).collect();
        assert_eq!(ids, [DEVELOPMENT, TEST, PRODUCTION, "custom-1"]);
        assert_eq!(ConnectionType::new_custom_id(&settings.connection_types), "custom-2");
    }

    #[test]
    fn the_registry_and_the_shared_resolver_agree_on_what_can_be_overridden() {
        let mut registered: Vec<_> = registry::registry().iter().filter(|d| d.overridable).map(|d| d.id).collect();
        let mut shared: Vec<_> = OVERRIDABLE_IDS.into_iter().filter(|id| *id != AGENT_ACCESS).collect();
        registered.sort_unstable();
        shared.sort_unstable();
        assert_eq!(registered, shared, "a new overridable setting needs a case in engine-connect's Globals::apply");
        for id in shared {
            let descriptor = registry::find(id).unwrap();
            let limits = match id {
                QUERY_TIMEOUT_SECS => QUERY_TIMEOUT_SECS_LIMITS,
                CONNECT_TIMEOUT_SECS => CONNECT_TIMEOUT_SECS_LIMITS,
                KEEP_ALIVE_SECS => KEEP_ALIVE_SECS_LIMITS,
                PAGE_SIZE => PAGE_SIZE_LIMITS,
                _ => continue,
            };
            assert_eq!(descriptor.kind, Kind::Number { min: limits.0, max: limits.1, step: limits.2 }, "{id}");
        }
        assert!(registry::find(AGENT_ACCESS).is_none(), "the agent level has its own controls");
    }

    #[test]
    fn the_shared_resolver_gives_what_the_settings_say_and_the_default_page_size_is_shared() {
        assert_eq!(Settings::default().results.page_size, DEFAULT_PAGE_SIZE);
        assert_eq!(crate::query::DEFAULT_OUTPUT_LIMIT, i64::from(DEFAULT_PAGE_SIZE));
        let mut settings = Settings::default();
        settings.connection_types.iter_mut().find(|ty| ty.id == PRODUCTION).unwrap().read_only = true;
        let production = profile(PRODUCTION, &[(AGENT_ACCESS, OverrideValue::Text("read_write".into()))]);
        let effective = settings.for_connection(&production);
        assert!(effective.read_only);
        assert_eq!(effective.agent_access, AgentAccess::Read, "a read-only type caps the agent");
    }
}
