// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. See `mod.rs` and
// `licenses/README.md` at the root of the repository.

//! Model-name rules of the request: which token limit field, which reasoning effort, whether the
//! thinking toggle applies; and the model-list filters. Ported from dbx `ai.rs`
//! (`is_openai_reasoning_model`, `is_kimi_model`, `is_openai_api_config`) and `ai_effort.rs`
//! (`openai_capability`, `ollama_capability`, `apply_openai_effort`).

use super::Flavor;
use crate::provider::Effort;
use crate::wire::host_of;

pub(super) fn is_openai_reasoning_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    ["gpt-5", "o1", "o3", "o4"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// The request keeps OpenAI semantics: the OpenAI flavor (also through a gateway), or any flavor
/// aimed at `api.openai.com`.
pub(super) fn is_openai_api(flavor: Flavor, base_url: &str) -> bool {
    flavor == Flavor::OpenAi
        || host_of(base_url).is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
}

/// OpenAI's reasoning models take `max_completion_tokens` and refuse `max_tokens`.
pub(super) fn uses_max_completion_tokens(flavor: Flavor, base_url: &str, model: &str) -> bool {
    is_openai_api(flavor, base_url) && is_openai_reasoning_model(model)
}

/// Kimi K2.5+ and K3+ reject the `extra_body.chat_template_kwargs` toggle (it matches
/// `kimi-k(2.[5-9]…|[3-9]…)`).
pub(super) fn is_kimi_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let Some(rest) = model.strip_prefix("kimi-k") else {
        return false;
    };
    match rest.strip_prefix("2.") {
        Some(minor) => minor
            .chars()
            .next()
            .is_some_and(|c| ('5'..='9').contains(&c)),
        None => rest
            .chars()
            .next()
            .is_some_and(|c| ('3'..='9').contains(&c)),
    }
}

/// `family`, or `family` followed by `-`, `:` or `@` (a variant, a tag).
fn matches_family(model: &str, family: &str) -> bool {
    model
        .strip_prefix(family)
        .is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['-', ':', '@']))
}

fn normalized(model: &str) -> String {
    model
        .trim()
        .trim_start_matches("models/")
        .to_ascii_lowercase()
}

/// What a model takes as `reasoning_effort`.
enum Capability {
    /// One of these words.
    Levels(&'static [&'static str]),
    /// On or off only (`high` / `none`).
    Switch,
}

fn openai_capability(model: &str) -> Option<Capability> {
    use Capability::Levels;
    if matches_family(model, "gpt-5-pro") {
        return Some(Levels(&["high"]));
    }
    if matches_family(model, "gpt-5.6") {
        return Some(Levels(&["none", "low", "medium", "high", "xhigh", "max"]));
    }
    if matches_family(model, "gpt-5.1") {
        return Some(Levels(&["none", "low", "medium", "high"]));
    }
    if ["gpt-5", "gpt-5.2", "gpt-5.4"]
        .iter()
        .any(|family| matches_family(model, family))
    {
        return Some(Levels(&["minimal", "low", "medium", "high", "xhigh"]));
    }
    if ["o1", "o3", "o3-mini", "o4-mini"]
        .iter()
        .any(|family| matches_family(model, family))
    {
        return Some(Levels(&["low", "medium", "high"]));
    }
    None
}

fn ollama_capability(model: &str) -> Option<Capability> {
    if matches_family(model, "gpt-oss") {
        return Some(Capability::Levels(&["low", "medium", "high"]));
    }
    if ["deepseek-r1", "qwen3", "qwq", "nemotron", "glm-4.7"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
    {
        return Some(Capability::Switch);
    }
    None
}

/// The `reasoning_effort` to send for `effort`, if the model takes one. dbx sends the effort the
/// user chose and rejects one the model does not list; TableX always carries an effort, so a
/// model that does not list it (or a flavor with no registry: any OpenAI-compatible server)
/// gets no field, which is dbx's "provider default".
pub(super) fn reasoning_effort(
    flavor: Flavor,
    model: &str,
    effort: Effort,
) -> Option<&'static str> {
    let model = normalized(model);
    let capability = match flavor {
        Flavor::OpenAi => openai_capability(&model),
        Flavor::Ollama => ollama_capability(&model),
        Flavor::Compatible => None,
    }?;
    match capability {
        Capability::Levels(levels) => levels.iter().copied().find(|l| *l == effort.as_str()),
        Capability::Switch => Some("high"),
    }
}

/// Model-list filter of dbx `ai_model_filter.rs` for the OpenAI flavor: embedding, image, audio,
/// moderation and similar models are not chat models.
pub(super) fn is_openai_non_assistant_model(model_id: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "babbage-002",
        "chatgpt-image",
        "computer-use",
        "dall-e",
        "davinci-002",
        "gpt-image",
        "omni-moderation",
        "sora",
        "text-embedding",
        "text-moderation",
        "tts",
        "whisper",
    ];
    let id = model_id
        .trim()
        .trim_start_matches("models/")
        .to_ascii_lowercase();
    let id = id.rsplit('/').next().unwrap_or(&id);
    let model = id
        .strip_prefix("ft:")
        .and_then(|model| model.split(':').next())
        .unwrap_or(id);
    PREFIXES.iter().any(|prefix| model.starts_with(prefix))
        || ["-audio", "-realtime", "-transcribe", "-tts"]
            .iter()
            .any(|part| model.contains(part))
}

/// `capabilities` of Ollama's `/api/show`: `Some(has it)`, or `None` when the model lists none
/// (an old server). Ported from dbx `ollama_capability`.
fn ollama_lists(capabilities: &[String], expected: &str) -> Option<bool> {
    if capabilities.is_empty() {
        return None;
    }
    Some(capabilities.iter().any(|c| c == expected))
}

pub(super) fn ollama_completion_capability(capabilities: &[String]) -> Option<bool> {
    ollama_lists(capabilities, "completion")
}

pub(super) fn ollama_tool_capability(capabilities: &[String]) -> Option<bool> {
    ollama_lists(capabilities, "tools")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(list: &[&str]) -> Vec<String> {
        list.iter().map(|c| (*c).to_owned()).collect()
    }

    // dbx `detects_kimi_models_that_skip_extra_body_thinking_toggle`.
    #[test]
    fn kimi_k25_and_later_are_told_from_older_k2_models() {
        for model in ["kimi-k2.7-code", "kimi-k2.6", "kimi-k2.5", "kimi-k3"] {
            assert!(is_kimi_model(model), "{model}");
        }
        for model in [
            "kimi-k2",
            "kimi-k2-thinking",
            "kimi-k2-0711-preview",
            "kimi-k2.4",
            "gpt-5",
        ] {
            assert!(!is_kimi_model(model), "{model}");
        }
    }

    // dbx `uses_max_completion_tokens_for_openai_reasoning_chat_completions`.
    #[test]
    fn openai_reasoning_models_use_max_completion_tokens() {
        let api = "https://api.openai.com/v1";
        assert!(uses_max_completion_tokens(Flavor::OpenAi, api, "gpt-5.5"));
        assert!(uses_max_completion_tokens(Flavor::OpenAi, api, "O3-mini"));
        assert!(!uses_max_completion_tokens(Flavor::OpenAi, api, "gpt-4o"));
        // Through a gateway the OpenAI flavor keeps OpenAI semantics.
        assert!(uses_max_completion_tokens(
            Flavor::OpenAi,
            "http://localhost:11434/v1",
            "gpt-5-proxy"
        ));
        // A compatible server is not OpenAI, unless it is api.openai.com.
        assert!(!uses_max_completion_tokens(
            Flavor::Compatible,
            "http://localhost:11434/v1",
            "gpt-5-local"
        ));
        assert!(uses_max_completion_tokens(Flavor::Compatible, api, "gpt-5"));
    }

    // dbx `ai_effort.rs` registry and `openai_responses_and_chat_use_different_fields` (chat part),
    // `ollama_openai_compatibility_uses_reasoning_effort`, `..._boolean_effort_maps_...`.
    #[test]
    fn the_effort_is_sent_only_when_the_model_lists_it() {
        let e = reasoning_effort;
        assert_eq!(e(Flavor::OpenAi, "gpt-5.6", Effort::High), Some("high"));
        assert_eq!(e(Flavor::OpenAi, "gpt-5.6", Effort::Max), Some("max"));
        assert_eq!(e(Flavor::OpenAi, "gpt-5.2", Effort::Max), None);
        assert_eq!(e(Flavor::OpenAi, "gpt-5-pro", Effort::Low), None);
        assert_eq!(e(Flavor::OpenAi, "gpt-5-pro", Effort::High), Some("high"));
        assert_eq!(e(Flavor::OpenAi, "o4-mini", Effort::Medium), Some("medium"));
        assert_eq!(e(Flavor::OpenAi, "gpt-4o", Effort::Medium), None);
        assert_eq!(
            e(Flavor::Ollama, "gpt-oss:20b", Effort::Medium),
            Some("medium")
        );
        assert_eq!(e(Flavor::Ollama, "gpt-oss", Effort::XHigh), None);
        assert_eq!(e(Flavor::Ollama, "qwen3:0.6b", Effort::Low), Some("high"));
        assert_eq!(e(Flavor::Ollama, "llama3.2", Effort::Medium), None);
        assert_eq!(e(Flavor::Compatible, "gpt-5", Effort::High), None);
    }

    // dbx `openai_filter_keeps_assistant_models_and_hides_specialized_endpoints`.
    #[test]
    fn the_openai_list_hides_models_that_do_not_chat() {
        for model in ["gpt-4o", "gpt-5.6", "o4-mini", "ft:gpt-4o-mini:org:name:id"] {
            assert!(!is_openai_non_assistant_model(model), "{model}");
        }
        for model in [
            "text-embedding-3-large",
            "gpt-image-1",
            "sora-2",
            "omni-moderation-latest",
            "gpt-4o-mini-transcribe",
            "gpt-4o-realtime-preview",
            "tts-1-hd",
        ] {
            assert!(is_openai_non_assistant_model(model), "{model}");
        }
    }

    // dbx `ollama_capabilities_only_exclude_explicit_non_completion_models`,
    // `ollama_tool_capability_uses_explicit_model_metadata`.
    #[test]
    fn ollama_capabilities_only_exclude_what_is_listed_as_missing() {
        assert_eq!(
            ollama_completion_capability(&caps(&["completion", "vision"])),
            Some(true)
        );
        assert_eq!(
            ollama_completion_capability(&caps(&["embedding"])),
            Some(false)
        );
        assert_eq!(ollama_completion_capability(&[]), None);
        assert_eq!(
            ollama_tool_capability(&caps(&["completion", "tools", "thinking"])),
            Some(true)
        );
        assert_eq!(
            ollama_tool_capability(&caps(&["completion", "thinking"])),
            Some(false)
        );
        assert_eq!(ollama_tool_capability(&[]), None);
    }
}
