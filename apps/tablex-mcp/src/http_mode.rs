//! `--http` and `--print-token` in the process shell: what is checked and found before anything is
//! set up (the address, the token), and the listener around [`McpServer::serve_http`].

use std::process::ExitCode;

use tablex_mcp::cli::HttpArgs;
use tablex_mcp::http::{self, HttpConfig};
use tablex_mcp::server::McpServer;
use tablex_mcp::token::{self, TokenSource, TOKEN_ENV};
use tablex_mcp::transport::{ServeError, Signals};

use super::{fail, print_and_exit};

/// Extra `Host` and `Origin` values for `--allow-remote` (comma separated).
const ALLOWED_HOSTS_ENV: &str = "TABLEX_MCP_ALLOWED_HOSTS";
const ALLOWED_ORIGINS_ENV: &str = "TABLEX_MCP_ALLOWED_ORIGINS";

/// What `--http` prepared: the address, the server's settings and where the token came from.
pub struct Plan {
    args: HttpArgs,
    config: HttpConfig,
    source: TokenSource,
}

/// `--print-token`: the token the server would use, on stdout (it is the one thing that prints it).
pub fn print_token() -> ExitCode {
    match token::load(std::env::var_os(TOKEN_ENV), token::default_path().as_deref()) {
        Ok((token, _)) => print_and_exit(&format!("{}\n", token.reveal())),
        Err(error) => fail(&error.to_string()),
    }
}

/// `--http`: refuses an address other machines can reach without `--allow-remote`, finds the token
/// and starts the server. Nothing is set up before both are settled, so a mistake never leaves a
/// half-started process.
pub fn serve_http(args: HttpArgs) -> ExitCode {
    if !http::is_loopback(&args.addr) && !args.allow_remote {
        eprintln!(
            "tablex-mcp: {} is not a loopback address: anything that can reach it could talk to \
             the server (with the token). Use 127.0.0.1 or ::1, or add --allow-remote.",
            args.addr
        );
        return ExitCode::from(2);
    }
    let (token, source) =
        match token::load(std::env::var_os(TOKEN_ENV), token::default_path().as_deref()) {
            Ok(found) => found,
            Err(error) => return fail(&error.to_string()),
        };
    let mut config = HttpConfig::new(token);
    config.extra_hosts = list_from_env(ALLOWED_HOSTS_ENV);
    config.extra_origins = list_from_env(ALLOWED_ORIGINS_ENV);
    super::serve(Some(Plan {
        args,
        config,
        source,
    }))
}

/// A comma separated list in an environment variable; empty items are dropped.
fn list_from_env(name: &str) -> Vec<String> {
    std::env::var(name)
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Binds, says where and how it is protected, and serves until a termination signal.
pub async fn run(
    server: McpServer,
    plan: Plan,
    signals: &mut Signals,
) -> Result<(), ServeError> {
    let Plan {
        args,
        config,
        source,
    } = plan;
    let listener = tokio::net::TcpListener::bind(args.addr)
        .await
        .map_err(ServeError::Listener)?;
    // The address with the port the system chose, when `:0` was asked.
    let bound = listener.local_addr().unwrap_or(args.addr);
    if !http::is_loopback(&bound) {
        eprintln!(
            "tablex-mcp: WARNING: listening on {bound}, which other machines can reach. The \
             connection is plain HTTP: the token and every query and result cross the network \
             unencrypted unless you put TLS or an SSH tunnel in front. Anyone who has the token \
             can use every connection agents may access."
        );
    }
    if let Some(note) = http::wildcard_hosts_note(&bound, &config.extra_hosts, ALLOWED_HOSTS_ENV) {
        eprintln!("tablex-mcp: NOTE: {note}");
    }
    eprintln!(
        "tablex-mcp: serving MCP over HTTP at http://{bound}{}",
        http::ENDPOINT
    );
    match source {
        TokenSource::Env => eprintln!("tablex-mcp: the bearer token is {TOKEN_ENV}"),
        TokenSource::File(path) => eprintln!(
            "tablex-mcp: the bearer token is in {} (show it with --print-token)",
            path.display()
        ),
    }
    server
        .serve_http(listener, config, async || {
            signals.next().await;
        })
        .await
}
