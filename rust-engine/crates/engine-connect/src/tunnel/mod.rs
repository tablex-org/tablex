//! SSH tunnels of a saved connection: which `host:port` is forwarded and how the URI is rewritten
//! ([`plan`], pure string work), and the settings, secrets, opener seam and failure typing around
//! `engine-net` ([`ssh`]), with the jump hosts and the proxy of the settings in [`route`].

pub mod plan;
pub mod route;
pub mod ssh;
