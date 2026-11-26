//! Where a Chat Completions server is asked: the chat, model-list and Ollama show URLs derived from
//! the one base URL the user typed. Pure string functions.
//!
//! The base URL is split into its origin (`scheme://authority`, brackets and port of an IPv6
//! host included) and its path; suffixes are only ever looked for in the path, so a host that is
//! named `chat`, `responses` or `models` is never taken for one.

use super::Flavor;

const CHAT_SUFFIX: &str = "/chat/completions";
const RESPONSES_SUFFIX: &str = "/responses";
const MODELS_SUFFIX: &str = "/models";

/// The URLs one provider talks to, derived once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Endpoints {
    pub chat: String,
    pub models: String,
    pub show: String,
}

impl Endpoints {
    pub fn new(base: &str, flavor: Flavor) -> Self {
        let models = models_url(base);
        Self {
            chat: chat_url(base, flavor),
            show: show_url(&models),
            models,
        }
    }

    /// Every derived URL stays on the origin of the base, so the key cannot go to another host.
    pub fn same_origin_as(&self, base: &str) -> bool {
        let origin = split(base).0;
        [&self.chat, &self.models, &self.show]
            .into_iter()
            .all(|url| split(url).0 == origin)
    }
}

/// `(origin, path)` of a URL without its trailing slash. The path is empty or starts with `/`.
fn split(url: &str) -> (&str, &str) {
    let url = url.trim().trim_end_matches('/');
    let authority_start = url.find("://").map_or(0, |at| at + 3);
    match url[authority_start..].find('/') {
        Some(at) => url.split_at(authority_start + at),
        None => (url, ""),
    }
}

/// An empty path becomes `/v1`; any other path is kept.
fn versioned(path: &str) -> &str {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        "/v1"
    } else {
        path
    }
}

/// The path without a pasted `/chat/completions` or `/responses` ending.
fn without_endpoint(path: &str) -> &str {
    path.strip_suffix(CHAT_SUFFIX)
        .or_else(|| path.strip_suffix(RESPONSES_SUFFIX))
        .unwrap_or(path)
}

pub(super) fn chat_url(base: &str, flavor: Flavor) -> String {
    let (origin, path) = split(base);
    if flavor == Flavor::OpenAi {
        let path = versioned(without_endpoint(path));
        format!("{origin}{path}{CHAT_SUFFIX}")
    } else if path.ends_with(CHAT_SUFFIX) {
        format!("{origin}{path}")
    } else {
        format!("{origin}{}{CHAT_SUFFIX}", versioned(path))
    }
}

pub(super) fn models_url(base: &str) -> String {
    let (origin, path) = split(base);
    if path.ends_with(MODELS_SUFFIX) {
        format!("{origin}{path}")
    } else {
        format!(
            "{origin}{}{MODELS_SUFFIX}",
            versioned(without_endpoint(path))
        )
    }
}

/// Ollama's native `POST /api/show`, next to its OpenAI-compatible `/v1`.
pub(super) fn show_url(models_url: &str) -> String {
    let (origin, path) = split(models_url);
    let path = path
        .strip_suffix("/v1/models")
        .or_else(|| path.strip_suffix(MODELS_SUFFIX))
        .unwrap_or(path)
        .trim_end_matches('/');
    format!("{origin}{path}/api/show")
}

#[cfg(test)]
mod tests {
    use super::*;
    use Flavor::{Compatible, Ollama, OpenAi};

    const CHAT: &str = "https://api.openai.com/v1/chat/completions";

    #[test]
    fn openai_chat_url_accepts_the_origin_the_v1_base_and_pasted_endpoints() {
        for base in [
            "https://api.openai.com",
            "https://api.openai.com/",
            "https://api.openai.com/v1",
            "https://api.openai.com/v1/",
            "https://api.openai.com/v1/chat/completions",
            "https://api.openai.com/v1/responses",
            "  https://api.openai.com/v1/responses/  ",
        ] {
            assert_eq!(chat_url(base, OpenAi), CHAT, "{base}");
        }
    }

    #[test]
    fn openai_chat_url_keeps_a_gateway_path() {
        assert_eq!(
            chat_url("https://gw.example/openai/v1/responses", OpenAi),
            "https://gw.example/openai/v1/chat/completions"
        );
        assert_eq!(
            chat_url("https://gw.example/chat/completions", OpenAi),
            "https://gw.example/v1/chat/completions"
        );
    }

    #[test]
    fn other_flavors_use_a_pasted_chat_url_as_it_is() {
        for flavor in [Compatible, Ollama] {
            assert_eq!(
                chat_url("http://localhost:8080/v2/chat/completions", flavor),
                "http://localhost:8080/v2/chat/completions"
            );
        }
    }

    #[test]
    fn other_flavors_add_v1_to_a_bare_origin_and_keep_any_other_path() {
        for flavor in [Compatible, Ollama] {
            let cases = [
                ("http://localhost:8080", "http://localhost:8080/v1/chat/completions"),
                ("http://localhost:8080/", "http://localhost:8080/v1/chat/completions"),
                ("http://localhost:8080/v1", "http://localhost:8080/v1/chat/completions"),
                ("http://localhost:8080/v1/", "http://localhost:8080/v1/chat/completions"),
                ("http://localhost:8080/v2", "http://localhost:8080/v2/chat/completions"),
                ("http://localhost:8080/api", "http://localhost:8080/api/chat/completions"),
            ];
            for (base, want) in cases {
                assert_eq!(chat_url(base, flavor), want, "{base}");
            }
        }
    }

    #[test]
    fn models_url_is_used_as_it_is_or_derived_for_every_flavor() {
        let cases = [
            ("https://api.openai.com", "https://api.openai.com/v1/models"),
            ("https://api.openai.com/v1", "https://api.openai.com/v1/models"),
            ("https://api.openai.com/v1/models", "https://api.openai.com/v1/models"),
            (CHAT, "https://api.openai.com/v1/models"),
            ("https://api.openai.com/v1/responses", "https://api.openai.com/v1/models"),
            ("http://127.0.0.1:1234/v1", "http://127.0.0.1:1234/v1/models"),
            ("https://host.example/api", "https://host.example/api/models"),
            ("https://host.example/api/models/", "https://host.example/api/models"),
        ];
        for (base, want) in cases {
            assert_eq!(models_url(base), want, "{base}");
        }
    }

    #[test]
    fn show_url_sits_next_to_the_v1_path() {
        let cases = [
            ("http://host:11434/v1", "http://host:11434/api/show"),
            ("http://host:11434", "http://host:11434/api/show"),
            ("http://host:11434/v1/chat/completions", "http://host:11434/api/show"),
            ("https://box.example/ollama/v1", "https://box.example/ollama/api/show"),
            ("https://box.example/ollama/models", "https://box.example/ollama/api/show"),
        ];
        for (base, want) in cases {
            assert_eq!(show_url(&models_url(base)), want, "{base}");
        }
    }

    #[test]
    fn a_host_named_like_a_suffix_is_not_a_suffix() {
        let compatible = "https://chat/completions";
        assert_eq!(
            chat_url(compatible, Compatible),
            "https://chat/completions/chat/completions"
        );
        assert_eq!(models_url(compatible), "https://chat/completions/models");

        assert_eq!(
            chat_url("https://responses", OpenAi),
            "https://responses/v1/chat/completions"
        );
        assert_eq!(models_url("https://responses"), "https://responses/v1/models");

        let ollama = Endpoints::new("https://models", Ollama);
        assert_eq!(ollama.chat, "https://models/v1/chat/completions");
        assert_eq!(ollama.models, "https://models/v1/models");
        assert_eq!(ollama.show, "https://models/api/show");
    }

    #[test]
    fn an_ipv6_origin_keeps_its_brackets_and_port() {
        let endpoints = Endpoints::new("http://[::1]:11434/v1", Ollama);
        assert_eq!(endpoints.chat, "http://[::1]:11434/v1/chat/completions");
        assert_eq!(endpoints.models, "http://[::1]:11434/v1/models");
        assert_eq!(endpoints.show, "http://[::1]:11434/api/show");
        assert!(endpoints.same_origin_as("http://[::1]:11434/v1"));
    }

    #[test]
    fn derived_urls_stay_on_the_origin_of_the_base() {
        for (base, flavor) in [
            ("https://api.openai.com", OpenAi),
            ("https://a.example/x/y/", Compatible),
            ("http://localhost:11434/v1", Ollama),
            ("https://models", Ollama),
        ] {
            assert!(Endpoints::new(base, flavor).same_origin_as(base), "{base}");
        }
        assert!(!Endpoints::new("https://a.example", Compatible).same_origin_as("https://b.example"));
    }
}
