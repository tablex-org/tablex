//! `*` / `?` wildcard matching as used by `known_hosts` and `ssh_config` host patterns.

/// Case-insensitive match (`known_hosts` host names are lowercased).
pub(crate) fn wildcard_match(pattern: &str, text: &str) -> bool {
    matches(pattern, text, true)
}

/// Case-sensitive match (OpenSSH matches `Host` aliases in `ssh_config` case-sensitively).
pub(crate) fn wildcard_match_exact(pattern: &str, text: &str) -> bool {
    matches(pattern, text, false)
}

fn matches(pattern: &str, text: &str, ignore_case: bool) -> bool {
    let fold = |s: &str| -> Vec<char> {
        if ignore_case {
            s.chars().flat_map(char::to_lowercase).collect()
        } else {
            s.chars().collect()
        }
    };
    let (pattern, text) = (fold(pattern), fold(text));
    let (mut p, mut t) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                backtrack = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '?' || c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match backtrack {
                Some((star_p, star_t)) => {
                    backtrack = Some((star_p, star_t + 1));
                    p = star_p + 1;
                    t = star_t + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::{wildcard_match, wildcard_match_exact};

    #[test]
    fn matches_like_openssh_patterns() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*.example.com", "db.example.com"));
        assert!(!wildcard_match("*.example.com", "example.com"));
        assert!(wildcard_match("bastion-?", "bastion-1"));
        assert!(!wildcard_match("bastion-?", "bastion-12"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
        assert!(wildcard_match("HOST", "host"));
        assert!(wildcard_match("", ""));
        assert!(!wildcard_match("", "x"));
    }

    #[test]
    fn exact_variant_is_case_sensitive() {
        assert!(wildcard_match_exact("Prod-*", "Prod-1"));
        assert!(!wildcard_match_exact("Prod-*", "prod-1"));
    }
}
