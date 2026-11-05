// Adapted from dbx (https://github.com/t8y2/dbx), Apache-2.0; changed. Ports dbx tests and
// samples. See `mod.rs` and `licenses/README.md` at the root of the repository.

//! The OpenAI-compatible provider on the scripted transport: request shapes, streams (the dbx
//! samples included), errors, retries, cancel, keys, flavors, model lists. No network.

use super::stream::ChatAssembler;
use super::*;
use crate::conversation::{HistoryForm, Message, UserBlock};
use crate::provider::{ContextBlock, Effort, StopReason, SystemPrompt, Usage};
use crate::test_support::*;
use crate::transport::TransportError;
use crate::wire::sse::SseParser;
use crate::wire::TurnLimits;
use engine_agent::ToolSpec;
use serde_json::{json, Value};
use std::sync::mpsc;
use std::time::Instant;

// ------------------------------------------------------------------------------------- helpers

struct NoKey;

impl KeySource for NoKey {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        Err(KeyError::NotSet)
    }
}

struct EmptyKey;

impl KeySource for EmptyKey {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        Ok(ApiKey::new("  "))
    }
}

struct Rig {
    provider: OpenAiProvider,
    transport: Arc<ScriptedTransport>,
    sleeper: Arc<ScriptedSleeper>,
}

fn config(flavor: Flavor, base_url: &str) -> OpenAiConfig {
    OpenAiConfig {
        base_url: base_url.to_owned(),
        ..OpenAiConfig::for_flavor(flavor)
    }
}

fn rig_with(config: OpenAiConfig, keys: Arc<dyn KeySource>, steps: Vec<Step>) -> Rig {
    let transport = ScriptedTransport::new(steps);
    let sleeper = ScriptedSleeper::new();
    let provider = OpenAiProvider::new(
        Arc::clone(&transport) as Arc<dyn HttpTransport>,
        keys,
        config,
    )
    .expect("a valid test configuration")
    .with_sleeper(Arc::clone(&sleeper) as Arc<dyn Sleeper>)
    .with_finish_grace(Duration::from_millis(60));
    Rig {
        provider,
        transport,
        sleeper,
    }
}

fn rig(flavor: Flavor, steps: Vec<Step>) -> Rig {
    let keys: Arc<dyn KeySource> = if flavor.requires_key() {
        Arc::new(FixedKey)
    } else {
        Arc::new(NoKey)
    };
    let base = flavor
        .default_base_url()
        .unwrap_or("http://localhost:1234/v1");
    rig_with(config(flavor, base), keys, steps)
}

fn spec() -> ToolSpec {
    ToolSpec {
        name: "list_tables",
        description: "Lists tables.",
        input_schema: json!({"type": "object", "properties": {"connection_id": {"type": "string"}}}),
        output_schema: json!({}),
        example: json!({}),
    }
}

fn request<'a>(model: &'a str, messages: &'a [Message], tools: &'a [ToolSpec]) -> TurnRequest<'a> {
    TurnRequest {
        model,
        effort: Effort::Medium,
        system: SystemPrompt {
            core: "CORE",
            context: Some(ContextBlock {
                text: "CTX",
                cacheable: true,
            }),
        },
        tools,
        messages,
        history: HistoryForm::default(),
    }
}

fn hello() -> Vec<Message> {
    vec![Message::user_text("hi")]
}

/// One `chat.completion.chunk` event.
fn chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "model": "gpt-test",
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

fn text_chunk(text: &str) -> Value {
    chunk(json!({"content": text}), None)
}

fn usage_chunk(prompt: u64, completion: u64, cached: Option<u64>) -> Value {
    let mut usage = json!({"prompt_tokens": prompt, "completion_tokens": completion});
    if let Some(cached) = cached {
        usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
    }
    json!({"id": "chatcmpl-1", "model": "gpt-test", "choices": [], "usage": usage})
}

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect()
}

const DONE: &str = "data: [DONE]\n\n";

fn complete(events: &[Value]) -> Reply {
    Reply::ok(format!("{}{DONE}", sse(events)))
}

type Turn = (Result<TurnResponse, ProviderError>, Vec<String>);

fn run(rig: &Rig, request: &TurnRequest<'_>) -> Turn {
    let mut events = Vec::new();
    let result = rig.provider.stream_turn(
        request,
        &mut |event| events.push(format!("{event:?}")),
        &CancelToken::new(),
    );
    (result, events)
}

fn turn(rig: &Rig, model: &str) -> Turn {
    let messages = hello();
    run(rig, &request(model, &messages, &[]))
}

fn ok(turn: Turn) -> (TurnResponse, Vec<String>) {
    (turn.0.expect("the turn completes"), turn.1)
}

fn err(turn: Turn) -> ProviderError {
    turn.0.expect_err("the turn fails")
}

trait Text {
    fn text(&self) -> String;
}

impl Text for TurnResponse {
    /// The text blocks, joined.
    fn text(&self) -> String {
        Message::Assistant {
            content: self.content.clone(),
        }
        .assistant_text()
    }
}

fn assert_no_key(text: &str) {
    assert!(
        !text.contains(FAKE_KEY) && !text.contains("ZZSECRETZZ"),
        "the API key leaked into: {text}"
    );
}

// -------------------------------------------------------------------------------------- request

fn history() -> Vec<Message> {
    vec![
        Message::user_text("How many users?"),
        Message::System {
            content: "Writes are off.".into(),
        },
        Message::Assistant {
            content: vec![
                json!({"type": "thinking", "thinking": "hmm", "signature": "s"}),
                json!({"type": "redacted_thinking", "data": "x"}),
                json!({"type": "reasoning", "text": "shown only"}),
                json!({"type": "text", "text": "Let me look."}),
                json!({"type": "tool_use", "id": "call_1", "name": "list_tables",
                       "input": {"connection_id": "c1"}}),
                json!({"type": "tool_use", "id": "call_2", "name": "list_tables",
                       "input": {"note": "quo\"te"}}),
            ],
        },
        Message::User {
            content: vec![
                UserBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "users".into(),
                    is_error: false,
                },
                UserBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "boom".into(),
                    is_error: true,
                },
            ],
        },
        Message::Assistant {
            content: vec![json!({"type": "text", "text": "There are 3."})],
        },
        Message::user_text("Thanks"),
    ]
}

#[test]
fn the_history_is_written_as_chat_completions_wants_it() {
    let rig = rig(Flavor::Compatible, vec![complete(&[]).into()]);
    let messages = history();
    let tools = [spec()];
    let _ = run(&rig, &request("local-model", &messages, &tools));
    let sent = rig.transport.request(0);
    assert_eq!(sent.method, "POST");
    assert_eq!(sent.url, "http://localhost:1234/v1/chat/completions");
    let expected = json!({
        "model": "local-model",
        "messages": [
            {"role": "system", "content": "CORE\n\nCTX"},
            {"role": "user", "content":
                "How many users?\n\n<system-reminder>\nWrites are off.\n</system-reminder>"},
            {"role": "assistant", "content": "Let me look.", "tool_calls": [
                {"id": "call_1", "type": "function", "function":
                    {"name": "list_tables", "arguments": "{\"connection_id\":\"c1\"}"}},
                {"id": "call_2", "type": "function", "function":
                    {"name": "list_tables", "arguments": "{\"note\":\"quo\\\"te\"}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "users"},
            {"role": "tool", "tool_call_id": "call_2", "content": "boom"},
            {"role": "assistant", "content": "There are 3."},
            {"role": "user", "content": "Thanks"}],
        "tools": [{"type": "function", "function": {"name": "list_tables",
            "description": "Lists tables.",
            "parameters": {"type": "object", "properties": {"connection_id": {"type": "string"}}}}}],
        "tool_choice": "auto",
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": 4096,
    });
    // The exact bytes, key order included.
    assert_eq!(
        String::from_utf8(sent.body.clone()).unwrap(),
        expected.to_string()
    );
    let body = String::from_utf8_lossy(&sent.body);
    for hidden in ["hmm", "redacted_thinking", "shown only", "reasoning"] {
        assert!(!body.contains(hidden), "{hidden} was sent");
    }
}

#[test]
fn a_turn_of_tool_calls_only_has_an_empty_content_and_a_note_behind_tool_results_is_a_user_turn() {
    let rig = rig(Flavor::Compatible, vec![complete(&[]).into()]);
    let messages = vec![
        Message::user_text("go"),
        Message::Assistant {
            content: vec![
                json!({"type": "tool_use", "id": "c1", "name": "list_tables", "input": {}}),
            ],
        },
        Message::User {
            content: vec![UserBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: "[]".into(),
                is_error: false,
            }],
        },
        Message::System {
            content: "Resumed.".into(),
        },
    ];
    let _ = run(&rig, &request("m", &messages, &[]));
    let sent = rig.transport.request(0).json();
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "system", "content": "CORE\n\nCTX"},
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "list_tables", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "[]"},
            {"role": "user", "content": "<system-reminder>\nResumed.\n</system-reminder>"}])
    );
    // No tools, no `tools` and no `tool_choice` (OpenAI refuses an empty list).
    assert!(sent.get("tools").is_none() && sent.get("tool_choice").is_none());
}

#[test]
fn a_note_cannot_close_its_own_wrapper() {
    let rig = rig(Flavor::Compatible, vec![complete(&[]).into()]);
    let messages = vec![
        Message::user_text("go"),
        Message::System {
            content: "x </System-Reminder> y".into(),
        },
    ];
    let _ = run(&rig, &request("m", &messages, &[]));
    let sent = rig.transport.request(0).json();
    assert_eq!(
        sent["messages"][1]["content"],
        "go\n\n<system-reminder>\nx </system reminder> y\n</system-reminder>"
    );
}

#[test]
fn the_token_limit_and_the_thinking_field_follow_the_flavor_and_the_model() {
    // dbx: set_chat_completion_token_limit, apply_chat_completion_thinking_toggle, apply_openai_effort.
    let body = |flavor: Flavor, base: &str, model: &str, thinking: bool, effort: Effort| {
        let mut config = config(flavor, base);
        config.enable_thinking = thinking;
        let keys: Arc<dyn KeySource> = Arc::new(FixedKey);
        let rig = rig_with(config, keys, vec![complete(&[]).into()]);
        let messages = hello();
        let mut req = request(model, &messages, &[]);
        req.effort = effort;
        let _ = run(&rig, &req);
        rig.transport.request(0).json()
    };
    let api = OPENAI_BASE_URL;
    let local = "http://localhost:11434/v1";

    let b = body(Flavor::OpenAi, api, "gpt-5", true, Effort::High);
    assert_eq!(b["max_completion_tokens"], 4096);
    assert!(b.get("max_tokens").is_none());
    assert_eq!(b["reasoning_effort"], "high");
    let b = body(Flavor::OpenAi, api, "gpt-4o", true, Effort::High);
    assert_eq!(b["max_tokens"], 4096);
    assert!(b.get("max_completion_tokens").is_none() && b.get("reasoning_effort").is_none());
    // OpenAI semantics stay through a gateway; a compatible server on this machine is not OpenAI.
    let b = body(Flavor::OpenAi, local, "gpt-5-proxy", true, Effort::Low);
    assert_eq!(b["max_completion_tokens"], 4096);
    let b = body(Flavor::Compatible, local, "gpt-5-local", true, Effort::Low);
    assert_eq!(b["max_tokens"], 4096);
    assert!(b.get("reasoning_effort").is_none());

    // Thinking off: nothing for OpenAI, `reasoning_effort: none` for Ollama, the template switch
    // for other compatible servers, unless the model is a Kimi K2.5+.
    let b = body(Flavor::OpenAi, api, "gpt-5", false, Effort::High);
    assert!(b.get("extra_body").is_none() && b.get("reasoning_effort").is_none());
    let b = body(Flavor::Compatible, api, "gpt-5", false, Effort::High);
    assert!(b.get("extra_body").is_none() && b.get("reasoning_effort").is_none());
    let b = body(
        Flavor::Ollama,
        local,
        "deepseek-r1:14b",
        false,
        Effort::High,
    );
    assert_eq!(b["reasoning_effort"], "none");
    assert!(b.get("extra_body").is_none());
    let b = body(
        Flavor::Compatible,
        "https://example.com/v1",
        "qwen3",
        false,
        Effort::High,
    );
    assert_eq!(
        b["extra_body"],
        json!({"chat_template_kwargs": {"enable_thinking": false}})
    );
    assert!(b.get("reasoning_effort").is_none());
    let b = body(
        Flavor::Compatible,
        "https://example.com/v1",
        "kimi-k2.6",
        false,
        Effort::High,
    );
    assert!(b.get("extra_body").is_none());

    // Ollama with thinking on: the effort its model lists.
    let b = body(Flavor::Ollama, local, "gpt-oss:20b", true, Effort::Medium);
    assert_eq!(b["reasoning_effort"], "medium");
    let b = body(Flavor::Ollama, local, "qwen3:0.6b", true, Effort::Low);
    assert_eq!(b["reasoning_effort"], "high");
    let b = body(Flavor::Ollama, local, "llama3.2", true, Effort::Low);
    assert!(b.get("reasoning_effort").is_none());
}

#[test]
fn the_key_goes_out_as_a_bearer_header_only() {
    let rig = rig(Flavor::OpenAi, vec![complete(&[]).into()]);
    let _ = turn(&rig, "gpt-4o");
    let sent = rig.transport.request(0);
    assert_eq!(sent.url, "https://api.openai.com/v1/chat/completions");
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {FAKE_KEY}").as_str())
    );
    assert_eq!(sent.header("content-type"), Some("application/json"));
    assert!(sent.header("x-api-key").is_none());
    assert_no_key(&sent.debug);
    assert_no_key(&String::from_utf8_lossy(&sent.body));
}

#[test]
fn a_key_is_needed_as_dbx_says_and_a_missing_one_sends_nothing() {
    // dbx provider_requires_api_key: OpenAI only.
    let openai = OpenAiProvider::new(
        ScriptedTransport::new(vec![]),
        Arc::new(NoKey),
        OpenAiConfig::default(),
    )
    .unwrap();
    let messages = hello();
    let req = request("gpt-4o", &messages, &[]);
    let result = openai.stream_turn(&req, &mut |_| {}, &CancelToken::new());
    assert!(matches!(result, Err(ProviderError::Key(KeyError::NotSet))));
    let empty = OpenAiProvider::new(
        ScriptedTransport::new(vec![]),
        Arc::new(EmptyKey),
        OpenAiConfig::default(),
    )
    .unwrap();
    let result = empty.stream_turn(&req, &mut |_| {}, &CancelToken::new());
    assert!(matches!(result, Err(ProviderError::Key(KeyError::Empty))));
    assert!(matches!(
        openai.list_models(&CancelToken::new()),
        Err(ProviderError::Key(KeyError::NotSet))
    ));

    for flavor in [Flavor::Ollama, Flavor::Compatible] {
        let rig = rig(flavor, vec![complete(&[text_chunk("ok")]).into()]);
        let (response, _) = ok(turn(&rig, "m"));
        assert_eq!(
            response.content,
            vec![json!({"type": "text", "text": "ok"})]
        );
        assert!(rig.transport.request(0).header("authorization").is_none());
    }
    // A key that is there is sent to Ollama too (a remote one behind a proxy).
    let keyed = rig_with(
        config(Flavor::Ollama, OLLAMA_BASE_URL),
        Arc::new(FixedKey),
        vec![complete(&[]).into()],
    );
    let _ = turn(&keyed, "m");
    assert!(keyed.transport.request(0).header("authorization").is_some());
}

// --------------------------------------------------------------------------------------- stream

#[test]
fn a_text_turn_with_usage_becomes_one_text_block() {
    let reply = complete(&[
        chunk(json!({"role": "assistant", "content": ""}), None),
        text_chunk("Hel"),
        text_chunk("lo \"world\"\nline2 😀"),
        chunk(json!({}), Some("stop")),
        usage_chunk(120, 30, Some(100)),
    ]);
    let rig = rig(Flavor::OpenAi, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "gpt-4o"));
    assert_eq!(
        response.content,
        vec![json!({"type": "text", "text": "Hello \"world\"\nline2 😀"})]
    );
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 30,
            cache_read_input_tokens: 100,
            cache_creation_input_tokens: 0,
        }
    );
    assert_eq!(response.model.as_deref(), Some("gpt-test"));
    assert_eq!(response.request_id.as_deref(), Some("req_scripted"));
    assert_eq!(
        events,
        vec![
            "TextDelta(\"Hel\")".to_owned(),
            "TextDelta(\"lo \\\"world\\\"\\nline2 😀\")".to_owned()
        ]
    );
}

#[test]
fn the_request_id_comes_from_the_header() {
    let reply = complete(&[text_chunk("x"), chunk(json!({}), Some("stop"))])
        .header("x-request-id", "req_oai");
    let rig = rig(Flavor::OpenAi, vec![reply.into()]);
    let (response, _) = ok(turn(&rig, "gpt-4o"));
    assert_eq!(response.request_id.as_deref(), Some("req_oai"));
}

#[test]
fn reasoning_is_streamed_as_thinking_and_stored_in_front_of_the_text() {
    // dbx parses_ollama_openai_reasoning_stream_chunks: `reasoning`, `thinking`, and the
    // `reasoning_content` of DeepSeek-style servers.
    let reply = complete(&[
        chunk(json!({"reasoning_content": "plan "}), None),
        chunk(json!({"reasoning": "more "}), None),
        chunk(json!({"thinking": "done"}), None),
        text_chunk("Answer"),
        chunk(json!({}), Some("stop")),
    ]);
    let rig = rig(Flavor::Ollama, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "qwen3"));
    assert_eq!(
        response.content,
        vec![
            json!({"type": "reasoning", "text": "plan more done"}),
            json!({"type": "text", "text": "Answer"})
        ]
    );
    assert_eq!(
        events,
        vec![
            "ThinkingDelta(\"plan \")",
            "ThinkingDelta(\"more \")",
            "ThinkingDelta(\"done\")",
            "TextDelta(\"Answer\")"
        ]
    );
}

#[test]
fn proxy_response_shapes_are_read_as_dbx_reads_them() {
    // dbx parses_openai_compatible_proxy_response_shapes, plus the other places `openai_stream_text`
    // looks: `choices[0].text`, the top-level `content`, a part list with the four field names.
    let reply = complete(&[
        json!({"choices": [{"delta": {"content": [{"type": "text", "text": "SELECT "},
                                                  {"type": "text", "text": "1;"}]}}]}),
        json!({"choices": [{"message": {"content": " a"}}]}),
        json!({"choices": [{"text": " b"}]}),
        json!({"type": "response.output_text.delta", "delta": " c"}),
        json!({"content": " d"}),
        json!({"choices": [{"delta": {"content": [{"output_text": " e"}, {"input_text": "f"}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, _) = ok(turn(&rig, "m"));
    assert_eq!(
        response.content,
        vec![json!({"type": "text", "text": "SELECT 1; a b c d ef"})]
    );
}

#[test]
fn parallel_tool_calls_split_across_chunks_become_tool_use_blocks() {
    let reply = complete(&[
        text_chunk("Looking."),
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "call_a", "type": "function",
                    "function": {"name": "list_tables", "arguments": ""}}]}),
            None,
        ),
        // GLM and others send an empty id on the later deltas: not a new call.
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "", "function": {"arguments": "{\"connec"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [
                {"index": 0, "function": {"arguments": "tion_id\": \"c1\"}"}},
                {"index": 1, "id": "call_b", "type": "function",
                 "function": {"name": "execute_query", "arguments": "{\"statement\":"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 1, "id": "", "function": {"arguments": " \"SELECT 1\"}"}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        usage_chunk(10, 20, None),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.content,
        vec![
            json!({"type": "text", "text": "Looking."}),
            json!({"type": "tool_use", "id": "call_a", "name": "list_tables",
                   "input": {"connection_id": "c1"}}),
            json!({"type": "tool_use", "id": "call_b", "name": "execute_query",
                   "input": {"statement": "SELECT 1"}}),
        ]
    );
    assert!(response.invalid_tool_inputs.is_empty());
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("ToolCallStarted"))
            .count(),
        2
    );
    assert!(events[1].contains("ToolCallStarted") && events[1].contains("call_a"));
    assert!(events.iter().any(|e| e.contains("ToolInputReady")
        && e.contains("call_b")
        && e.contains("valid: true")));
}

#[test]
fn arguments_that_are_not_an_object_become_an_empty_input_and_are_reported() {
    let reply = complete(&[
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "c1",
                "function": {"name": "list_tables", "arguments": "{\"connec"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 1, "id": "c2",
                "function": {"name": "list_tables", "arguments": "[1]"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": 2, "id": "c3", "function": {"name": "list_tables"}}]}),
            None,
        ),
        chunk(json!({}), Some("stop")),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    // `stop` with calls is a tool turn.
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response
            .content
            .iter()
            .map(|b| b["input"].clone())
            .collect::<Vec<_>>(),
        vec![json!({}), json!({}), json!({})]
    );
    let invalid: Vec<_> = response
        .invalid_tool_inputs
        .iter()
        .map(|i| (i.tool_use_id.as_str(), i.raw.as_str()))
        .collect();
    assert_eq!(invalid, vec![("c1", "{\"connec"), ("c2", "[1]")]);
    assert!(events
        .iter()
        .any(|e| e.contains("ToolInputReady") && e.contains("c1") && e.contains("valid: false")));
    assert!(events
        .iter()
        .any(|e| e.contains("ToolInputReady") && e.contains("c3") && e.contains("valid: true")));
}

#[test]
fn a_new_id_on_a_used_index_is_a_new_call_and_a_call_without_an_id_gets_a_generated_one() {
    let call = |index: u32, id: Option<&str>, name: Option<&str>, args: &str| {
        let mut call = json!({"index": index, "function": {"arguments": args}});
        if let Some(id) = id {
            call["id"] = json!(id);
        }
        if let Some(name) = name {
            call["function"]["name"] = json!(name);
        }
        chunk(json!({"tool_calls": [call]}), None)
    };
    let reply = complete(&[
        // Some servers number every call 0.
        call(0, Some("x1"), Some("list_tables"), "{}"),
        call(0, Some("x2"), Some("list_tables"), "{\"a\":1}"),
        // Never named: the index and the name are all there is.
        call(1, None, Some("list_tables"), "{\"b\""),
        call(1, None, None, ":2}"),
        call(2, None, Some("list_tables"), "{}"),
        chunk(json!({}), Some("tool_calls")),
    ]);
    let rig = rig(Flavor::Ollama, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    let ids: Vec<_> = response
        .content
        .iter()
        .map(|b| b["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids[0], "x1");
    assert_eq!(ids[1], "x2");
    assert!(ids[2].starts_with("call_") && ids[3].starts_with("call_"));
    assert_ne!(ids[2], ids[3]);
    let inputs: Vec<_> = response
        .content
        .iter()
        .map(|b| b["input"].clone())
        .collect();
    assert_eq!(
        inputs,
        vec![json!({}), json!({"a": 1}), json!({"b": 2}), json!({})]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("ToolCallStarted"))
            .count(),
        4
    );

    // Another turn never repeats a generated id.
    let again = ok(turn(
        &self::rig(
            Flavor::Ollama,
            vec![complete(&[
                call(0, None, Some("t"), "{}"),
                chunk(json!({}), Some("stop")),
            ])
            .into()],
        ),
        "m",
    ))
    .0;
    assert!(!ids.contains(&again.content[0]["id"].as_str().unwrap().to_owned()));
}

#[test]
fn finish_reasons_map_to_stop_reasons() {
    for (reason, expected) in [
        ("stop", StopReason::EndTurn),
        ("length", StopReason::MaxTokens),
        ("content_filter", StopReason::Refusal { category: None }),
        ("tool_calls", StopReason::ToolUse),
        ("function_call", StopReason::ToolUse),
        ("something_new", StopReason::Other("something_new".into())),
    ] {
        let reply = complete(&[text_chunk("x"), chunk(json!({}), Some(reason))]);
        let rig = rig(Flavor::Compatible, vec![reply.into()]);
        assert_eq!(ok(turn(&rig, "m")).0.stop_reason, expected, "{reason}");
    }
    // No finish reason but `[DONE]`: an end of turn.
    let rig = rig(
        Flavor::Compatible,
        vec![complete(&[text_chunk("x")]).into()],
    );
    assert_eq!(ok(turn(&rig, "m")).0.stop_reason, StopReason::EndTurn);
}

#[test]
fn blank_text_is_not_stored() {
    let reply = complete(&[text_chunk(" \n"), chunk(json!({}), Some("stop"))]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    assert!(ok(turn(&rig, "m")).0.content.is_empty());
}

#[test]
fn the_stream_ends_shortly_after_a_finish_reason_when_no_done_comes() {
    // dbx openai_tool_stream_finishes_without_done_marker_after_finish_reason: the server sends a
    // finish reason, then keeps the connection open.
    let (blocked, entered) = mpsc::channel();
    let reply = Reply::ok(sse(&[text_chunk("done"), chunk(json!({}), Some("stop"))]))
        .end(End::BlockUntilCancel(Some(blocked)));
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let started = Instant::now();
    let (response, _) = ok(turn(&rig, "m"));
    assert_eq!(
        response.content,
        vec![json!({"type": "text", "text": "done"})]
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    // The helper that was reading the rest of the body is released: the connection was closed.
    assert!(entered.recv_timeout(Duration::from_secs(5)).is_ok());
}

#[test]
fn the_usage_chunk_that_follows_the_finish_reason_is_kept_without_done() {
    let reply = Reply::ok(sse(&[
        text_chunk("x"),
        chunk(json!({}), Some("stop")),
        usage_chunk(5, 6, None),
    ]));
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, _) = ok(turn(&rig, "m"));
    assert_eq!(response.usage.input_tokens, 5);
    assert_eq!(response.usage.output_tokens, 6);
}

#[test]
fn a_stream_that_ends_with_neither_done_nor_a_finish_reason_is_truncated() {
    for body in [
        String::new(),
        sse(&[text_chunk("half")]),
        ": keep-alive\n\n".to_owned(),
    ] {
        let rig = rig(Flavor::Compatible, vec![Reply::ok(body).into()]);
        assert!(matches!(
            err(turn(&rig, "m")),
            ProviderError::Protocol {
                source: ProtocolError::Truncated,
                ..
            }
        ));
        assert_eq!(
            rig.transport.request_count(),
            1,
            "a broken stream is not sent again"
        );
    }
}

#[test]
fn a_stream_that_breaks_is_a_transport_error() {
    let reply = Reply::ok(sse(&[text_chunk("half")])).end(End::Break);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    assert!(matches!(
        err(turn(&rig, "m")),
        ProviderError::Transport { .. }
    ));
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn an_error_in_the_stream_is_an_api_failure_in_both_shapes() {
    let after_text = |error: Value| {
        let reply = Reply::ok(format!("{}data: {error}\n\n", sse(&[text_chunk("so far")])));
        let rig = rig(Flavor::Compatible, vec![reply.into()]);
        let error = err(turn(&rig, "m"));
        assert_eq!(
            rig.transport.request_count(),
            1,
            "after text: no new request"
        );
        error
    };
    let error = after_text(
        json!({"error": {"message": "The server had an error", "type": "server_error", "code": null}}),
    );
    assert!(
        matches!(&error, ProviderError::Server(f) if f.message == "The server had an error" && f.status.is_none())
    );
    // Ollama: the error is the text.
    let error = after_text(json!({"error": "model runner has unexpectedly stopped"}));
    assert!(
        matches!(&error, ProviderError::Rejected(f) if f.message == "model runner has unexpectedly stopped")
    );
    let error = after_text(
        json!({"error": {"message": "slow down", "type": "tokens", "code": "rate_limit_exceeded"}}),
    );
    assert!(matches!(error, ProviderError::RateLimited(_)));
    let error = after_text(json!({"error": {"message": "bad key", "code": "invalid_api_key"}}));
    assert!(matches!(error, ProviderError::Authentication(_)));
    // `error: null` on a normal chunk is no error.
    let reply = complete(&[
        json!({"error": null, "choices": [{"delta": {"content": "fine"}, "finish_reason": "stop"}]}),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    assert_eq!(ok(turn(&rig, "m")).0.text(), "fine");
}

#[test]
fn an_error_before_any_content_is_retried_like_the_http_error_it_stands_for() {
    let failing = Reply::ok(format!(
        "data: {}\n\n",
        json!({"error": {"message": "Rate limit reached", "type": "rate_limit_exceeded"}})
    ));
    let good = complete(&[text_chunk("ok"), chunk(json!({}), Some("stop"))]);
    let rig = rig(Flavor::Compatible, vec![failing.into(), good.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    assert_eq!(
        response.content,
        vec![json!({"type": "text", "text": "ok"})]
    );
    assert_eq!(rig.transport.request_count(), 2);
    assert_eq!(rig.sleeper.waits().len(), 1);
    assert!(events[0].starts_with("Retrying") && events[0].contains("RateLimited"));
}

#[test]
fn a_chunk_that_is_no_json_is_skipped_within_the_bound_on_idle_events() {
    let reply = Reply::ok(format!(
        "data: not json\n\n{}{DONE}",
        sse(&[text_chunk("ok"), chunk(json!({}), Some("stop"))])
    ));
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    assert_eq!(ok(turn(&rig, "m")).0.text(), "ok");

    let junk = "data: nope\n\n".repeat(30);
    let rig = self::rig(Flavor::Compatible, vec![Reply::ok(junk).into()]);
    let provider = OpenAiProvider::new(
        Arc::clone(&rig.transport) as Arc<dyn HttpTransport>,
        Arc::new(NoKey),
        config(Flavor::Compatible, "http://localhost:1/v1"),
    )
    .unwrap()
    .with_limits(TurnLimits {
        max_idle_events: 10,
        ..TurnLimits::default()
    });
    let messages = hello();
    let result = provider.stream_turn(
        &request("m", &messages, &[]),
        &mut |_| {},
        &CancelToken::new(),
    );
    assert!(matches!(
        result,
        Err(ProviderError::Protocol {
            source: ProtocolError::TooManyIdleEvents { limit: 10 },
            ..
        })
    ));
}

#[test]
fn the_size_of_a_turn_is_bounded() {
    let big = sse(&[
        text_chunk(&"x".repeat(2000)),
        chunk(json!({}), Some("stop")),
    ]);
    let transport = ScriptedTransport::new(vec![Reply::ok(big).into()]);
    let provider = OpenAiProvider::new(
        transport as Arc<dyn HttpTransport>,
        Arc::new(NoKey),
        config(Flavor::Compatible, "http://localhost:1/v1"),
    )
    .unwrap()
    .with_limits(TurnLimits {
        max_bytes: 1000,
        ..TurnLimits::default()
    });
    let messages = hello();
    let result = provider.stream_turn(
        &request("m", &messages, &[]),
        &mut |_| {},
        &CancelToken::new(),
    );
    assert!(matches!(
        result,
        Err(ProviderError::Protocol {
            source: ProtocolError::TurnTooLarge { limit: 1000 },
            ..
        })
    ));
    let too_many: Vec<Value> = (0..5)
        .map(|i| chunk(json!({"tool_calls": [{"index": i, "id": format!("c{i}"), "function": {"name": "t"}}]}), None))
        .collect();
    let transport = ScriptedTransport::new(vec![complete(&too_many).into()]);
    let provider = OpenAiProvider::new(
        transport as Arc<dyn HttpTransport>,
        Arc::new(NoKey),
        config(Flavor::Compatible, "http://localhost:1/v1"),
    )
    .unwrap()
    .with_limits(TurnLimits {
        max_blocks: 3,
        ..TurnLimits::default()
    });
    let result = provider.stream_turn(
        &request("m", &messages, &[]),
        &mut |_| {},
        &CancelToken::new(),
    );
    assert!(matches!(
        result,
        Err(ProviderError::Protocol {
            source: ProtocolError::TooManyBlocks { limit: 3 },
            ..
        })
    ));
}

/// Every stream the assembler reads gives the same events and turn however it is chunked.
fn assemble(chunks: &[&[u8]]) -> (Vec<String>, TurnResponse) {
    let mut parser = SseParser::default();
    let mut assembler = ChatAssembler::new(
        TurnLimits::default(),
        crate::wire::errors::Scrub::new(ApiKey::new("")),
        &[],
    );
    let mut events = Vec::new();
    for chunk in chunks {
        let done = parser
            .feed::<crate::wire::StreamError>(chunk, &mut |event| {
                assembler.handle(event.data, &mut |e| events.push(format!("{e:?}")))
            })
            .unwrap();
        if done {
            break;
        }
    }
    let turn = assembler
        .finish(Some("req".into()), &mut |e| events.push(format!("{e:?}")))
        .unwrap();
    (events, turn)
}

fn fixture() -> String {
    format!(
        "{}{DONE}",
        sse(&[
            chunk(json!({"reasoning_content": "Hmm, ü — 日本"}), None),
            text_chunk("Hello \"world\"\nline2 "),
            text_chunk("😀"),
            chunk(
                json!({"tool_calls": [{"index": 0, "id": "c1", "function": {"name": "list_tables", "arguments": "{\"connec"}}]}),
                None
            ),
            chunk(
                json!({"tool_calls": [{"index": 0, "function": {"arguments": "tion_id\":\"c1\"}"}}]}),
                None
            ),
            chunk(
                json!({"tool_calls": [{"index": 1, "id": "c2", "function": {"name": "t", "arguments": "{"}}]}),
                None
            ),
            chunk(json!({}), Some("tool_calls")),
            usage_chunk(7, 8, Some(2)),
        ])
    )
}

#[test]
fn the_result_does_not_depend_on_the_chunking_or_the_line_ends() {
    let text = fixture();
    let bytes = text.as_bytes();
    let whole = assemble(&[bytes]);
    assert_eq!(whole.1.content.len(), 4);
    for at in 0..=bytes.len() {
        assert_eq!(
            assemble(&[&bytes[..at], &bytes[at..]]),
            whole,
            "split at {at}"
        );
    }
    let singles: Vec<&[u8]> = bytes.chunks(1).collect();
    assert_eq!(assemble(&singles), whole);
    for replaced in [text.replace('\n', "\r\n"), text.replace('\n', "\r")] {
        let bytes = replaced.as_bytes();
        assert_eq!(assemble(&[bytes]), whole);
        let singles: Vec<&[u8]> = bytes.chunks(1).collect();
        assert_eq!(assemble(&singles), whole);
    }
}

// --------------------------------------------------------------------------------------- errors

#[test]
fn http_statuses_map_to_typed_errors_with_the_message_of_either_body_shape() {
    type Check = fn(&ProviderError) -> bool;
    let cases: [(u16, &str, Check); 7] = [
        (400, "bad", |e| matches!(e, ProviderError::BadRequest(_))),
        (401, "no", |e| matches!(e, ProviderError::Authentication(_))),
        (403, "no", |e| matches!(e, ProviderError::Permission(_))),
        (404, "no", |e| matches!(e, ProviderError::NotFound(_))),
        (413, "no", |e| matches!(e, ProviderError::TooLarge(_))),
        (402, "no", |e| matches!(e, ProviderError::Rejected(_))),
        (422, "no", |e| matches!(e, ProviderError::Rejected(_))),
    ];
    for (status, message, matches) in cases {
        let rig = rig(
            Flavor::OpenAi,
            vec![Reply::error(status, "invalid_request_error", message).into()],
        );
        let error = err(turn(&rig, "m"));
        assert!(matches(&error), "{status}: {error:?}");
        assert_eq!(error.api_failure().unwrap().message, message);
        assert_eq!(error.api_failure().unwrap().status, Some(status));
        assert_eq!(rig.transport.request_count(), 1, "{status} is not retried");
    }
    // Ollama: `{"error": "text"}`.
    let body = json!({"error": "model 'nope' not found"}).to_string();
    let rig = rig(Flavor::Ollama, vec![Reply::with_status(404, body).into()]);
    let error = err(turn(&rig, "nope"));
    assert!(matches!(&error, ProviderError::NotFound(f) if f.message == "model 'nope' not found"));
    // A body that is no JSON still gives a typed error.
    let rig = self::rig(
        Flavor::OpenAi,
        vec![Reply::with_status(400, "<html>").into()],
    );
    assert_eq!(
        err(turn(&rig, "m")).api_failure().unwrap().message,
        "the server answered HTTP 400"
    );
}

#[test]
fn a_429_and_the_5xx_are_retried_within_the_policy_then_the_typed_error() {
    let reply = || complete(&[text_chunk("ok"), chunk(json!({}), Some("stop"))]);
    let rig = rig(
        Flavor::OpenAi,
        vec![
            Reply::error(429, "rate_limit_exceeded", "slow")
                .header("retry-after", "7")
                .into(),
            Reply::error(500, "server_error", "oops").into(),
            Reply::error(529, "overloaded_error", "busy").into(),
            reply().into(),
        ],
    );
    let (response, events) = ok(turn(&rig, "m"));
    assert_eq!(response.text(), "ok");
    assert_eq!(rig.transport.request_count(), 4);
    assert_eq!(
        rig.sleeper.waits(),
        vec![
            Duration::from_secs(7),
            Duration::from_secs(2),
            Duration::from_secs(4)
        ]
    );
    assert_eq!(events.len(), 3 + 1);
    assert!(
        events[0].contains("RateLimited")
            && events[1].contains("Server")
            && events[2].contains("Overloaded")
    );

    let rig = self::rig(
        Flavor::OpenAi,
        (0..5)
            .map(|_| Reply::error(503, "server_error", "down").into())
            .collect(),
    );
    assert!(matches!(err(turn(&rig, "m")), ProviderError::Server(_)));
    assert_eq!(
        rig.transport.request_count(),
        4,
        "the first try and three retries"
    );

    // A connection that fails before the first byte is tried once more.
    let rig = self::rig(
        Flavor::OpenAi,
        vec![
            Step::Fail(TransportError::Failed("reset".into())),
            reply().into(),
        ],
    );
    assert_eq!(ok(turn(&rig, "m")).0.text(), "ok");
    assert_eq!(rig.transport.request_count(), 2);
}

#[test]
fn cancel_ends_a_stream_that_is_blocked() {
    let (blocked, entered) = mpsc::channel();
    let reply = Reply::ok(sse(&[text_chunk("so far")])).end(End::BlockUntilCancel(Some(blocked)));
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let cancel = CancelToken::new();
    let messages = hello();
    let req = request("m", &messages, &[]);
    let started = Instant::now();
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| rig.provider.stream_turn(&req, &mut |_| {}, &cancel));
        entered
            .recv_timeout(Duration::from_secs(10))
            .expect("the read blocks");
        cancel.cancel();
        worker.join().unwrap()
    });
    assert!(matches!(result, Err(ProviderError::Cancelled)));
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(rig.transport.request_count(), 1);

    // Cancelled before the request: nothing is sent.
    let rig = self::rig(Flavor::Compatible, vec![complete(&[]).into()]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let result = rig.provider.stream_turn(&req, &mut |_| {}, &cancel);
    assert!(matches!(result, Err(ProviderError::Cancelled)));
    assert_eq!(rig.transport.request_count(), 0);
}

#[test]
fn a_cancel_during_the_wait_before_a_retry_ends_the_turn() {
    let rig = rig(
        Flavor::OpenAi,
        vec![
            Reply::error(429, "rate_limit_exceeded", "slow").into(),
            complete(&[]).into(),
        ],
    );
    let (entered, waiting) = mpsc::channel();
    rig.sleeper.block_on_cancel(entered);
    let cancel = CancelToken::new();
    let messages = hello();
    let req = request("m", &messages, &[]);
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| rig.provider.stream_turn(&req, &mut |_| {}, &cancel));
        waiting.recv_timeout(Duration::from_secs(10)).unwrap();
        cancel.cancel();
        worker.join().unwrap()
    });
    assert!(matches!(result, Err(ProviderError::Cancelled)));
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn the_key_is_in_no_error_event_or_debug_text() {
    let leak = format!("the key {FAKE_KEY} is wrong");
    let mut texts = Vec::new();
    let mut check = |turn: Turn| {
        let (result, events) = turn;
        if let Ok(response) = &result {
            texts.push(format!("{response:?}"));
        }
        if let Err(error) = &result {
            texts.push(format!("{error}"));
            texts.push(format!("{error:?}"));
        }
        texts.extend(events);
    };
    // The key in an HTTP error body, a stream error, a model name, a tool name or id, a request
    // id, a transport failure.
    let rig1 = rig(
        Flavor::OpenAi,
        vec![Reply::error(400, "invalid_request_error", &leak).into()],
    );
    check(turn(&rig1, "m"));
    let stream_error = Reply::ok(format!(
        "data: {}\n\n",
        json!({"error": {"message": leak, "type": "server_error"}})
    ));
    let rig2 = rig(Flavor::OpenAi, vec![stream_error.into()]);
    check(turn(&rig2, "m"));
    let mut named = chunk(json!({"content": "x"}), Some("stop"));
    named["model"] = json!(format!("{FAKE_KEY}-model"));
    let rig3 = rig(
        Flavor::OpenAi,
        vec![complete(&[named]).header("x-request-id", FAKE_KEY).into()],
    );
    check(turn(&rig3, "m"));
    let call = |id: &str, name: &str| {
        chunk(
            json!({"tool_calls": [{"index": 0, "id": id, "function": {"name": name, "arguments": "{}"}}]}),
            Some("tool_calls"),
        )
    };
    let rig4 = rig(
        Flavor::OpenAi,
        vec![complete(&[call(FAKE_KEY, "t")]).into()],
    );
    check(turn(&rig4, "m"));
    let rig5 = rig(
        Flavor::OpenAi,
        vec![complete(&[call("c1", FAKE_KEY)]).into()],
    );
    check(turn(&rig5, "m"));
    let rig6 = rig(
        Flavor::OpenAi,
        vec![
            Step::Fail(TransportError::Failed(leak.clone())),
            Step::Fail(TransportError::Failed(leak.clone())),
        ],
    );
    check(turn(&rig6, "m"));
    let rig7 = rig(
        Flavor::OpenAi,
        vec![Reply::with_status(200, format!("data: {}\n\n", leak)).into()],
    );
    check(turn(&rig7, "m"));
    let rig8 = rig(
        Flavor::OpenAi,
        vec![Reply::error(401, "authentication_error", &leak)
            .header("request-id", FAKE_KEY)
            .into()],
    );
    check(turn(&rig8, "m"));
    for r in [&rig1, &rig2, &rig3, &rig4, &rig5, &rig6, &rig7, &rig8] {
        for index in 0..r.transport.request_count() {
            let sent = r.transport.request(index);
            texts.push(sent.debug);
            texts.push(String::from_utf8_lossy(&sent.body).into_owned());
        }
        texts.push(format!("{:?}", r.provider.config()));
    }
    assert!(!texts.is_empty());
    for text in texts {
        assert_no_key(&text);
    }
}

// ------------------------------------------------------------------------------------ run sessions

#[test]
fn a_run_session_belongs_to_the_provider_that_issued_it() {
    let one = rig(Flavor::OpenAi, vec![complete(&[]).into()]);
    let two = rig(Flavor::OpenAi, vec![complete(&[]).into()]);
    let session = one.provider.begin_run().unwrap();
    let messages = hello();
    let req = request("m", &messages, &[]);
    let result = two
        .provider
        .stream_turn_in_run(&session, &req, &mut |_| {}, &CancelToken::new());
    assert!(matches!(result, Err(ProviderError::ForeignRunSession)));
    assert_eq!(two.transport.request_count(), 0);
    // An Anthropic provider's session is refused too (its key would go to another host).
    let anthropic = crate::AnthropicProvider::new(
        ScriptedTransport::new(vec![]),
        Arc::new(FixedKey),
        crate::AnthropicConfig::default(),
    )
    .unwrap();
    let foreign = anthropic.begin_run().unwrap();
    let result = one
        .provider
        .stream_turn_in_run(&foreign, &req, &mut |_| {}, &CancelToken::new());
    assert!(matches!(result, Err(ProviderError::ForeignRunSession)));
    assert!(one
        .provider
        .stream_turn_in_run(&session, &req, &mut |_| {}, &CancelToken::new())
        .is_ok());
}

// -------------------------------------------------------------------------------------- config

#[test]
fn the_address_rule_is_the_one_of_the_anthropic_provider() {
    for url in [
        "https://api.openai.com/v1",
        "http://localhost:11434/v1",
        "http://127.0.0.1:8080",
        "http://[::1]:8080/v1",
        "HTTP://LOCALHOST/v1",
        "http://192.168.1.5:11434/v1",
        "http://example.com/v1",
        "https://user:pw@example.com/v1",
        "https://example.com/v1?x=1",
        "https://example.com/v1#frag",
        "ftp://example.com",
        "example.com/v1",
        "https://",
        "https://exa mple.com",
        "  https://example.com/v1/  ",
        "",
    ] {
        let anthropic = crate::AnthropicConfig {
            base_url: url.to_owned(),
            ..crate::AnthropicConfig::default()
        };
        let openai = OpenAiConfig {
            base_url: url.to_owned(),
            ..OpenAiConfig::default()
        };
        assert_eq!(openai.validate(), anthropic.validate(), "{url:?}");
    }
    assert_eq!(
        OpenAiConfig::for_flavor(Flavor::Compatible).validate(),
        Err(ConfigError::InvalidBaseUrl)
    );
    assert_eq!(
        config(Flavor::OpenAi, "http://example.com/v1").validate(),
        Err(ConfigError::InsecureBaseUrl)
    );
    let refused = OpenAiProvider::new(
        ScriptedTransport::new(vec![]),
        Arc::new(NoKey),
        config(Flavor::OpenAi, "http://example.com/v1"),
    );
    assert!(matches!(refused, Err(ConfigError::InsecureBaseUrl)));
}

#[test]
fn the_defaults_and_the_provider_keep_the_address_trimmed() {
    assert_eq!(
        OpenAiConfig::default().base_url,
        "https://api.openai.com/v1"
    );
    assert_eq!(
        OpenAiConfig::for_flavor(Flavor::Ollama).base_url,
        "http://localhost:11434/v1"
    );
    assert_eq!(OpenAiConfig::for_flavor(Flavor::Compatible).base_url, "");
    assert!(
        Flavor::OpenAi.requires_key()
            && !Flavor::Ollama.requires_key()
            && !Flavor::Compatible.requires_key()
    );
    let rig = rig_with(
        config(Flavor::Compatible, "  http://localhost:8080/ "),
        Arc::new(NoKey),
        vec![complete(&[]).into()],
    );
    assert_eq!(rig.provider.config().base_url, "http://localhost:8080");
    let _ = turn(&rig, "m");
    assert_eq!(
        rig.transport.request(0).url,
        "http://localhost:8080/v1/chat/completions"
    );
    // A pasted completion address is accepted.
    let rig = self::rig_with(
        config(Flavor::OpenAi, "https://api.openai.com/v1/chat/completions"),
        Arc::new(FixedKey),
        vec![complete(&[]).into()],
    );
    let _ = turn(&rig, "m");
    assert_eq!(
        rig.transport.request(0).url,
        "https://api.openai.com/v1/chat/completions"
    );
}

// ------------------------------------------------------------------------------------ model lists

fn list_reply(ids: &[&str]) -> Reply {
    let data: Vec<Value> = ids
        .iter()
        .map(|id| json!({"id": id, "object": "model"}))
        .collect();
    Reply::ok(json!({"object": "list", "data": data}).to_string())
}

#[test]
fn the_openai_list_is_filtered_and_sent_with_the_key() {
    let rig = rig(
        Flavor::OpenAi,
        vec![list_reply(&[
            "gpt-4o",
            "text-embedding-3-small",
            "whisper-1",
            "o4-mini",
            "gpt-4o",
        ])
        .into()],
    );
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["gpt-4o", "o4-mini"]
    );
    let sent = rig.transport.request(0);
    assert_eq!(
        (sent.method, sent.url.as_str()),
        ("GET", "https://api.openai.com/v1/models")
    );
    assert_eq!(
        sent.header("authorization"),
        Some(format!("Bearer {FAKE_KEY}").as_str())
    );
}

#[test]
fn a_compatible_list_is_not_filtered_and_names_are_read() {
    let body = json!({"data": [{"id": "text-embedding-private-chat"}, {"id": "future-model", "name": "Future"}]});
    let rig = rig(Flavor::Compatible, vec![Reply::ok(body.to_string()).into()]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(models.len(), 2);
    assert_eq!(models[1].display_name.as_deref(), Some("Future"));
    assert!(rig.transport.request(0).header("authorization").is_none());
}

#[test]
fn list_errors_are_typed() {
    let rig = rig(
        Flavor::OpenAi,
        vec![Reply::error(401, "authentication_error", "bad key").into()],
    );
    assert!(matches!(
        rig.provider.list_models(&CancelToken::new()),
        Err(ProviderError::Authentication(_))
    ));
    let rig = self::rig(Flavor::OpenAi, vec![Reply::ok("{\"models\": []}").into()]);
    assert!(matches!(
        rig.provider.list_models(&CancelToken::new()),
        Err(ProviderError::Protocol {
            source: ProtocolError::MalformedBody(_),
            ..
        })
    ));
    let rig = self::rig(
        Flavor::OpenAi,
        vec![Reply::ok("x".repeat(5 * 1024 * 1024)).into()],
    );
    assert!(matches!(
        rig.provider.list_models(&CancelToken::new()),
        Err(ProviderError::Protocol {
            source: ProtocolError::TurnTooLarge { .. },
            ..
        })
    ));
    // 429 is retried.
    let rig = self::rig(
        Flavor::OpenAi,
        vec![
            Reply::error(429, "rate_limit_exceeded", "x").into(),
            list_reply(&["gpt-4o"]).into(),
        ],
    );
    assert_eq!(
        rig.provider.list_models(&CancelToken::new()).unwrap().len(),
        1
    );
}

fn show_reply(capabilities: &[&str]) -> Reply {
    Reply::ok(json!({"capabilities": capabilities, "details": {"family": "x"}}).to_string())
}

fn probed(rig: &Rig) -> Vec<String> {
    let mut models: Vec<String> = (1..rig.transport.request_count())
        .map(|i| {
            let sent = rig.transport.request(i);
            assert_eq!(sent.method, "POST");
            assert_eq!(sent.url, "http://localhost:11434/api/show");
            sent.json()["model"].as_str().unwrap().to_owned()
        })
        .collect();
    models.sort();
    models
}

#[test]
fn the_ollama_list_drops_models_that_say_they_cannot_complete() {
    // dbx ollama_catalog_excludes_models_with_explicit_non_completion_capability.
    let rig = rig(
        Flavor::Ollama,
        vec![
            list_reply(&[
                "qwen3:0.6b",
                "embeddinggemma:latest",
                "legacy-model",
                "broken",
            ])
            .into(),
            show_reply(&["completion", "tools"]).into(),
            show_reply(&["embedding"]).into(),
            Reply::ok(json!({"details": {"family": "legacy"}}).to_string()).into(),
            Reply::error(500, "server_error", "probe failed").into(),
        ],
    );
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        vec!["qwen3:0.6b", "legacy-model", "broken"]
    );
    assert_eq!(
        probed(&rig),
        vec![
            "broken",
            "embeddinggemma:latest",
            "legacy-model",
            "qwen3:0.6b"
        ]
    );
    // One attempt per model: the 500 was not retried; no key, no authorization.
    assert_eq!(rig.transport.request_count(), 5);
    assert!(rig.transport.request(1).header("authorization").is_none());
    assert_eq!(
        rig.transport.request(0).url,
        "http://localhost:11434/v1/models"
    );
}

#[test]
fn the_ollama_probes_are_bounded_in_number() {
    let ids: Vec<String> = (0..models::MAX_PROBES + 6)
        .map(|i| format!("m{i:03}"))
        .collect();
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut steps: Vec<Step> = vec![list_reply(&id_refs).into()];
    // The first probed model is an embedding model; every other one completes.
    steps.push(show_reply(&["embedding"]).into());
    steps.extend((1..models::MAX_PROBES).map(|_| Step::from(show_reply(&["completion"]))));
    let rig = rig(Flavor::Ollama, steps);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(rig.transport.request_count(), 1 + models::MAX_PROBES);
    // The one dropped model, and the six past the cap kept unfiltered.
    assert_eq!(models.len(), ids.len() - 1);
    assert_eq!(rig.transport.steps_left(), 0);
}

#[test]
fn a_cancel_ends_the_ollama_list_before_the_next_probe() {
    let cancel = CancelToken::new();
    let reply = list_reply(&["a", "b"]);
    let hook = {
        let cancel = cancel.clone();
        move || cancel.cancel()
    };
    let rig = rig(
        Flavor::Ollama,
        vec![
            reply.hook_at(10, hook).into(),
            show_reply(&["completion"]).into(),
        ],
    );
    assert!(matches!(
        rig.provider.list_models(&cancel),
        Err(ProviderError::Cancelled)
    ));
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn the_selected_ollama_model_can_be_asked_for_tools() {
    // dbx ollama_selected_model_tool_support_reads_show_capabilities.
    let rig = rig(
        Flavor::Ollama,
        vec![
            show_reply(&["completion", "tools"]).into(),
            show_reply(&["completion"]).into(),
            show_reply(&[]).into(),
        ],
    );
    let cancel = CancelToken::new();
    assert_eq!(
        rig.provider
            .model_supports_tools(" qwen3:0.6b ", &cancel)
            .unwrap(),
        Some(true)
    );
    assert_eq!(
        rig.transport.request(0).json(),
        json!({"model": "qwen3:0.6b"})
    );
    assert_eq!(
        rig.provider.model_supports_tools("x", &cancel).unwrap(),
        Some(false)
    );
    assert_eq!(
        rig.provider.model_supports_tools("x", &cancel).unwrap(),
        None
    );
    let other = self::rig(Flavor::OpenAi, vec![]);
    assert_eq!(
        other
            .provider
            .model_supports_tools("gpt-4o", &cancel)
            .unwrap(),
        None
    );
    assert_eq!(other.transport.request_count(), 0);
}

// ------------------------------------------------------------- stored ids and names (B1)

fn fits(text: &str) -> bool {
    (1..=64).contains(&text.len())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn call_chunk(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> Value {
    let mut call = json!({"index": index, "function": {"arguments": args}});
    if let Some(id) = id {
        call["id"] = json!(id);
    }
    if let Some(name) = name {
        call["function"]["name"] = json!(name);
    }
    chunk(json!({"tool_calls": [call]}), None)
}

fn calls_reply(calls: Vec<Value>) -> Reply {
    let mut events = calls;
    events.push(chunk(json!({}), Some("tool_calls")));
    complete(&events)
}

fn stored_ids(response: &TurnResponse) -> Vec<String> {
    response
        .content
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["id"].as_str().unwrap().to_owned())
        .collect()
}

/// The history after a tool turn: the user message, the turn, and an answer to each call.
fn after_tool_turn(before: &[Message], response: &TurnResponse) -> Vec<Message> {
    let mut messages = before.to_vec();
    messages.push(Message::Assistant {
        content: response.content.clone(),
    });
    messages.push(Message::User {
        content: stored_ids(response)
            .into_iter()
            .map(|id| UserBlock::ToolResult {
                tool_use_id: id,
                content: "ok".into(),
                is_error: false,
            })
            .collect(),
    });
    messages
}

#[test]
fn an_id_that_cannot_be_stored_is_replaced_and_the_history_then_suits_anthropic() {
    let reply = calls_reply(vec![
        call_chunk(
            0,
            Some("functions.list_tables:0"),
            Some("list_tables"),
            "{\"a\":1}",
        ),
        call_chunk(1, Some("call:2"), Some("list_tables"), "{}"),
        call_chunk(2, Some(&"x".repeat(65)), Some("list_tables"), "{}"),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    let ids = stored_ids(&response);
    assert_eq!(ids.len(), 3);
    assert!(ids.iter().all(|id| fits(id)), "{ids:?}");
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );
    // The events say the stored id, never the server's.
    let started: Vec<_> = events
        .iter()
        .filter(|e| e.starts_with("ToolCallStarted"))
        .collect();
    assert_eq!(started.len(), 3);
    for (event, id) in started.iter().zip(&ids) {
        assert!(event.contains(id.as_str()), "{event}");
    }
    assert!(!events
        .iter()
        .any(|e| e.contains("functions.") || e.contains("call:2")));
    assert_eq!(response.content[0]["input"], json!({"a": 1}));

    // The conversation then goes to Anthropic: the ids are valid there, and the answers match.
    let messages = after_tool_turn(&[Message::user_text("go")], &response);
    let mut harness = Harness::new(vec![text_reply("fine").into()]);
    harness.conversation = serde_json::from_value(json!({
        "scope": {"connection_id": CONNECTION}, "context": null, "messages": messages}))
    .unwrap();
    harness.run("next").unwrap();
    let body = harness.transport.request(0).json();
    let mut used = Vec::new();
    for message in body["messages"].as_array().unwrap() {
        for block in message["content"].as_array().unwrap() {
            for field in ["id", "tool_use_id"] {
                if let Some(id) = block[field].as_str() {
                    assert!(fits(id), "{id}");
                    used.push(id.to_owned());
                }
            }
        }
    }
    assert_eq!(used.len(), 6);
    for id in &ids {
        assert_eq!(used.iter().filter(|u| *u == id).count(), 2);
    }
}

#[test]
fn an_id_that_the_conversation_already_has_is_replaced() {
    let first = calls_reply(vec![call_chunk(
        0,
        Some("call_1"),
        Some("list_tables"),
        "{}",
    )]);
    let rig1 = rig(Flavor::Compatible, vec![first.into()]);
    let (response, _) = ok(turn(&rig1, "m"));
    assert_eq!(stored_ids(&response), vec!["call_1"], "a fresh id is kept");

    let history = after_tool_turn(&[Message::user_text("go")], &response);
    let second = calls_reply(vec![
        call_chunk(0, Some("call_1"), Some("list_tables"), "{}"),
        // The same id again later in this response, on another index.
        call_chunk(1, Some("call_9"), Some("list_tables"), "{}"),
        call_chunk(2, Some("call_9"), Some("list_tables"), "{}"),
    ]);
    let rig2 = self::rig(Flavor::Compatible, vec![second.into()]);
    let result = run(&rig2, &request("m", &history, &[]));
    let ids = stored_ids(&result.0.unwrap());
    assert_eq!(ids.len(), 3);
    assert_ne!(ids[0], "call_1");
    assert_eq!(ids[1], "call_9");
    assert_ne!(ids[2], "call_9");
    assert!(ids.iter().all(|id| fits(id)));
}

#[test]
fn a_server_that_numbers_its_calls_the_same_every_turn_gets_different_stored_ids() {
    let reply = || calls_reply(vec![call_chunk(0, Some("0"), Some("list_tables"), "{}")]);
    let rig = rig(
        Flavor::Compatible,
        vec![reply().into(), reply().into(), reply().into()],
    );
    let mut messages = vec![Message::user_text("go")];
    let mut seen = Vec::new();
    for _ in 0..3 {
        let result = run(&rig, &request("m", &messages, &[]));
        let response = result.0.unwrap();
        seen.extend(stored_ids(&response));
        messages = after_tool_turn(&messages, &response);
        messages.push(Message::user_text("again"));
        messages.pop();
    }
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0], "0", "the first one fits and is new");
    assert_eq!(
        seen.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "{seen:?}"
    );
    assert!(seen.iter().all(|id| fits(id)));
}

#[test]
fn generated_ids_never_meet_the_ids_of_the_history_or_of_another_provider() {
    let nameless = || calls_reply(vec![call_chunk(0, None, Some("list_tables"), "{}")]);
    // Two provider instances, the second continuing the history of the first, as after a restart.
    let one = rig(Flavor::Ollama, vec![nameless().into()]);
    let first = ok(turn(&one, "m")).0;
    let history = after_tool_turn(&[Message::user_text("go")], &first);
    let two = self::rig(Flavor::Ollama, vec![nameless().into()]);
    let second = run(&two, &request("m", &history, &[])).0.unwrap();
    let (a, b) = (stored_ids(&first), stored_ids(&second));
    assert!(fits(&a[0]) && fits(&b[0]));
    assert_ne!(a, b);

    // A restart restarts the counter: put the next ids the counter would give into the history.
    let next = super::stream::next_generated_ids_for_test(300);
    let crowded: Vec<Message> = vec![
        Message::user_text("go"),
        Message::Assistant {
            content: next
                .iter()
                .map(|id| json!({"type": "tool_use", "id": id, "name": "list_tables", "input": {}}))
                .collect(),
        },
        Message::User {
            content: next
                .iter()
                .map(|id| UserBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: "ok".into(),
                    is_error: false,
                })
                .collect(),
        },
    ];
    let three = self::rig(Flavor::Ollama, vec![nameless().into()]);
    let third = run(&three, &request("m", &crowded, &[])).0.unwrap();
    assert!(!next.contains(&stored_ids(&third)[0]));
}

#[test]
fn the_same_id_on_two_indexes_is_two_calls_with_their_own_arguments() {
    let reply = calls_reply(vec![
        call_chunk(0, Some("dup"), Some("list_tables"), "{\"a\":"),
        call_chunk(1, Some("dup"), Some("execute_query"), "{\"b\":"),
        call_chunk(0, None, None, "1}"),
        call_chunk(1, None, None, "2}"),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, _) = ok(turn(&rig, "m"));
    let calls: Vec<_> = response
        .content
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        (calls[0]["name"].as_str(), &calls[0]["input"]),
        (Some("list_tables"), &json!({"a": 1}))
    );
    assert_eq!(
        (calls[1]["name"].as_str(), &calls[1]["input"]),
        (Some("execute_query"), &json!({"b": 2}))
    );
    assert_ne!(calls[0]["id"], calls[1]["id"]);
}

#[test]
fn a_tool_name_that_cannot_be_stored_is_kept_out_and_answered_as_not_run() {
    let reply = calls_reply(vec![
        call_chunk(0, Some("c1"), Some("functions.list_tables"), "{\"a\":1}"),
        call_chunk(1, Some("c2"), Some(&"n".repeat(65)), "{}"),
        call_chunk(2, Some("c3"), Some(FAKE_KEY), "{}"),
        call_chunk(3, Some("c4"), None, "{}"),
        call_chunk(4, Some("c5"), Some("list_tables"), "{}"),
    ]);
    let rig = rig(Flavor::OpenAi, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    let names: Vec<_> = response
        .content
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["invalid_tool_name"; 4]
            .into_iter()
            .chain(["list_tables"])
            .collect::<Vec<_>>()
    );
    assert!(response.content.iter().all(|b| b["input"] == json!({})));
    let invalid: Vec<_> = response
        .invalid_tool_inputs
        .iter()
        .map(|i| i.tool_use_id.as_str())
        .collect();
    assert_eq!(invalid, vec!["c1", "c2", "c3", "c4"]);
    assert!(response.invalid_tool_inputs[0]
        .raw
        .contains("functions.list_tables"));
    assert!(events
        .iter()
        .any(|e| e.contains("ToolInputReady") && e.contains("c1") && e.contains("valid: false")));
    assert!(!events.iter().any(|e| e.contains("functions.")));
    for text in events
        .iter()
        .chain(response.invalid_tool_inputs.iter().map(|i| &i.raw))
    {
        assert_no_key(text);
    }
    // The stored turn is valid for both providers.
    assert!(response
        .content
        .iter()
        .all(|b| fits(b["name"].as_str().unwrap())));
}

#[test]
fn the_tool_result_is_written_with_the_stored_id_on_both_sides() {
    let reply = calls_reply(vec![call_chunk(
        0,
        Some("functions.list_tables:0"),
        Some("list_tables"),
        "{}",
    )]);
    let rig1 = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, _) = ok(turn(&rig1, "m"));
    let id = stored_ids(&response).remove(0);
    let history = after_tool_turn(&[Message::user_text("go")], &response);
    let rig2 = self::rig(Flavor::Compatible, vec![complete(&[]).into()]);
    let _ = run(&rig2, &request("m", &history, &[]));
    let sent = rig2.transport.request(0).json();
    let messages = sent["messages"].as_array().unwrap();
    let call = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    let result = messages.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(call["tool_calls"][0]["id"], id.as_str());
    assert_eq!(result["tool_call_id"], id.as_str());
    assert!(fits(&id));
}

// ------------------------------------------------------------------------------- nits

#[test]
fn an_unknown_finish_reason_with_calls_still_runs_them() {
    for reason in ["tool_use", "eos", "stop", "end_turn"] {
        let reply = complete(&[
            call_chunk(0, Some("c1"), Some("list_tables"), "{}"),
            chunk(json!({}), Some(reason)),
        ]);
        let rig = rig(Flavor::Compatible, vec![reply.into()]);
        assert_eq!(
            ok(turn(&rig, "m")).0.stop_reason,
            StopReason::ToolUse,
            "{reason}"
        );
    }
    // Without calls an unknown reason stays what it is; the output limit wins over the calls.
    let reply = complete(&[text_chunk("x"), chunk(json!({}), Some("eos"))]);
    let rig1 = rig(Flavor::Compatible, vec![reply.into()]);
    assert_eq!(
        ok(turn(&rig1, "m")).0.stop_reason,
        StopReason::Other("eos".into())
    );
    let reply = complete(&[
        call_chunk(0, Some("c1"), Some("t"), "{"),
        chunk(json!({}), Some("length")),
    ]);
    let rig2 = self::rig(Flavor::Compatible, vec![reply.into()]);
    assert_eq!(ok(turn(&rig2, "m")).0.stop_reason, StopReason::MaxTokens);
}

#[test]
fn arguments_as_an_object_and_odd_numbers_do_not_lose_the_delta() {
    let reply = complete(&[
        // The input as an object, in one piece.
        chunk(
            json!({"tool_calls": [{"index": 0, "id": "c1",
                "function": {"name": "list_tables", "arguments": {"connection_id": "c1"}}}]}),
            None,
        ),
        // `index` as a float with an integer value, a string (ignored: index 0 is the first call).
        chunk(
            json!({"tool_calls": [{"index": 1.0, "id": "c2",
                "function": {"name": "list_tables", "arguments": "{\"x\":1}"}}]}),
            None,
        ),
        chunk(
            json!({"tool_calls": [{"index": "1", "function": {"arguments": ""}}]}),
            None,
        ),
        chunk(json!({}), Some("tool_calls")),
        json!({"choices": [], "usage": {"prompt_tokens": 12.0, "completion_tokens": 3.0,
            "prompt_tokens_details": {"cached_tokens": 2.5}}}),
    ]);
    let rig = rig(Flavor::Compatible, vec![reply.into()]);
    let (response, events) = ok(turn(&rig, "m"));
    assert_eq!(response.content[0]["input"], json!({"connection_id": "c1"}));
    assert_eq!(response.content[1]["input"], json!({"x": 1}));
    assert!(response.invalid_tool_inputs.is_empty());
    assert!(events
        .iter()
        .any(|e| e.contains("ToolInputReady") && e.contains("c1") && e.contains("valid: true")));
    // 12.0 and 3.0 are counts; 2.5 is not one and is ignored for that field only.
    assert_eq!(response.usage.input_tokens, 12);
    assert_eq!(response.usage.output_tokens, 3);
    assert_eq!(response.usage.cache_read_input_tokens, 0);
    // Wrong types elsewhere cost that field, not the chunk.
    let reply = complete(&[
        json!({"choices": [{"delta": {"content": "kept"}, "finish_reason": null}],
               "usage": {"prompt_tokens": "x", "completion_tokens": []}}),
        chunk(json!({}), Some("stop")),
    ]);
    let rig2 = self::rig(Flavor::Compatible, vec![reply.into()]);
    assert_eq!(ok(turn(&rig2, "m")).0.text(), "kept");
}

#[test]
fn an_address_whose_host_looks_like_a_path_suffix_keeps_its_host() {
    let cases = [
        (
            Flavor::Compatible,
            "https://chat/completions",
            "https://chat/completions/chat/completions",
            "https://chat/completions/models",
        ),
        (
            Flavor::OpenAi,
            "https://responses",
            "https://responses/v1/chat/completions",
            "https://responses/v1/models",
        ),
        (
            Flavor::Ollama,
            "https://models",
            "https://models/v1/chat/completions",
            "https://models/v1/models",
        ),
    ];
    for (flavor, base, chat, models) in cases {
        let rig = rig_with(
            config(flavor, base),
            Arc::new(FixedKey),
            vec![
                complete(&[]).into(),
                list_reply(&["m"]).into(),
                show_reply(&["completion"]).into(),
            ],
        );
        let _ = turn(&rig, "m");
        assert_eq!(rig.transport.request(0).url, chat, "{base}");
        let _ = rig.provider.list_models(&CancelToken::new()).unwrap();
        assert_eq!(rig.transport.request(1).url, models, "{base}");
        if flavor == Flavor::Ollama {
            assert_eq!(rig.transport.request(2).url, "https://models/api/show");
        }
    }
}
