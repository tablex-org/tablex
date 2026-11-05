// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. See `mod.rs` and
// `licenses/README.md` at the root of the repository.

//! Where the requests go: the chat, model-list and Ollama `/api/show` addresses derived from the
//! one base URL the user gave.

use super::Flavor;

const CHAT_SUFFIX: &str = "/chat/completions";

/// `scheme://authority` and the path (empty, or starting with `/`) of an address. Suffixes are
/// looked for in the path only: `https://chat/completions` is the host `chat`.
pub(super) fn split_origin(url: &str) -> (&str, &str) {
    let after = url.find("://").map_or(0, |at| at + 3);
    let end = url[after..].find('/').map_or(url.len(), |at| after + at);
    url.split_at(end)
}

/// `scheme://authority` of an address.
pub(super) fn origin(url: &str) -> &str {
    split_origin(url).0
}

/// Adds `/v1` to a bare origin (`https://api.example.com`), the usual omission; an address that
/// has a path is left alone (the user knows what they are doing). Ported from dbx
/// `ensure_openai_version_prefix`.
fn with_version_prefix(origin: &str, path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        format!("{origin}/v1")
    } else {
        format!("{origin}{path}")
    }
}

/// The chat completions address. A pasted `…/chat/completions` is accepted; for OpenAI itself a
/// pasted `…/responses` is too (its base is used). Ported from dbx `resolve_endpoint`.
pub(super) fn chat_url(flavor: Flavor, base: &str) -> String {
    let (origin, path) = split_origin(base.trim().trim_end_matches('/'));
    if flavor == Flavor::OpenAi {
        let path = path
            .strip_suffix(CHAT_SUFFIX)
            .or_else(|| path.strip_suffix("/responses"))
            .unwrap_or(path);
        return format!("{}{CHAT_SUFFIX}", with_version_prefix(origin, path));
    }
    if path.ends_with(CHAT_SUFFIX) {
        return format!("{origin}{path}");
    }
    format!("{}{CHAT_SUFFIX}", with_version_prefix(origin, path))
}

/// The model list address. Ported from dbx `resolve_model_list_endpoint`.
pub(super) fn models_url(base: &str) -> String {
    let (origin, path) = split_origin(base.trim().trim_end_matches('/'));
    if path.ends_with("/models") {
        return format!("{origin}{path}");
    }
    let path = path
        .strip_suffix(CHAT_SUFFIX)
        .or_else(|| path.strip_suffix("/responses"))
        .unwrap_or(path);
    format!("{}/models", with_version_prefix(origin, path))
}

/// Ollama's own `/api/show`, next to its OpenAI-compatible `/v1`. Ported from dbx
/// `resolve_ollama_show_endpoint`; the base URL has no query or fragment (see `validate`).
pub(super) fn show_url(models_url: &str) -> String {
    let (origin, path) = split_origin(models_url);
    let path = path.trim_end_matches('/');
    let base = path
        .strip_suffix("/v1/models")
        .or_else(|| path.strip_suffix("/models"))
        .unwrap_or(path)
        .trim_end_matches('/');
    format!("{origin}{base}/api/show")
}

#[cfg(test)]
mod tests {
    use super::*;

    // dbx `auto_adds_v1_to_openai_compatible_endpoints`, `official_openai_endpoint_tracks_api_style`
    // (chat part), `resolves_model_list_endpoints_from_base_and_completion_urls` and
    // `resolves_ollama_native_show_endpoint_from_openai_compatibility_url`.
    #[test]
    fn a_bare_origin_gets_v1_and_a_path_is_left_alone() {
        use Flavor::*;
        let cases = [
            (
                Compatible,
                "https://api.example.com",
                "https://api.example.com/v1/chat/completions",
            ),
            (
                Compatible,
                "https://api.example.com/v1",
                "https://api.example.com/v1/chat/completions",
            ),
            (
                Compatible,
                "https://api.example.com/v1/",
                "https://api.example.com/v1/chat/completions",
            ),
            (
                Compatible,
                "https://api.example.com/v2",
                "https://api.example.com/v2/chat/completions",
            ),
            (
                Compatible,
                "https://api.openai.com/v1/chat/completions",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                Compatible,
                "https://my-gateway.com/api",
                "https://my-gateway.com/api/chat/completions",
            ),
            (
                Compatible,
                "http://localhost:8080",
                "http://localhost:8080/v1/chat/completions",
            ),
            (
                Ollama,
                "http://localhost:11434/v1",
                "http://localhost:11434/v1/chat/completions",
            ),
            (
                OpenAi,
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                OpenAi,
                "https://api.openai.com",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                OpenAi,
                "https://api.openai.com/v1/chat/completions",
                "https://api.openai.com/v1/chat/completions",
            ),
            (
                OpenAi,
                "https://api.openai.com/v1/responses",
                "https://api.openai.com/v1/chat/completions",
            ),
        ];
        for (flavor, base, expected) in cases {
            assert_eq!(chat_url(flavor, base), expected, "{flavor:?} {base}");
        }
    }

    #[test]
    fn the_model_list_address_follows_the_base_or_a_pasted_completion_url() {
        let cases = [
            (
                "https://api.openai.com/v1/chat/completions",
                "https://api.openai.com/v1/models",
            ),
            (
                "https://api.example.com",
                "https://api.example.com/v1/models",
            ),
            (
                "https://api.example.com/v1",
                "https://api.example.com/v1/models",
            ),
            (
                "https://my-gateway.com/api",
                "https://my-gateway.com/api/models",
            ),
            (
                "https://my-gateway.com/api/models",
                "https://my-gateway.com/api/models",
            ),
        ];
        for (base, expected) in cases {
            assert_eq!(models_url(base), expected, "{base}");
        }
    }

    #[test]
    fn the_ollama_show_address_sits_beside_the_compatible_api() {
        let show = |base: &str| show_url(&models_url(base));
        assert_eq!(
            show("http://localhost:11434/v1/chat/completions"),
            "http://localhost:11434/api/show"
        );
        assert_eq!(
            show("http://localhost:11434/v1"),
            "http://localhost:11434/api/show"
        );
        assert_eq!(
            show("https://example.com/ollama/v1"),
            "https://example.com/ollama/api/show"
        );
    }

    #[test]
    fn a_suffix_is_looked_for_in_the_path_not_in_the_scheme_or_host() {
        // These pass `validate()`: the host is `chat`, `responses`, `models`.
        assert_eq!(
            chat_url(Flavor::Compatible, "https://chat/completions"),
            "https://chat/completions/chat/completions"
        );
        assert_eq!(
            chat_url(Flavor::OpenAi, "https://responses"),
            "https://responses/v1/chat/completions"
        );
        assert_eq!(models_url("https://models"), "https://models/v1/models");
        assert_eq!(
            show_url(&models_url("https://models")),
            "https://models/api/show"
        );
        assert_eq!(origin("https://chat/completions"), "https://chat");
        assert_eq!(origin("http://[::1]:8080/v1"), "http://[::1]:8080");
        assert_eq!(origin("https://host"), "https://host");
    }
}
