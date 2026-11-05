//! Pure-Rust SSH local port-forward tunnels for TableX: direct, through jump hosts (ProxyJump), and/or through
//! a SOCKS5 / HTTP CONNECT proxy; or through a proxy alone.
//!
//! ```no_run
//! use std::sync::Arc;
//! use engine_net::{Auth, NoPrompts, Secret, Target, Tunnel, TunnelSpec};
//!
//! let mut spec = TunnelSpec::new(
//!     "bastion.example.com",
//!     "deploy",
//!     Auth::Password(Secret::new("...")),
//!     "/path/to/app-data/known_hosts".into(),
//! );
//! let db = Target::new("db.internal.example.com", 5432);
//! spec.targets.push(db.clone());
//! let tunnel = Tunnel::open(spec, Arc::new(NoPrompts))?; // blocking: call from a background thread
//! let local = tunnel.local_addr(&db).unwrap();           // connect the database driver to 127.0.0.1:<port>
//! # Ok::<(), engine_net::TunnelError>(())
//! ```
//!
//! The crate owns a small tokio runtime, so callers need no async runtime of their own: [`Tunnel::open`]
//! blocks the calling (background) thread and [`Tunnel::open_async`] returns a runtime-agnostic future.

// Errors are cold-path values carrying their context; boxing them would only obscure the API.
#![allow(clippy::result_large_err)]

mod auth;
mod error;
mod glob;
mod hostname;
mod known_hosts;
mod paths;
mod proxy;
mod runtime;
mod secret;
mod session;
mod spec;
mod ssh_config;
mod tunnel;

pub use error::{AuthError, ConfigError, HopId, HopInfo, TunnelError};
pub use known_hosts::{trust_host_key, HostKeyStatus, KnownHosts};
pub use paths::{
    default_ssh_config, default_system_known_hosts, default_user_known_hosts, expand_tilde,
    home_dir,
};
pub use proxy::{ProxyCredentials, ProxyKind, ProxyRoute, ProxySpec};
pub use secret::Secret;
pub use spec::{
    Auth, CancelToken, Jump, KeyboardInteractiveRequest, NoPrompts, PromptItem, Prompts, Target,
    Timeouts, TunnelSpec, DEFAULT_FORWARD_BUFFER_BYTES, DEFAULT_MAX_CONCURRENT_FORWARDS,
    DEFAULT_TUNNEL_BUFFER_BYTES, MAX_JUMPS,
};
pub use ssh_config::{
    parse_proxy_jump, Directive, JumpHost, ResolvedChain, ResolvedHost, SshConfig,
};
pub use tunnel::Tunnel;

/// Live tasks on the crate's runtime. Test support only (leak assertions); not part of the API.
#[doc(hidden)]
pub fn alive_task_count() -> usize {
    runtime::alive_tasks()
}
