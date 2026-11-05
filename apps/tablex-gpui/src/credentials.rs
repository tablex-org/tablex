//! OS credential storage for saved connection passwords: lives in `engine-connect` (shared with
//! `tablex-mcp`); the app reaches it under its old path.

pub use engine_connect::credentials::*;
