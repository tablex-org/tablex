//! The pieces of the AI assistant that are not UI: the HTTP transport on the app's client, the API
//! keys in the OS credential store, and the rules for a provider's base URL. The assistant
//! service that uses them is `app::assistant`; the engine side is the `engine-ai` crate.

pub mod keys;
pub mod transport;

use crate::settings::AiProvider;
use url::{Host, Url};

/// Anthropic's default address.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// The models offered before the provider's list was fetched.
pub const BUILTIN_MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

/// The models of `provider` before its list was fetched (Anthropic's built-in ones; the other
/// providers have only what their server lists) followed by the fetched ones the list does not
/// have yet, in the order the provider gave them.
pub fn merge_models(
    provider: AiProvider,
    fetched: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let builtin: &[&str] = if provider == AiProvider::Anthropic {
        &BUILTIN_MODELS
    } else {
        &[]
    };
    let mut models: Vec<String> = builtin.iter().map(|id| (*id).to_owned()).collect();
    for id in fetched {
        if !models.contains(&id) {
            models.push(id);
        }
    }
    models
}

/// Whether the API key may be sent to `url`: always over `https`, over plain `http` only to this
/// machine (a local gateway or a test server).
pub fn sends_secrets_to(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => is_loopback(url),
        _ => false,
    }
}

/// A plain `http` address on this machine. A proxy is never given such a request: the system
/// proxy settings have no loopback exemption of their own, and the key would reach the proxy in
/// clear text.
pub fn is_plain_loopback(url: &Url) -> bool {
    url.scheme() == "http" && is_loopback(url)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

/// Why `text` cannot be `provider`'s base URL, in words for the Settings page; `None` when it
/// can. The provider's own check decides first (`https://`, or `http://` to this machine only,
/// with a host, no credentials, query or fragment), so Settings never accepts what the provider
/// would refuse.
pub fn base_url_problem(provider: AiProvider, text: &str) -> Option<&'static str> {
    use engine_ai::{AnthropicConfig, ConfigError, OpenAiConfig};
    let base_url = text.trim().to_owned();
    let checked = match provider.flavor() {
        None => AnthropicConfig {
            base_url,
            ..AnthropicConfig::default()
        }
        .validate(),
        Some(flavor) => OpenAiConfig {
            base_url,
            ..OpenAiConfig::for_flavor(flavor)
        }
        .validate(),
    };
    if let Err(error) = checked {
        return Some(match error {
            ConfigError::InvalidBaseUrl => match provider {
                AiProvider::Anthropic => "Enter a full address such as https://api.anthropic.com",
                AiProvider::OpenAi => "Enter a full address such as https://api.openai.com/v1",
                AiProvider::OpenAiCompatible => {
                    "Enter the server's full address, such as http://localhost:1234/v1"
                }
                AiProvider::Ollama => "Enter a full address such as http://localhost:11434/v1",
            },
            ConfigError::BaseUrlHasUserinfo => "Do not put a user name or password in the address",
            ConfigError::BaseUrlHasQueryOrFragment => {
                "The address must not have a query or a fragment"
            }
            ConfigError::InsecureBaseUrl => "Use https:// (http:// only for this machine)",
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetched_models_are_merged_after_the_built_in_ones_without_repeats() {
        let merged = merge_models(
            AiProvider::Anthropic,
            [
                "claude-sonnet-5-5".to_owned(),
                "claude-new-6".to_owned(),
                "claude-new-6".to_owned(),
            ],
        );
        assert_eq!(&merged[..4], BUILTIN_MODELS.map(str::to_owned));
        assert_eq!(merged.len(), 5);
        assert_eq!(merged[4], "claude-new-6");
        assert_eq!(merge_models(AiProvider::Anthropic, []).len(), 4);
        // The others have no built-in list: only what the server said.
        assert!(merge_models(AiProvider::Ollama, []).is_empty());
        assert_eq!(
            merge_models(AiProvider::OpenAi, ["gpt-x".to_owned(), "gpt-x".to_owned()]),
            ["gpt-x"]
        );
    }

    #[test]
    fn base_urls_must_be_https_or_loopback() {
        for good in [
            DEFAULT_BASE_URL,
            "https://gateway.example.com/anthropic",
            "http://localhost:8080",
            "http://127.0.0.1:9000/",
            "http://[::1]:9000",
            "HTTP://LOCALHOST",
        ] {
            for provider in AiProvider::ALL {
                assert_eq!(
                    base_url_problem(provider, good),
                    None,
                    "{provider:?} {good}"
                );
            }
        }
        for bad in [
            "",
            "api.anthropic.com",
            "http://api.anthropic.com",
            "http://10.0.0.5:8080",
            "ftp://localhost",
            "https://user:pw@example.com",
            "https://example.com/?key=1",
            "https://example.com/#x",
            "file:///etc/passwd",
        ] {
            for provider in AiProvider::ALL {
                assert!(
                    base_url_problem(provider, bad).is_some(),
                    "{provider:?} {bad}"
                );
            }
        }
    }

    #[test]
    fn every_default_address_is_usable_and_the_compatible_one_must_be_given() {
        for provider in AiProvider::ALL {
            let default = provider.default_base_url();
            match provider {
                AiProvider::OpenAiCompatible => {
                    assert_eq!(default, "");
                    assert!(base_url_problem(provider, default).is_some());
                }
                _ => assert_eq!(base_url_problem(provider, default), None, "{provider:?}"),
            }
        }
        assert_eq!(
            AiProvider::OpenAi.default_base_url(),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            AiProvider::Ollama.default_base_url(),
            "http://localhost:11434/v1"
        );
    }
}
