//! The command line: one optional flag, parsed by hand. `--version`, `--help` and `--print-config`
//! print to stdout and exit; they never start the server.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// No argument: speak MCP on stdin / stdout.
    Serve,
    Version,
    Help,
    PrintConfig,
    /// Read the saved secrets of the connections agents may use, so macOS asks for them once.
    AuthorizeKeychain,
    /// The child `--authorize-keychain` starts to check the access was kept (hidden: not in
    /// [`USAGE`]).
    VerifyKeychainAccess,
}

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
Usage: tablex-mcp [OPTION]

MCP server (stdio) for TableX. With no option it serves: the MCP client starts it and talks to it
on stdin / stdout. Access is off by default; set a default for all connections in
TableX > Settings > AI > Default agent access, or one per connection in
TableX > connection > Overrides > Agent access.

Options:
      --print-config   print a ready-to-paste MCP client configuration and exit
      --authorize-keychain
                       let this program read the saved passwords of the connections agents
                       may use (macOS); run it once after installing or updating
  -V, --version        print the version and exit
  -h, --help           print this help and exit
";

impl Command {
    /// `args` excludes the program name. An argument that is not valid UTF-8 is an unknown one.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, UsageError> {
        let mut args = args.into_iter();
        let Some(first) = args.next() else {
            return Ok(Self::Serve);
        };
        if let Some(extra) = args.next() {
            return Err(UsageError(format!(
                "unexpected argument `{}`",
                extra.to_string_lossy()
            )));
        }
        let Some(first) = first.to_str() else {
            return Err(UsageError(format!(
                "unknown argument `{}` (not valid UTF-8)",
                first.to_string_lossy()
            )));
        };
        match first {
            "-V" | "--version" => Ok(Self::Version),
            "-h" | "--help" => Ok(Self::Help),
            "--print-config" => Ok(Self::PrintConfig),
            "--authorize-keychain" => Ok(Self::AuthorizeKeychain),
            "--verify-keychain-access" => Ok(Self::VerifyKeychainAccess),
            other => Err(UsageError(format!("unknown argument `{other}`"))),
        }
    }
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
