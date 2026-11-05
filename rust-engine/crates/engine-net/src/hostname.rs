//! Strict host validation. A host name ends up in `known_hosts` and in SSH requests, so anything that is not
//! a plain RFC 1123 name or an IP literal is refused (no wildcards, brackets, commas, whitespace, ...).

use std::net::IpAddr;

/// A validated host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    /// Lowercased DNS name.
    Name(String),
    Ip(IpAddr),
}

impl Host {
    /// `host` for port 22, `[host]:port` otherwise: the string OpenSSH matches and hashes. IP literals use the
    /// canonical std form.
    pub(crate) fn known_hosts_name(&self, port: u16) -> String {
        let host = match self {
            Self::Name(name) => name.clone(),
            Self::Ip(ip) => ip.to_string(),
        };
        if port == 22 {
            host
        } else {
            format!("[{host}]:{port}")
        }
    }
}

/// `allow_underscore` is for forward targets (container / service names such as `my_db`); the SSH host that is
/// written to `known_hosts` is always strict.
pub(crate) fn parse_host(host: &str, allow_underscore: bool) -> Result<Host, String> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Host::Ip(ip));
    }
    let bad = |why: &str| {
        Err(format!(
            "'{}' is not a valid host name: {why}",
            host.escape_debug()
        ))
    };
    if host.is_empty() || host.len() > 253 {
        return bad("empty or longer than 253 characters");
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return bad("empty label or label longer than 63 characters");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return bad("a label starts or ends with '-'");
        }
        let valid =
            |c: char| c.is_ascii_alphanumeric() || c == '-' || (allow_underscore && c == '_');
        if !label.chars().all(valid) {
            return bad("only letters, digits, '-' and '.' are allowed");
        }
    }
    Ok(Host::Name(host.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_names_and_ip_literals() {
        assert_eq!(
            parse_host("Bastion.Example.com", false),
            Ok(Host::Name("bastion.example.com".into()))
        );
        assert!(parse_host("localhost", false).is_ok());
        assert!(parse_host("127.0.0.1", false).is_ok());
        assert!(parse_host("2001:db8::1", false).is_ok());
        assert!(parse_host("a-b.c1", false).is_ok());
    }

    #[test]
    fn rejects_everything_else() {
        for bad in [
            "",
            "*",
            "?",
            "!x",
            "a,b",
            "[::1]",
            "a|b",
            "#x",
            "@x",
            "a b",
            " a",
            "a\n",
            "a\0",
            "-x.com",
            "x-.com",
            "x..com",
            ".x.com",
            "x.com.",
            "a_b.com",
            "a/b",
            "a:22",
            "*,x.attacker.tld",
            "ex\u{e4}mple.com",
            "fe80::1%en0",
        ] {
            assert!(parse_host(bad, false).is_err(), "{bad:?} must be rejected");
        }
        assert!(parse_host(&"a".repeat(64), false).is_err());
        assert!(parse_host(&format!("{}.com", "a.".repeat(130)), false).is_err());
    }

    #[test]
    fn underscore_only_when_allowed() {
        assert!(parse_host("my_db", true).is_ok());
        assert!(parse_host("my_db", false).is_err());
    }

    #[test]
    fn known_hosts_names() {
        let name = parse_host("Bastion.example.com", false).unwrap();
        assert_eq!(name.known_hosts_name(22), "bastion.example.com");
        assert_eq!(name.known_hosts_name(2222), "[bastion.example.com]:2222");
        let v6 = parse_host("2001:0DB8:0:0:0:0:0:1", false).unwrap();
        assert_eq!(v6.known_hosts_name(22), "2001:db8::1");
        assert_eq!(v6.known_hosts_name(2222), "[2001:db8::1]:2222");
    }
}
