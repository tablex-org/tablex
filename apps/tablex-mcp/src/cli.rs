//! The command line, parsed by hand: one command flag, and `--allow-remote` with `--http`.
//! `--version`, `--help`, `--print-config` and `--print-token` print to stdout and exit; they
//! never start the server.

use std::ffi::OsString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// No argument: speak MCP on stdin / stdout.
    Serve,
    /// `--http [ADDR]`: speak MCP over Streamable HTTP.
    ServeHttp(HttpArgs),
    /// Show the bearer token of the HTTP transport (made on first use).
    PrintToken,
    Version,
    Help,
    PrintConfig,
    /// Read the saved secrets of the connections agents may use, so macOS asks for them once.
    AuthorizeKeychain,
    /// The child `--authorize-keychain` starts to check the access was kept (hidden: not in
    /// [`USAGE`]).
    VerifyKeychainAccess,
}

/// What `--http` asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpArgs {
    pub addr: SocketAddr,
    /// `--allow-remote`: `addr` may be an address other machines can reach.
    pub allow_remote: bool,
}

/// The port of `--http` without an address.
pub const DEFAULT_HTTP_PORT: u16 = 7717;

/// The address of `--http` without one.
pub const DEFAULT_HTTP_ADDR: SocketAddr =
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_HTTP_PORT);

/// An argument the program does not know (the usage goes to stderr, exit code 2).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct UsageError(String);

/// Why the client configuration cannot be printed (exit code 1).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot find this executable's path: {0}")]
    Executable(#[from] io::Error),
    /// JSON can only carry a path that is valid Unicode.
    #[error("this executable's path is not valid UTF-8, so it cannot be written into a JSON client configuration")]
    PathNotUtf8,
}

pub const USAGE: &str = "\
Usage: tablex-mcp [OPTION]...

MCP server for tablex. With no option it serves over stdio: the MCP client starts it and talks to
it on stdin / stdout. Access is off by default; set a default for all connections in
tablex > Settings > AI > Default agent access, or one per connection in
tablex > connection > Overrides > Agent access.

Options:
      --http [ADDR]    serve MCP over Streamable HTTP at http://ADDR/mcp instead of stdio
                       (default 127.0.0.1:7717; a bare number is a port on 127.0.0.1). Every
                       request needs the bearer token from the TABLEX_MCP_TOKEN environment
                       variable or the token file in the tablex data directory
      --allow-remote   with --http: allow an address that is not loopback (plain HTTP: put TLS or
                       an SSH tunnel in front, and read the security notes first)
      --print-token    print the HTTP bearer token (made on first use) and exit
      --print-config   print a ready-to-paste MCP client configuration and exit
      --authorize-keychain
                       let this program read the tablex master key (one Keychain item) and
                       the saved passwords of the connections agents may use (macOS); run it
                       once after installing, and again if macOS asks after an update
  -V, --version        print the version and exit
  -h, --help           print this help and exit
";

impl Command {
    /// `args` excludes the program name. An argument that is not valid UTF-8 is an unknown one.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, UsageError> {
        let args = args
            .into_iter()
            .map(|arg| {
                arg.into_string().map_err(|arg| {
                    UsageError(format!(
                        "unknown argument `{}` (not valid UTF-8)",
                        arg.to_string_lossy()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let Some(first) = args.first().copied() else {
            return Ok(Self::Serve);
        };
        if matches!(first.split('=').next(), Some("--http" | "--allow-remote")) {
            return Self::parse_http(&args);
        }
        if let Some(extra) = args.get(1) {
            return Err(UsageError(format!("unexpected argument `{extra}`")));
        }
        match first {
            "-V" | "--version" => Ok(Self::Version),
            "-h" | "--help" => Ok(Self::Help),
            "--print-config" => Ok(Self::PrintConfig),
            "--print-token" => Ok(Self::PrintToken),
            "--authorize-keychain" => Ok(Self::AuthorizeKeychain),
            "--verify-keychain-access" => Ok(Self::VerifyKeychainAccess),
            other => Err(UsageError(format!("unknown argument `{other}`"))),
        }
    }

    /// `--http [ADDR]` (or `--http=ADDR`) and `--allow-remote`, in either order, each once; ADDR is
    /// the next argument unless that is a flag.
    fn parse_http(args: &[&str]) -> Result<Self, UsageError> {
        let mut http = false;
        let mut addr = None;
        let mut allow_remote = false;
        let mut args = args.iter().copied().peekable();
        while let Some(arg) = args.next() {
            match arg.split_once('=') {
                Some(("--http", value)) if !http => {
                    http = true;
                    addr = Some(parse_addr(value)?);
                }
                None if arg == "--http" && !http => {
                    http = true;
                    addr = args
                        .next_if(|next| !next.starts_with('-'))
                        .map(parse_addr)
                        .transpose()?;
                }
                None if arg == "--allow-remote" && !allow_remote => allow_remote = true,
                _ => return Err(UsageError(format!("unexpected argument `{arg}`"))),
            }
        }
        if !http {
            return Err(UsageError("`--allow-remote` is for `--http`".to_owned()));
        }
        Ok(Self::ServeHttp(HttpArgs {
            addr: addr.unwrap_or(DEFAULT_HTTP_ADDR),
            allow_remote,
        }))
    }
}

/// `HOST:PORT` with HOST an IP address (`127.0.0.1:7717`, `[::1]:7717`), or a bare port on the
/// loopback interface. A name is not resolved: what is listened on must be exact.
fn parse_addr(text: &str) -> Result<SocketAddr, UsageError> {
    if let Ok(port) = text.parse::<u16>() {
        return Ok(SocketAddr::new(DEFAULT_HTTP_ADDR.ip(), port));
    }
    text.parse().map_err(|_| {
        UsageError(format!(
            "`{text}` is not an address: use IP:PORT (for example 127.0.0.1:7717) or a port"
        ))
    })
}

/// `tablex-mcp 0.1.0`.
pub fn version_line() -> String {
    format!("{} {}\n", crate::NAME, env!("CARGO_PKG_VERSION"))
}

/// The client configuration for the executable at `exe` (an absolute path), as the
/// `mcpServers` JSON most MCP clients (Claude Desktop, Claude Code, Cursor) accept.
pub fn client_config(exe: &Path) -> Result<String, ConfigError> {
    let exe = exe.to_str().ok_or(ConfigError::PathNotUtf8)?;
    let config = serde_json::json!({
        "mcpServers": { "tablex": { "command": exe, "args": [] } }
    });
    // Printing a `Value` cannot fail.
    Ok(format!("{config:#}\n"))
}

/// [`client_config`] for the running executable.
pub fn current_client_config() -> Result<String, ConfigError> {
    let exe: PathBuf = std::env::current_exe()?;
    // `current_exe` can be relative on some platforms when started through a relative path.
    let exe = std::path::absolute(exe)?;
    client_config(&exe)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, UsageError> {
        Command::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn no_argument_serves_and_each_flag_is_one_command() {
        assert_eq!(parse(&[]), Ok(Command::Serve));
        assert_eq!(parse(&["--version"]), Ok(Command::Version));
        assert_eq!(parse(&["-V"]), Ok(Command::Version));
        assert_eq!(parse(&["--help"]), Ok(Command::Help));
        assert_eq!(parse(&["-h"]), Ok(Command::Help));
        assert_eq!(parse(&["--print-config"]), Ok(Command::PrintConfig));
        assert_eq!(
            parse(&["--authorize-keychain"]),
            Ok(Command::AuthorizeKeychain)
        );
    }

    #[test]
    fn the_verify_command_is_hidden_and_neither_keychain_flag_combines() {
        assert_eq!(
            parse(&["--verify-keychain-access"]),
            Ok(Command::VerifyKeychainAccess)
        );
        assert!(!USAGE.contains("verify-keychain-access"));
        assert!(USAGE.contains("--authorize-keychain"));
        for flag in ["--authorize-keychain", "--verify-keychain-access"] {
            assert!(parse(&[flag, "--help"]).is_err());
            assert!(parse(&["--print-config", flag]).is_err());
        }
        assert!(parse(&["--authorize-keychain", "--verify-keychain-access"]).is_err());
    }

    fn http(addr: &str, allow_remote: bool) -> Result<Command, UsageError> {
        Ok(Command::ServeHttp(HttpArgs {
            addr: addr.parse().unwrap(),
            allow_remote,
        }))
    }

    #[test]
    fn http_takes_an_optional_address_and_allow_remote_in_either_order() {
        assert_eq!(parse(&["--http"]), http("127.0.0.1:7717", false));
        assert_eq!(parse(&["--http", "9000"]), http("127.0.0.1:9000", false));
        assert_eq!(parse(&["--http", "127.0.0.1:0"]), http("127.0.0.1:0", false));
        assert_eq!(parse(&["--http=[::1]:8000"]), http("[::1]:8000", false));
        assert_eq!(
            parse(&["--http", "0.0.0.0:7717", "--allow-remote"]),
            http("0.0.0.0:7717", true)
        );
        assert_eq!(
            parse(&["--allow-remote", "--http", "10.0.0.5:80"]),
            http("10.0.0.5:80", true)
        );
        // The next flag is not an address.
        assert_eq!(parse(&["--http", "--allow-remote"]), http("127.0.0.1:7717", true));
        assert_eq!(parse(&["--print-token"]), Ok(Command::PrintToken));
    }

    #[test]
    fn http_arguments_that_do_not_belong_together_are_usage_errors() {
        for args in [
            &["--allow-remote"][..],
            &["--http", "localhost:80"],
            &["--http", "99999"],
            &["--http", "--http"],
            &["--http", "1.2.3.4:5", "extra"],
            &["--http", "--allow-remote", "--allow-remote"],
            &["--http", "--help"],
            &["--print-token", "--http"],
            &["--print-token", "--allow-remote"],
            &["--http=", "--allow-remote"],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
        assert!(USAGE.contains("--http") && USAGE.contains("--allow-remote"));
        assert!(USAGE.contains("--print-token"));
    }

    #[test]
    fn anything_else_is_a_usage_error() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["serve"]).is_err());
        assert!(parse(&["--version", "--help"]).is_err());
    }

    #[test]
    fn the_client_config_is_json_with_the_command_and_no_arguments() {
        let text = client_config(Path::new("/opt/tablex/tablex-mcp")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"mcpServers": {"tablex": {"command": "/opt/tablex/tablex-mcp", "args": []}}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_argument_is_an_unknown_argument_not_a_panic() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'-', b'-', 0xff]);
        let error = Command::parse([bad.clone()]).unwrap_err();
        assert!(error.to_string().contains("not valid UTF-8"), "{error}");
        assert!(Command::parse([OsString::from("--help"), bad]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_executable_path_is_an_error_not_a_panic() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/opt/tablex/\xff/tablex-mcp"));
        assert!(matches!(client_config(path), Err(ConfigError::PathNotUtf8)));
    }
}
