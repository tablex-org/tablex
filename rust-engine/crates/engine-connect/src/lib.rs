//! Opening a saved TableX connection without a window: the logic the GPUI app and the standalone
//! `tablex-mcp` binary share, so a second front end cannot fork security-critical code.
//!
//! | module | what |
//! |---|---|
//! | [`platform`] | where TableX keeps its files; the OS credential store's name |
//! | [`records`] | the saved connection model, kinds, URI helpers, export / duplicate redaction |
//! | [`credentials`] | the keyring-backed secret store and its entry naming |
//! | [`tool_endpoint`] | the parts of a connection's URI a native client tool (`pg_dump`, `mysqldump`) connects with |
//! | [`tunnel`] | the SSH tunnel plan (URI rewrite), settings, secrets and the `engine-net` seam |
//! | [`drivers`] | driver store, manifest trust keys and policy, verified loading |
//! | [`profile`] | connection types, per-connection overrides, effective values, agent access |
//! | [`connect`] | [`connect::Connector`]: a saved connection id to a live driver session |
//!
//! No GPUI, no dialogs: every question to the user is a callback ([`connect::TunnelPrompts`]).

// `ConnectFailure` carries the details of a host-key problem (algorithms, fingerprints, locations);
// it is the cold path of a connect, moved here unchanged from the app, where boxing it would only
// add noise to every `match`.
#![allow(clippy::result_large_err)]

pub mod connect;
pub mod credentials;
pub mod drivers;
pub mod platform;
pub mod profile;
pub mod records;
pub mod tool_endpoint;
pub mod tunnel;
