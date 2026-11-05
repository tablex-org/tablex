//! What an agent may do on a connection: the access lattice.
//!
//! The access level is an ordered enum, so "at most" is [`Ord::min`]. The one
//! function that combines a connection's setting with its connection type is
//! [`AgentAccess::effective`]; nothing else computes a level.

use serde::{Deserialize, Serialize};
use std::fmt;

/// How far an agent may go on one connection. Ordered: each level includes the
/// ones below it.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentAccess {
    /// No tool touches the connection (the default).
    #[default]
    Off,
    /// List and describe objects; no row data leaves the machine.
    SchemaOnly,
    /// Run statements that only read.
    Read,
    /// Run writes, each one approved by the front end.
    ReadWrite,
}

impl AgentAccess {
    pub const ALL: [Self; 4] = [Self::Off, Self::SchemaOnly, Self::Read, Self::ReadWrite];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::SchemaOnly => "schema_only",
            Self::Read => "read",
            Self::ReadWrite => "read_write",
        }
    }

    /// Effective access: the connection's own setting, else the connection
    /// type's preset, capped by the type.
    ///
    /// * An explicit connection setting wins over the type's preset.
    /// * Without one, a production type is [`Off`](Self::Off); other types use
    ///   their preset, which can never grant more than [`Read`](Self::Read)
    ///   (writes need an explicit setting on the connection).
    /// * A `read_only` type caps the result at [`Read`](Self::Read), whatever
    ///   the setting says.
    pub fn effective(explicit: Option<Self>, connection_type: &ConnectionTypeInfo) -> Self {
        let base = match explicit {
            Some(access) => access,
            None if connection_type.production => Self::Off,
            None => connection_type
                .default_agent_access
                .map_or(Self::Off, |preset| preset.min(Self::Read)),
        };
        if connection_type.read_only {
            base.min(Self::Read)
        } else {
            base
        }
    }
}

impl fmt::Display for AgentAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The parts of a connection type that matter to the agent policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionTypeInfo {
    /// Display name (`Development`, `Production`, ...).
    pub name: String,
    /// The type refuses writes.
    pub read_only: bool,
    /// The type is a production type.
    pub production: bool,
    /// The type's preset for connections without their own setting.
    #[serde(default)]
    pub default_agent_access: Option<AgentAccess>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use AgentAccess::*;

    fn kind(read_only: bool, production: bool, preset: Option<AgentAccess>) -> ConnectionTypeInfo {
        ConnectionTypeInfo {
            name: "t".into(),
            read_only,
            production,
            default_agent_access: preset,
        }
    }

    #[test]
    fn levels_are_ordered() {
        assert!(Off < SchemaOnly && SchemaOnly < Read && Read < ReadWrite);
        assert_eq!(AgentAccess::default(), Off);
    }

    /// Hand-written expectation for every combination, so the function is not
    /// tested against itself. Columns: explicit setting, then the effective
    /// level for (plain, read-only, production, read-only + production).
    #[test]
    fn effective_matrix_without_a_type_preset() {
        let none: Option<AgentAccess> = None;
        let rows: [(Option<AgentAccess>, [AgentAccess; 4]); 5] = [
            (none, [Off, Off, Off, Off]),
            (Some(Off), [Off, Off, Off, Off]),
            (
                Some(SchemaOnly),
                [SchemaOnly, SchemaOnly, SchemaOnly, SchemaOnly],
            ),
            (Some(Read), [Read, Read, Read, Read]),
            (Some(ReadWrite), [ReadWrite, Read, ReadWrite, Read]),
        ];
        for (explicit, expected) in rows {
            let types = [
                kind(false, false, None),
                kind(true, false, None),
                kind(false, true, None),
                kind(true, true, None),
            ];
            for (ty, want) in types.iter().zip(expected) {
                assert_eq!(
                    AgentAccess::effective(explicit, ty),
                    want,
                    "explicit {explicit:?}, read_only {}, production {}",
                    ty.read_only,
                    ty.production
                );
            }
        }
    }

    #[test]
    fn a_type_preset_never_grants_more_than_read_and_never_on_production() {
        let expected_without_explicit = |preset: AgentAccess, production: bool| {
            if production {
                Off
            } else {
                preset.min(Read)
            }
        };
        for preset in AgentAccess::ALL {
            for read_only in [false, true] {
                for production in [false, true] {
                    let ty = kind(read_only, production, Some(preset));
                    assert_eq!(
                        AgentAccess::effective(None, &ty),
                        expected_without_explicit(preset, production),
                        "preset {preset}, read_only {read_only}, production {production}"
                    );
                    // An explicit setting overrides the preset, subject to the cap.
                    for explicit in AgentAccess::ALL {
                        let want = if read_only {
                            explicit.min(Read)
                        } else {
                            explicit
                        };
                        assert_eq!(AgentAccess::effective(Some(explicit), &ty), want);
                    }
                }
            }
        }
    }

    #[test]
    fn the_effective_level_never_exceeds_the_explicit_setting_or_the_cap() {
        for explicit in AgentAccess::ALL {
            for read_only in [false, true] {
                for production in [false, true] {
                    for preset in [None, Some(Read), Some(ReadWrite)] {
                        let ty = kind(read_only, production, preset);
                        let got = AgentAccess::effective(Some(explicit), &ty);
                        assert!(got <= explicit);
                        if read_only {
                            assert!(got <= Read);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn serde_names_are_stable() {
        assert_eq!(serde_json::to_string(&ReadWrite).unwrap(), "\"read_write\"");
        assert_eq!(
            serde_json::from_str::<AgentAccess>("\"schema_only\"").unwrap(),
            SchemaOnly
        );
        assert!(serde_json::from_str::<AgentAccess>("\"admin\"").is_err());
    }
}
