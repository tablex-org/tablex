//! Opening a saved TableX connection without a window: the logic the GPUI app and the standalone
//! `tablex-mcp` binary share, so a second front end cannot fork security-critical code.
//!
//! | module | what |
//! |---|---|
//! | [`app_channel`] | the local socket from `tablex-mcp` to the running app: open a table, show a query, reload connections |
//! | [`platform`] | where TableX keeps its files; the OS credential store's name |
//! | [`records`] | the saved connection model, kinds, URI helpers, export / duplicate redaction |
//! | [`credentials`] | the secret store trait, the OS store (legacy items) and [`credentials::default_store`] |
//! | [`secret_store`] | the encrypted secrets table of the app database and its one master key in the OS store |
//! | [`session_passwords`] | the passwords typed for connections that save none, in memory for this run |
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

pub mod app_channel;
pub mod connect;
pub mod credentials;
pub mod drivers;
pub mod platform;
pub mod profile;
pub mod records;
pub mod secret_store;
pub mod session_passwords;
pub mod tool_endpoint;
pub mod tunnel;
