//! Model-name rules of the request: which token-limit field a model takes and how thinking is
//! asked for or switched off.
//!
//! OpenAI's reasoning models refuse `max_tokens` (they take `max_completion_tokens`) and take
//! `reasoning_effort`, with a different set of levels per model family. Ollama maps
//! `reasoning_effort` onto its own `think` setting; models that only switch thinking on or off
//! take any level as "on". Other servers get nothing but the `chat_template_kwargs` switch for
//! thinking off, which vLLM-style servers read.

use super::{Flavor, OpenAiConfig};
use crate::provider::Effort;
use crate::wire::host_of;

/// What a request carries for thinking: at most one field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Thinking {
    Nothing,
    /// `reasoning_effort: "<word>"`.
    Effort(&'static str),
    /// `extra_body.chat_template_kwargs.enable_thinking = false`.
    TemplateOff,
}

/// OpenAI's own API: the OpenAi flavor (whatever host it is pointed at, a gateway included), or any
/// flavor aimed at `api.openai.com`.
pub(super) fn is_openai(config: &OpenAiConfig) -> bool {
    config.flavor == Flavor::OpenAi
        || host_of(&config.base_url).is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
}

fn is_reasoning_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    ["gpt-5", "o1", "o3", "o4"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
}

/// Whether the token limit goes out as `max_completion_tokens` (else `max_tokens`).
pub(super) fn uses_completion_tokens(config: &OpenAiConfig, model: &str) -> bool {
    is_openai(config) && is_reasoning_model(model)
}

/// Lowercase, without surrounding space and a leading `models/`.
fn normalized(model: &str) -> String {
    let model = model.trim();
    model
        .strip_prefix("models/")
        .unwrap_or(model)
        .to_ascii_lowercase()
}

/// `model` is `family`, or continues it after `-`, `:` or `@` (`gpt-5.2` is not `gpt-5`).
fn in_family(model: &str, family: &str) -> bool {
    model.strip_prefix(family).is_some_and(|rest| {
        rest.is_empty() || rest.starts_with(['-', ':', '@'])
    })
}

/// The levels each OpenAI family lists, first match wins. Levels outside [`Effort`] (`none`,
/// `minimal`) are never sent, they only document the family.
const OPENAI_LEVELS: &[(&str, &[&str])] = &[
    ("gpt-5-pro", &["high"]),
    ("gpt-5.6", &["none", "low", "medium", "high", "xhigh", "max"]),
    ("gpt-5.1", &["none", "low", "medium", "high"]),
    ("gpt-5", &["minimal", "low", "medium", "high", "xhigh"]),
    ("gpt-5.2", &["minimal", "low", "medium", "high", "xhigh"]),
    ("gpt-5.4", &["minimal", "low", "medium", "high", "xhigh"]),
    ("o1", &["low", "medium", "high"]),
    ("o3", &["low", "medium", "high"]),
    ("o3-mini", &["low", "medium", "high"]),
    ("o4-mini", &["low", "medium", "high"]),
];

/// Ollama models that only think or do not (any level means on), matched by prefix.
const OLLAMA_ON_OFF: &[&str] = &["deepseek-r1", "qwen3", "qwq", "nemotron", "glm-4.7"];

fn openai_effort(model: &str, effort: Effort) -> Thinking {
    let model = normalized(model);
    let levels = OPENAI_LEVELS
        .iter()
        .find(|(family, _)| in_family(&model, family))
        .map(|(_, levels)| *levels);
    match levels {
        Some(levels) if levels.contains(&effort.as_str()) => Thinking::Effort(effort.as_str()),
        _ => Thinking::Nothing,
    }
}

fn ollama_effort(model: &str, effort: Effort) -> Thinking {
    let model = normalized(model);
    if in_family(&model, "gpt-oss") {
        return if ["low", "medium", "high"].contains(&effort.as_str()) {
            Thinking::Effort(effort.as_str())
        } else {
            Thinking::Nothing
        };
    }
    if OLLAMA_ON_OFF.iter().any(|prefix| model.starts_with(prefix)) {
        return Thinking::Effort("high");
    }
    Thinking::Nothing
}

/// Kimi K2.5 and later (`kimi-k2.5`, `kimi-k2.7-code`, `kimi-k3`): they think by default and take
/// no switch; K2 and K2.4 and below do take it.
fn is_kimi_k2_5_or_later(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let Some(rest) = model.strip_prefix("kimi-k") else {
        return false;
    };
    match rest.strip_prefix("2.") {
        Some(minor) => minor.starts_with(['5', '6', '7', '8', '9']),
        None => rest.starts_with(['3', '4', '5', '6', '7', '8', '9']),
    }
}

/// The thinking field of a request for `model`.
pub(super) fn thinking(config: &OpenAiConfig, model: &str, effort: Effort) -> Thinking {
    if config.enable_thinking {
        return match config.flavor {
            Flavor::OpenAi => openai_effort(model, effort),
            Flavor::Ollama => ollama_effort(model, effort),
            Flavor::Compatible => Thinking::Nothing,
        };
    }
    if is_openai(config) {
        // `extra_body` is not an OpenAI parameter.
        Thinking::Nothing
    } else if config.flavor == Flavor::Ollama {
        Thinking::Effort("none")
    } else if is_kimi_k2_5_or_later(model) {
        Thinking::Nothing
    } else {
        Thinking::TemplateOff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Effort::{High, Low, Max, Medium, XHigh};

    fn config(flavor: Flavor, base_url: &str, enable_thinking: bool) -> OpenAiConfig {
        OpenAiConfig {
            base_url: base_url.to_owned(),
            enable_thinking,
            ..OpenAiConfig::for_flavor(flavor)
        }
    }

    fn openai(model: &str, effort: Effort) -> Thinking {
        thinking(&config(Flavor::OpenAi, "https://api.openai.com/v1", true), model, effort)
    }

    fn ollama(model: &str, effort: Effort) -> Thinking {
        thinking(&config(Flavor::Ollama, "http://localhost:11434/v1", true), model, effort)
    }

    #[test]
    fn openai_semantics_follow_the_flavor_or_the_host() {
        assert!(is_openai(&config(Flavor::OpenAi, "https://gw.example/v1", true)));
        assert!(is_openai(&config(Flavor::Compatible, "https://API.OpenAI.com/v1", true)));
        assert!(!is_openai(&config(Flavor::Compatible, "http://localhost:8080/v1", true)));
        assert!(!is_openai(&config(Flavor::Ollama, "http://localhost:11434/v1", true)));
    }

    #[test]
    fn reasoning_models_take_max_completion_tokens_on_openai_only() {
        let real = config(Flavor::OpenAi, "https://api.openai.com/v1", true);
        for model in ["gpt-5", "GPT-5.6", " o1-preview", "o3-mini", "o4-mini"] {
            assert!(uses_completion_tokens(&real, model), "{model}");
        }
        for model in ["gpt-4o", "gpt-4.1", "llama3"] {
            assert!(!uses_completion_tokens(&real, model), "{model}");
        }
        let local = config(Flavor::Compatible, "http://localhost:8080/v1", true);
        assert!(!uses_completion_tokens(&local, "gpt-5"));
        let aimed = config(Flavor::Compatible, "https://api.openai.com/v1", true);
        assert!(uses_completion_tokens(&aimed, "gpt-5"));
        let gateway = config(Flavor::OpenAi, "https://gw.example/v1", true);
        assert!(uses_completion_tokens(&gateway, "o3"));
    }

    #[test]
    fn openai_sends_a_level_only_when_the_family_lists_it() {
        assert_eq!(openai("gpt-5.6", High), Thinking::Effort("high"));
        assert_eq!(openai("gpt-5.6", Max), Thinking::Effort("max"));
        assert_eq!(openai("gpt-5.2", Max), Thinking::Nothing);
        assert_eq!(openai("gpt-5.2", XHigh), Thinking::Effort("xhigh"));
        assert_eq!(openai("gpt-5-pro", Low), Thinking::Nothing);
        assert_eq!(openai("gpt-5-pro", High), Thinking::Effort("high"));
        assert_eq!(openai("gpt-5.1", XHigh), Thinking::Nothing);
        assert_eq!(openai("gpt-5.1", Low), Thinking::Effort("low"));
        assert_eq!(openai("o4-mini", Medium), Thinking::Effort("medium"));
        assert_eq!(openai("o3-mini", XHigh), Thinking::Nothing);
        assert_eq!(openai("o1", Low), Thinking::Effort("low"));
    }

    #[test]
    fn openai_models_outside_every_family_get_nothing() {
        for model in ["gpt-4o", "gpt-5.5", "gpt-5.3", "gpt-5x", "o3x", "claude"] {
            assert_eq!(openai(model, High), Thinking::Nothing, "{model}");
        }
    }

    #[test]
    fn a_family_continues_after_a_dash_colon_or_at_and_the_model_is_normalized() {
        assert_eq!(openai("gpt-5-2025-08-07", Low), Thinking::Effort("low"));
        assert_eq!(openai("gpt-5:custom", Low), Thinking::Effort("low"));
        assert_eq!(openai("o3@preview", Low), Thinking::Effort("low"));
        assert_eq!(openai("  models/GPT-5.6  ", High), Thinking::Effort("high"));
        // `gpt-5-pro-...` belongs to the pro family, not to `gpt-5`.
        assert_eq!(openai("gpt-5-pro-2025", Low), Thinking::Nothing);
    }

    #[test]
    fn ollama_gpt_oss_takes_three_levels_and_on_off_models_always_high() {
        assert_eq!(ollama("gpt-oss:20b", Medium), Thinking::Effort("medium"));
        assert_eq!(ollama("gpt-oss", XHigh), Thinking::Nothing);
        assert_eq!(ollama("gpt-oss", Max), Thinking::Nothing);
        for model in ["qwen3:0.6b", "deepseek-r1:7b", "qwq", "nemotron-mini", "glm-4.7-flash", "Qwen3-coder"] {
            assert_eq!(ollama(model, Low), Thinking::Effort("high"), "{model}");
        }
        assert_eq!(ollama("llama3.2", High), Thinking::Nothing);
        // A plain prefix, no boundary, for the on/off models; none for gpt-oss.
        assert_eq!(ollama("gpt-ossy", High), Thinking::Nothing);
    }

    #[test]
    fn a_compatible_server_never_gets_reasoning_effort() {
        let compatible = config(Flavor::Compatible, "http://localhost:8080/v1", true);
        for model in ["gpt-5", "o3", "qwen3", "gpt-oss"] {
            assert_eq!(thinking(&compatible, model, High), Thinking::Nothing, "{model}");
        }
    }

    #[test]
    fn thinking_off_follows_the_flavor() {
        let off = |flavor, url: &str, model: &str| thinking(&config(flavor, url, false), model, High);
        assert_eq!(off(Flavor::OpenAi, "https://api.openai.com/v1", "gpt-5"), Thinking::Nothing);
        assert_eq!(off(Flavor::Compatible, "https://api.openai.com/v1", "gpt-5"), Thinking::Nothing);
        assert_eq!(off(Flavor::Ollama, "http://localhost:11434/v1", "qwen3"), Thinking::Effort("none"));
        assert_eq!(off(Flavor::Compatible, "http://localhost:8080/v1", "qwen3"), Thinking::TemplateOff);
        assert_eq!(off(Flavor::Compatible, "http://localhost:8080/v1", "kimi-k2.5"), Thinking::Nothing);
        assert_eq!(off(Flavor::Compatible, "http://localhost:8080/v1", "kimi-k2-thinking"), Thinking::TemplateOff);
    }

    #[test]
    fn kimi_k2_5_and_later() {
        for yes in ["kimi-k2.5", "kimi-k2.6", "KIMI-K2.7-code", "kimi-k2.9", "kimi-k3", " kimi-k4-preview "] {
            assert!(is_kimi_k2_5_or_later(yes), "{yes}");
        }
        for no in ["kimi-k2", "kimi-k2-thinking", "kimi-k2-0905", "kimi-k2.4", "kimi-k2.0", "kimi-k1", "moonshot", "qwen3"] {
            assert!(!is_kimi_k2_5_or_later(no), "{no}");
        }
    }
}
