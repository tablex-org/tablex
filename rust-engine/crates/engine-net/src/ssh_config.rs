//! Typed reader for `~/.ssh/config`.
//!
//! Understood: `Host` patterns (case-sensitive wildcards, `!negation`), `HostName`, `Port`, `User`,
//! `IdentityFile` (`~` and `%d %h %r %p %u %%` expansion), `ProxyJump` (exposed by [`SshConfig::resolve`] and
//! resolved hop by hop by [`SshConfig::resolve_chain`]). `Keyword value`, `Keyword=value` and `Keyword = value` are all accepted, and a UTF-8 BOM on
//! the first line is skipped. Like OpenSSH, the first value found wins for single-valued options and
//! `IdentityFile` accumulates.
//!
//! Never executed or honoured, and reported in [`ResolvedHost::unsupported`]: `ProxyCommand`, `HostKeyAlias`,
//! `UserKnownHostsFile`, `GlobalKnownHostsFile`, `IdentityAgent` (from applicable blocks) and `Match` /
//! `Include` (anywhere in the file; `Match` blocks are skipped entirely).
//!
//! Values are validated lazily: a malformed `Port` in a `Host` block that does not apply to the alias being
//! resolved does not make the file unusable.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::glob::wildcard_match_exact;
use crate::paths;
use crate::spec::MAX_JUMPS;

/// Larger configs are refused instead of read into memory.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// One hop of a `ProxyJump` chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JumpHost {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

/// A `ProxyJump` chain resolved through the config: what to dial, in order, to reach `target`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedChain {
    /// Dial order, nested `ProxyJump`s already flattened (at most [`MAX_JUMPS`]). Their `proxy_jump` is empty.
    pub jumps: Vec<ResolvedHost>,
    /// The host the user asked for; its `proxy_jump` is empty (it is `jumps`).
    pub target: ResolvedHost,
}

/// A directive this crate does not implement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directive {
    pub keyword: String,
    pub value: String,
    /// 1-based line in the config file.
    pub line: usize,
}

/// A `Host` alias resolved against the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHost {
    pub alias: String,
    /// `HostName`, or the alias itself when none applies.
    pub host_name: String,
    /// `Port`, default 22.
    pub port: u16,
    pub user: Option<String>,
    pub identity_files: Vec<PathBuf>,
    pub proxy_jump: Vec<JumpHost>,
    /// Directives that apply to this alias (or `Match` / `Include`, which may) and are not implemented.
    pub unsupported: Vec<Directive>,
}

#[derive(Debug, Clone)]
struct Pattern {
    negated: bool,
    glob: String,
}

#[derive(Debug, Clone)]
struct Setting {
    keyword: String,
    value: String,
    line: usize,
}

#[derive(Debug, Clone, Default)]
struct Block {
    /// `None` for the implicit leading block, which applies to every host.
    patterns: Option<Vec<Pattern>>,
    options: Vec<Setting>,
}

impl Block {
    fn applies_to(&self, alias: &str) -> bool {
        let Some(patterns) = &self.patterns else {
            return true;
        };
        let hit = |negated: bool| {
            patterns
                .iter()
                .filter(move |p| p.negated == negated)
                .any(|p| wildcard_match_exact(&p.glob, alias))
        };
        hit(false) && !hit(true)
    }
}

const UNSUPPORTED_KEYWORDS: [&str; 5] = [
    "proxycommand",
    "hostkeyalias",
    "userknownhostsfile",
    "globalknownhostsfile",
    "identityagent",
];

#[derive(Debug, Clone, Default)]
pub struct SshConfig {
    blocks: Vec<Block>,
    /// `Match` and `Include` lines: file-global because they may affect any alias.
    global_unsupported: Vec<Directive>,
}

impl SshConfig {
    /// Reads `path`; a missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let io_error = |reason: String| ConfigError::Io {
            path: path.to_owned(),
            reason,
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(io_error(e.to_string())),
        };
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| io_error(e.to_string()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(io_error(format!(
                "larger than {} MiB",
                MAX_FILE_BYTES >> 20
            )));
        }
        Self::parse(&String::from_utf8_lossy(&bytes))
    }

    /// Reads `~/.ssh/config` (empty when there is no home directory or no file).
    pub fn load_default() -> Result<Self, ConfigError> {
        paths::default_ssh_config().map_or_else(|| Ok(Self::default()), |path| Self::load(&path))
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut blocks = vec![Block::default()];
        let mut global_unsupported = Vec::new();
        let mut skipping_match = false;
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let Some((keyword, args)) = split_directive(raw) else {
                continue;
            };
            let keyword = keyword.to_ascii_lowercase();
            match keyword.as_str() {
                "host" => {
                    skipping_match = false;
                    let patterns = args
                        .iter()
                        .map(|a| match a.strip_prefix('!') {
                            Some(glob) => Pattern {
                                negated: true,
                                glob: glob.to_owned(),
                            },
                            None => Pattern {
                                negated: false,
                                glob: a.clone(),
                            },
                        })
                        .collect();
                    blocks.push(Block {
                        patterns: Some(patterns),
                        options: Vec::new(),
                    });
                }
                "match" => {
                    skipping_match = true;
                    global_unsupported.push(Directive {
                        keyword: "Match".into(),
                        value: args.join(" "),
                        line,
                    });
                }
                "include" => {
                    global_unsupported.push(Directive {
                        keyword: "Include".into(),
                        value: args.join(" "),
                        line,
                    });
                }
                _ if skipping_match => {}
                "hostname" | "user" | "identityfile" | "port" | "proxyjump" => {
                    push(&mut blocks, keyword, &args, line)
                }
                k if UNSUPPORTED_KEYWORDS.contains(&k) => push(&mut blocks, keyword, &args, line),
                _ => {}
            }
        }
        Ok(Self {
            blocks,
            global_unsupported,
        })
    }

    /// Concrete (wildcard-free, non-negated) `Host` aliases in file order, for a host picker.
    pub fn host_aliases(&self) -> Vec<String> {
        let mut aliases: Vec<String> = Vec::new();
        let names = self
            .blocks
            .iter()
            .filter_map(|b| b.patterns.as_ref())
            .flatten();
        for pattern in names.filter(|p| !p.negated && !p.glob.contains(['*', '?'])) {
            if !aliases.contains(&pattern.glob) {
                aliases.push(pattern.glob.clone());
            }
        }
        aliases
    }

    /// Resolves `alias` (what the user typed after `ssh`) the way OpenSSH would for the supported options.
    /// Only values the alias actually uses are validated.
    pub fn resolve(&self, alias: &str) -> Result<ResolvedHost, ConfigError> {
        let applicable = self
            .blocks
            .iter()
            .filter(|b| b.applies_to(alias))
            .flat_map(|b| b.options.iter());
        let (mut host_name, mut port, mut user, mut proxy_jump) = (None, None, None, None);
        let mut identity_raw: Vec<&Setting> = Vec::new();
        let mut unsupported = self.global_unsupported.clone();
        for setting in applicable {
            match setting.keyword.as_str() {
                "hostname" => host_name = host_name.or(Some(setting)),
                "port" => port = port.or(Some(setting)),
                "user" => user = user.or(Some(setting)),
                "proxyjump" => proxy_jump = proxy_jump.or(Some(setting)),
                "identityfile" => identity_raw.push(setting),
                other => unsupported.push(Directive {
                    keyword: display_keyword(other),
                    value: setting.value.clone(),
                    line: setting.line,
                }),
            }
        }
        unsupported.sort_by_key(|d| d.line);

        let bad = |setting: &Setting, message: String| ConfigError::SshConfig {
            line: setting.line,
            message,
        };
        let value_of = |setting: &Setting| -> Result<String, ConfigError> {
            if setting.value.is_empty() {
                Err(bad(setting, format!("{} needs a value", setting.keyword)))
            } else {
                Ok(setting.value.clone())
            }
        };
        let port_value = match port {
            Some(setting) => setting
                .value
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| bad(setting, format!("invalid Port '{}'", setting.value)))?,
            None => 22,
        };
        let user_value = user.map(value_of).transpose()?;
        let proxy_jump_value = match proxy_jump {
            Some(setting) => parse_proxy_jump(&setting.value).map_err(|m| bad(setting, m))?,
            None => Vec::new(),
        };

        let tokens = Tokens {
            home: paths::home_dir(),
            host: alias.to_owned(),
            local_user: std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_default(),
            user: user_value.clone(),
            port: port_value,
        };
        let host_name = match host_name {
            Some(setting) => tokens.expand(&value_of(setting)?),
            None => alias.to_owned(),
        };
        let tokens = Tokens {
            host: host_name.clone(),
            ..tokens
        };
        let identity_files = identity_raw
            .into_iter()
            .filter(|s| !s.value.is_empty() && !s.value.eq_ignore_ascii_case("none"))
            .map(|s| paths::expand_tilde_with(&tokens.expand(&s.value), tokens.home.as_deref()))
            .collect();
        Ok(ResolvedHost {
            alias: alias.to_owned(),
            host_name,
            port: port_value,
            user: user_value,
            identity_files,
            proxy_jump: proxy_jump_value,
            unsupported,
        })
    }
}

impl SshConfig {
    /// [`resolve`](Self::resolve) plus the `ProxyJump` chain in front of it. Each hop is looked up in the same
    /// config as an alias (HostName, Port, User, IdentityFile); a `user@` or `:port` written in the
    /// `ProxyJump` value overrides the config, like `ssh -J`. Like `ssh -J`, only the **first** hop of a
    /// list is reached through its own `ProxyJump` (nested chains are flattened in front of it); later hops
    /// are reached through the previous hop and their own `ProxyJump` is ignored. `ProxyJump none` ends a
    /// chain. Refused with [`ConfigError::Invalid`]: a loop (including two aliases for one `host:port`), and
    /// more than [`MAX_JUMPS`] jumps in total.
    pub fn resolve_chain(&self, alias: &str) -> Result<ResolvedChain, ConfigError> {
        let mut target = self.resolve(alias)?;
        let mut path = vec![alias.to_owned()];
        let mut jumps = self.jumps_in_front_of(&target, &mut path)?;
        target.proxy_jump.clear();
        let mut seen: Vec<(String, u16)> = Vec::new();
        for host in jumps.iter().chain([&target]) {
            let key = (host.host_name.to_ascii_lowercase(), host.port);
            if seen.contains(&key) {
                return Err(ConfigError::Invalid(format!(
                    "ProxyJump loop: {}:{} appears twice on the way to '{alias}'",
                    host.host_name, host.port
                )));
            }
            seen.push(key);
        }
        for jump in &mut jumps {
            jump.proxy_jump.clear();
        }
        Ok(ResolvedChain { jumps, target })
    }

    /// `path` holds the aliases whose own `ProxyJump` is being expanded (loop guard).
    fn jumps_in_front_of(
        &self,
        host: &ResolvedHost,
        path: &mut Vec<String>,
    ) -> Result<Vec<ResolvedHost>, ConfigError> {
        let root = path.first().cloned().unwrap_or_default();
        let too_many = || {
            ConfigError::Invalid(format!(
                "ProxyJump chain of '{root}' has more than {MAX_JUMPS} jump hosts"
            ))
        };
        let mut chain: Vec<ResolvedHost> = Vec::new();
        for (index, hop) in host.proxy_jump.iter().enumerate() {
            if path.contains(&hop.host) {
                return Err(ConfigError::Invalid(format!(
                    "ProxyJump loop: {} -> {}",
                    path.join(" -> "),
                    hop.host
                )));
            }
            let mut resolved = self.resolve(&hop.host)?;
            if let Some(user) = &hop.user {
                resolved.user = Some(user.clone());
            }
            if let Some(port) = hop.port {
                resolved.port = port;
            }
            if index == 0 {
                path.push(hop.host.clone());
                if path.len() > MAX_JUMPS + 1 {
                    return Err(too_many());
                }
                chain.extend(self.jumps_in_front_of(&resolved, path)?);
                path.pop();
            }
            chain.push(resolved);
            if chain.len() > MAX_JUMPS {
                return Err(too_many());
            }
        }
        Ok(chain)
    }
}

fn push(blocks: &mut [Block], keyword: String, args: &[String], line: usize) {
    if let Some(block) = blocks.last_mut() {
        block.options.push(Setting {
            keyword,
            value: args.join(" "),
            line,
        });
    }
}

fn display_keyword(lower: &str) -> String {
    match lower {
        "proxycommand" => "ProxyCommand",
        "hostkeyalias" => "HostKeyAlias",
        "userknownhostsfile" => "UserKnownHostsFile",
        "globalknownhostsfile" => "GlobalKnownHostsFile",
        "identityagent" => "IdentityAgent",
        other => other,
    }
    .to_owned()
}

struct Tokens {
    home: Option<PathBuf>,
    host: String,
    local_user: String,
    user: Option<String>,
    port: u16,
}

impl Tokens {
    /// `%d` home, `%h` host, `%r` remote user, `%p` port, `%u` local user, `%%` percent. Unknown tokens stay.
    fn expand(&self, value: &str) -> String {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('%') => out.push('%'),
                Some('d') => out.push_str(
                    &self
                        .home
                        .as_deref()
                        .map(|h| h.display().to_string())
                        .unwrap_or_default(),
                ),
                Some('h') => out.push_str(&self.host),
                Some('r') => out.push_str(self.user.as_deref().unwrap_or(&self.local_user)),
                Some('p') => out.push_str(&self.port.to_string()),
                Some('u') => out.push_str(&self.local_user),
                Some(other) => {
                    out.push('%');
                    out.push(other);
                }
                None => out.push('%'),
            }
        }
        out
    }
}

/// `[user@]host[:port]` hops separated by commas; `none` means no jump. Accepts `ssh://` prefixes and
/// `[v6]:port` brackets.
pub fn parse_proxy_jump(value: &str) -> Result<Vec<JumpHost>, String> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|hop| parse_jump_hop(hop.trim()))
        .collect()
}

fn parse_jump_hop(hop: &str) -> Result<JumpHost, String> {
    let hop = hop.strip_prefix("ssh://").unwrap_or(hop);
    let (user, rest) = match hop.rsplit_once('@') {
        Some((user, rest)) if !user.is_empty() => (Some(user.to_owned()), rest),
        Some(_) => return Err(format!("invalid ProxyJump hop '{hop}'")),
        None => (None, hop),
    };
    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let (host, after) = inner
            .split_once(']')
            .ok_or_else(|| format!("invalid ProxyJump hop '{hop}'"))?;
        (host, after.strip_prefix(':'))
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    let port = port
        .map(|p| {
            p.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| format!("invalid port in ProxyJump hop '{hop}'"))
        })
        .transpose()?;
    if host.is_empty() {
        return Err(format!("invalid ProxyJump hop '{hop}'"));
    }
    Ok(JumpHost {
        user,
        host: host.to_owned(),
        port,
    })
}

/// Splits one line into its keyword and arguments. The keyword ends at whitespace or `=`; one optional `=`
/// (with any surrounding spaces) separates it from the arguments. Double quotes group, an unquoted `#` at the
/// start of a word starts a comment.
fn split_directive(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    Some((keyword.to_owned(), split_words(rest.trim_start())))
}

fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut started = false;
    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                started = true;
            }
            '#' if !in_quotes && !started => break,
            c if c.is_whitespace() && !in_quotes => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TunnelError;
    use crate::spec::{Auth, TunnelSpec};

    const CONFIG: &str = r#"
# an option before any Host line applies to every host and, being first, wins
Compression yes

Host bastion
    HostName bastion.example.com
    Port 2222
    User deploy
    IdentityFile ~/.ssh/id_bastion
    IdentityFile /keys/second

Host db-*
    HostName=%h.internal.example.com
    ProxyJump jump1@192.0.2.10:2200, jump2.example.com
    User dbadmin

Host !db-secret db-*
    Port 2022

Host *
    Port 9999
    User fallback-user
    IdentityFile "/path with space/key"

Match host foo
    Port 1
Host quoted
    HostName "quoted.example.com"  # trailing comment
"#;

    fn config() -> SshConfig {
        SshConfig::parse(CONFIG).unwrap()
    }

    #[test]
    fn resolves_hostname_port_user_and_identities() {
        let r = config().resolve("bastion").unwrap();
        assert_eq!(r.host_name, "bastion.example.com");
        assert_eq!(r.port, 2222);
        assert_eq!(r.user.as_deref(), Some("deploy"));
        assert_eq!(r.identity_files.len(), 3);
        assert_eq!(r.identity_files[1], PathBuf::from("/keys/second"));
        assert_eq!(r.identity_files[2], PathBuf::from("/path with space/key"));
        assert!(
            !r.identity_files[0].to_string_lossy().starts_with('~') || paths::home_dir().is_none()
        );
    }

    #[test]
    fn first_value_wins_and_wildcards_apply() {
        let r = config().resolve("db-1").unwrap();
        assert_eq!(r.host_name, "db-1.internal.example.com");
        assert_eq!(r.user.as_deref(), Some("dbadmin"));
        assert_eq!(r.port, 2022);
    }

    #[test]
    fn negation_excludes_a_host() {
        let r = config().resolve("db-secret").unwrap();
        assert_eq!(r.port, 9999, "the negated block must not apply");
        assert_eq!(r.user.as_deref(), Some("dbadmin"));
    }

    #[test]
    fn host_matching_is_case_sensitive_like_openssh() {
        let r = config().resolve("DB-1").unwrap();
        assert_eq!(r.host_name, "DB-1", "db-* must not match DB-1");
        assert_eq!(r.port, 9999);
    }

    #[test]
    fn catch_all_block_fills_in_what_is_unset() {
        let r = config().resolve("unrelated.example.com").unwrap();
        assert_eq!(r.user.as_deref(), Some("fallback-user"));
        assert_eq!(r.host_name, "unrelated.example.com");
        assert_eq!(r.port, 9999);
    }

    #[test]
    fn options_before_any_host_line_apply_to_everyone_and_win() {
        let c = SshConfig::parse("User first\nHost a\n  User second\n").unwrap();
        assert_eq!(c.resolve("a").unwrap().user.as_deref(), Some("first"));
    }

    #[test]
    fn proxy_jump_is_parsed_but_not_executed() {
        let r = config().resolve("db-1").unwrap();
        assert_eq!(
            r.proxy_jump,
            vec![
                JumpHost {
                    user: Some("jump1".into()),
                    host: "192.0.2.10".into(),
                    port: Some(2200)
                },
                JumpHost {
                    user: None,
                    host: "jump2.example.com".into(),
                    port: None
                },
            ]
        );
        assert!(config().resolve("bastion").unwrap().proxy_jump.is_empty());
    }

    const CHAIN: &str = r#"
Host target
    HostName db-bastion.internal
    User dbadmin
    IdentityFile /keys/target
    ProxyJump jump-a, ops@jump-b:2200

Host jump-a
    HostName a.example.com
    Port 2201
    User alice
    IdentityFile /keys/a
    ProxyJump edge

Host jump-b
    HostName b.example.com
    User bob-from-config
    IdentityFile /keys/b
    ProxyJump never-used-because-jump-b-is-not-first

Host edge
    HostName edge.example.com
    User eve

Host solo
    ProxyJump none

Host * !edge
    ProxyJump edge
"#;

    fn names(hosts: &[ResolvedHost]) -> Vec<(&str, u16, Option<&str>)> {
        hosts
            .iter()
            .map(|h| (h.host_name.as_str(), h.port, h.user.as_deref()))
            .collect()
    }

    #[test]
    fn chain_resolves_every_hop_through_the_config_and_flattens_nested_jumps() {
        let chain = SshConfig::parse(CHAIN)
            .unwrap()
            .resolve_chain("target")
            .unwrap();
        // jump-a is reached through its own ProxyJump (edge); jump-b is reached through jump-a, so its own
        // ProxyJump is ignored (like `ssh -J`); `ops@` and `:2200` override the config.
        assert_eq!(
            names(&chain.jumps),
            [
                ("edge.example.com", 22, Some("eve")),
                ("a.example.com", 2201, Some("alice")),
                ("b.example.com", 2200, Some("ops")),
            ]
        );
        assert_eq!(
            chain.jumps[1].identity_files,
            vec![PathBuf::from("/keys/a")]
        );
        assert_eq!(
            chain.jumps[2].identity_files,
            vec![PathBuf::from("/keys/b")]
        );
        assert_eq!(chain.target.host_name, "db-bastion.internal");
        assert_eq!(chain.target.user.as_deref(), Some("dbadmin"));
        assert!(
            chain.target.proxy_jump.is_empty()
                && chain.jumps.iter().all(|j| j.proxy_jump.is_empty())
        );

        let spec = TunnelSpec::from_ssh_config(&chain, None, "/tmp/kh".into()).unwrap();
        assert_eq!(spec.jumps.len(), 3);
        assert_eq!(
            (
                spec.jumps[2].host.as_str(),
                spec.jumps[2].port,
                spec.jumps[2].user.as_str()
            ),
            ("b.example.com", 2200, "ops")
        );
        // Default auth: the hop's first IdentityFile, else the agent.
        assert!(matches!(&spec.jumps[0].auth, Auth::Agent { socket: None }));
        assert!(
            matches!(&spec.jumps[1].auth, Auth::KeyFile { path, .. } if path == &PathBuf::from("/keys/a"))
        );
        assert!(
            matches!(&spec.auth, Auth::KeyFile { path, .. } if path == &PathBuf::from("/keys/target"))
        );
        // The caller's auth is for the bastion only.
        let spec =
            TunnelSpec::from_ssh_config(&chain, Some(Auth::KeyboardInteractive), "/tmp/kh".into())
                .unwrap();
        assert!(matches!(spec.auth, Auth::KeyboardInteractive));
        assert!(matches!(&spec.jumps[1].auth, Auth::KeyFile { .. }));
    }

    #[test]
    fn proxy_jump_none_and_hosts_without_one_have_no_jumps() {
        let c = SshConfig::parse(CHAIN).unwrap();
        // `ProxyJump none` in the more specific block beats `Host *`.
        assert!(c.resolve_chain("solo").unwrap().jumps.is_empty());
        // The catch-all gives an unlisted host the edge jump (and, negated, not edge itself).
        let unlisted = c.resolve_chain("other").unwrap();
        assert_eq!(
            names(&unlisted.jumps),
            [("edge.example.com", 22, Some("eve"))]
        );
        assert!(c.resolve_chain("edge").unwrap().jumps.is_empty());
        // Without the negation the catch-all sends edge through itself.
        let looped = SshConfig::parse("Host *\n  ProxyJump edge\n").unwrap();
        assert!(looped
            .resolve_chain("edge")
            .unwrap_err()
            .to_string()
            .contains("loop"));
        let plain = SshConfig::parse(
            "Host x
  HostName x.example.com
",
        )
        .unwrap();
        assert!(plain.resolve_chain("x").unwrap().jumps.is_empty());
    }

    #[test]
    fn chain_loops_are_refused() {
        let loops = SshConfig::parse(
            "Host a
 ProxyJump b
Host b
 ProxyJump c
Host c
 ProxyJump a
",
        )
        .unwrap();
        for alias in ["a", "b", "c"] {
            let err = loops.resolve_chain(alias).unwrap_err().to_string();
            assert!(err.contains("loop"), "{alias}: {err}");
        }
        // A list that names the target itself, or two aliases for one host:port.
        let twice = SshConfig::parse(
            "Host a
 ProxyJump j, a
Host j
 HostName j.example.com
",
        )
        .unwrap();
        assert!(twice
            .resolve_chain("a")
            .unwrap_err()
            .to_string()
            .contains("loop"));
        let same = SshConfig::parse(
            "Host a
 ProxyJump j1, j2
Host j1
 HostName same.example.com
Host j2
 HostName SAME.example.com
",
        )
        .unwrap();
        let err = same.resolve_chain("a").unwrap_err().to_string();
        assert!(
            err.to_lowercase().contains("same.example.com:22") && err.contains("twice"),
            "{err}"
        );
    }

    #[test]
    fn chain_depth_is_limited() {
        let mut text = String::new();
        for i in 0..10 {
            text.push_str(&format!(
                "Host h{i}
  ProxyJump h{}
",
                i + 1
            ));
        }
        text.push_str(
            "Host h10
  HostName end.example.com
",
        );
        let c = SshConfig::parse(&text).unwrap();
        // h0 needs h1..h10 = 10 jumps.
        assert!(c
            .resolve_chain("h0")
            .unwrap_err()
            .to_string()
            .contains("more than 8"));
        // h2 needs h3..h10 = 8 jumps: allowed.
        assert_eq!(c.resolve_chain("h2").unwrap().jumps.len(), 8);
        // A flat list of nine hops is refused too.
        let flat = format!(
            "Host t
  ProxyJump {}
",
            (0..9)
                .map(|i| format!("j{i}"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let err = SshConfig::parse(&flat)
            .unwrap()
            .resolve_chain("t")
            .unwrap_err();
        assert!(err.to_string().contains("more than 8"), "{err}");
    }

    #[test]
    fn a_hop_with_a_proxy_command_is_refused() {
        let c = SshConfig::parse(
            "Host t
  ProxyJump j
Host j
  ProxyCommand nc %h %p
",
        )
        .unwrap();
        let chain = c.resolve_chain("t").unwrap();
        let err = TunnelSpec::from_ssh_config(&chain, None, "/tmp/kh".into()).unwrap_err();
        assert!(
            matches!(
                err,
                TunnelError::Config(ConfigError::ProxyCommandUnsupported)
            ),
            "{err:?}"
        );
    }

    #[test]
    fn match_blocks_are_skipped_and_quotes_and_comments_handled() {
        let c = config();
        assert_ne!(c.resolve("foo").unwrap().port, 1);
        assert_eq!(c.resolve("quoted").unwrap().host_name, "quoted.example.com");
    }

    #[test]
    fn host_aliases_lists_concrete_names_only() {
        assert_eq!(
            config().host_aliases(),
            vec!["bastion".to_string(), "quoted".to_string()]
        );
    }

    #[test]
    fn proxy_jump_none_and_ipv6() {
        assert!(parse_proxy_jump("none").unwrap().is_empty());
        assert_eq!(
            parse_proxy_jump("ssh://me@[2001:db8::1]:22").unwrap(),
            vec![JumpHost {
                user: Some("me".into()),
                host: "2001:db8::1".into(),
                port: Some(22)
            }]
        );
        assert!(parse_proxy_jump("host:notaport").is_err());
        assert!(parse_proxy_jump("@host").is_err());
    }

    #[test]
    fn a_malformed_value_only_matters_to_hosts_that_use_it() {
        let c = SshConfig::parse(
            "Host broken\n  Port banana\nHost good\n  Port 2200\nHost empty\n  HostName\n",
        )
        .unwrap();
        assert_eq!(c.resolve("good").unwrap().port, 2200);
        assert_eq!(c.resolve("unrelated").unwrap().port, 22);
        let err = c.resolve("broken").unwrap_err();
        assert!(
            matches!(err, ConfigError::SshConfig { line: 2, .. }),
            "{err}"
        );
        let err = c.resolve("empty").unwrap_err();
        assert!(
            matches!(err, ConfigError::SshConfig { line: 6, .. }),
            "{err}"
        );
    }

    #[test]
    fn identity_tokens_expand() {
        let c = SshConfig::parse("Host t\n HostName real.example.com\n Port 2200\n User bob\n IdentityFile /k/%h_%r_%p_%%\n")
            .unwrap();
        assert_eq!(
            c.resolve("t").unwrap().identity_files,
            vec![PathBuf::from("/k/real.example.com_bob_2200_%")]
        );
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let c = SshConfig::load(Path::new("/nonexistent-dir-for-test/config")).unwrap();
        assert_eq!(c.resolve("x").unwrap().host_name, "x");
    }

    #[test]
    fn ssh_config_bom() {
        let c = SshConfig::parse("\u{feff}Host bastion\n  Port 2200\n").unwrap();
        assert_eq!(c.resolve("bastion").unwrap().port, 2200);
        assert_eq!(c.host_aliases(), vec!["bastion".to_string()]);
    }

    #[test]
    fn ssh_config_keyword_equals_with_spaces() {
        let c = SshConfig::parse(
            "Host = a\n  Port = 2201\n  User=bob\n  HostName =real.example.com\n  IdentityFile= \"/k/id\"\nHost b\n  Port=2202\n",
        )
        .unwrap();
        let a = c.resolve("a").unwrap();
        assert_eq!(
            (a.port, a.user.as_deref(), a.host_name.as_str()),
            (2201, Some("bob"), "real.example.com")
        );
        assert_eq!(a.identity_files, vec![PathBuf::from("/k/id")]);
        assert_eq!(c.resolve("b").unwrap().port, 2202);
    }

    #[test]
    fn unsupported_directives_are_exposed_and_proxy_command_blocks_spec_building() {
        let c = SshConfig::parse(
            "Include ~/.orbstack/ssh/config\nHost a\n  ProxyCommand ssh -W %h:%p jump\n  HostKeyAlias other\n  UserKnownHostsFile /x\n  IdentityAgent /sock\nMatch host a\n  Port 1\nHost b\n  Port 3\n",
        )
        .unwrap();
        let a = c.resolve("a").unwrap();
        let keywords: Vec<&str> = a.unsupported.iter().map(|d| d.keyword.as_str()).collect();
        assert_eq!(
            keywords,
            [
                "Include",
                "ProxyCommand",
                "HostKeyAlias",
                "UserKnownHostsFile",
                "IdentityAgent",
                "Match"
            ]
        );
        let chain = |alias: &str| c.resolve_chain(alias).unwrap();
        let err = TunnelSpec::from_ssh_config(&chain("a"), None, "/tmp/kh".into()).unwrap_err();
        assert!(
            matches!(
                err,
                TunnelError::Config(ConfigError::ProxyCommandUnsupported)
            ),
            "{err:?}"
        );
        // Directives that change which host-key record applies are refused as well.
        let aliased = SshConfig::parse("Host k\n  HostName k.example.com\n  HostKeyAlias other\n")
            .unwrap()
            .resolve_chain("k")
            .unwrap();
        let err = TunnelSpec::from_ssh_config(&aliased, None, "/tmp/kh".into()).unwrap_err();
        assert!(
            matches!(
                &err,
                TunnelError::Config(ConfigError::HostKeyDirectiveUnsupported(name)) if name == "HostKeyAlias"
            ),
            "{err:?}"
        );
        // b has only the file-global Include / Match warnings and builds fine.
        let b = c.resolve("b").unwrap();
        assert!(b
            .unsupported
            .iter()
            .all(|d| d.keyword == "Include" || d.keyword == "Match"));
        let spec = TunnelSpec::from_ssh_config(&chain("b"), None, "/tmp/kh".into()).unwrap();
        assert_eq!((spec.host.as_str(), spec.port), ("b", 3));
    }
}
