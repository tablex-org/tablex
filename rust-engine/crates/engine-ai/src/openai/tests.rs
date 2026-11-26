//! The provider over a scripted transport: configuration, the key, the request body, retries,
//! cancellation, the end of a stream, and the model list. The pieces below the transport have
//! their own tests next to them (`endpoint`, `effort`, `stream`).

use super::*;
use crate::anthropic::{AnthropicConfig, AnthropicProvider};
use crate::conversation::{HistoryForm, UserBlock};
use crate::provider::{ContextBlock, Effort, ProviderError, StopReason, SystemPrompt, Usage};
use crate::test_support::{
    lock, End, FixedKey, Reply, ScriptedSleeper, ScriptedTransport, Step, FAKE_KEY,
};
use crate::transport::TransportError;
use engine_agent::ToolSpec;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;

// ------------------------------------------------------------------------------------- helpers

struct Keys {
    answer: Result<ApiKey, KeyError>,
    asked: AtomicUsize,
}

impl Keys {
    fn new(answer: Result<ApiKey, KeyError>) -> Arc<Self> {
        Arc::new(Self {
            answer,
            asked: AtomicUsize::new(0),
        })
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl KeySource for Keys {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.answer.clone()
    }
}

/// Keeps the token each request was sent with.
struct Spy {
    inner: Arc<ScriptedTransport>,
    tokens: Mutex<Vec<CancelToken>>,
}

impl HttpTransport for Spy {
    fn send(
        &self,
        request: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        lock(&self.tokens).push(cancel.clone());
        self.inner.send(request, cancel)
    }
}

struct Rig {
    transport: Arc<ScriptedTransport>,
    sleeper: Arc<ScriptedSleeper>,
    provider: OpenAiProvider,
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
    .with_sleeper(Arc::clone(&sleeper) as Arc<dyn Sleeper>);
    Rig {
        transport,
        sleeper,
        provider,
    }
}

fn rig(flavor: Flavor, base_url: &str, steps: Vec<Step>) -> Rig {
    rig_with(config(flavor, base_url), Arc::new(FixedKey), steps)
}

fn openai(steps: Vec<Step>) -> Rig {
    rig(Flavor::OpenAi, OPENAI_BASE_URL, steps)
}

fn no_key() -> Arc<Keys> {
    Keys::new(Err(KeyError::NotSet))
}

// SSE fixtures: the chunks of a Chat Completions stream.

fn sse(chunks: &[Value]) -> String {
    chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect()
}

fn text_chunk(text: &str) -> Value {
    json!({"id": "c1", "object": "chat.completion.chunk", "model": "gpt-4o-2024",
           "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]})
}

fn finish_chunk(reason: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": reason}]})
}

fn usage_chunk(prompt: u64, completion: u64, cached: u64) -> Value {
    json!({"choices": [], "usage": {"prompt_tokens": prompt, "completion_tokens": completion,
           "prompt_tokens_details": {"cached_tokens": cached}}})
}

fn tool_chunk(index: u64, id: &str, name: &str, arguments: &str) -> Value {
    json!({"choices": [{"index": 0, "delta": {"tool_calls": [{
        "index": index, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments}}]}, "finish_reason": null}]})
}

/// A complete turn: text, the finish reason, the usage, `[DONE]`.
fn full_turn(text: &str) -> String {
    sse(&[
        text_chunk(text),
        finish_chunk("stop"),
        usage_chunk(100, 7, 40),
    ]) + "data: [DONE]\n\n"
}

fn reply(text: &str) -> Reply {
    Reply::ok(full_turn(text))
}

// Requests.

fn request<'a>(
    model: &'a str,
    messages: &'a [Message],
    tools: &'a [ToolSpec],
) -> TurnRequest<'a> {
    TurnRequest {
        model,
        effort: Effort::Medium,
        system: SystemPrompt {
            core: "CORE",
            context: None,
        },
        tools,
        messages,
        history: HistoryForm::default(),
    }
}

struct Outcome {
    result: Result<TurnResponse, ProviderError>,
    events: Vec<String>,
}

impl Outcome {
    fn ok(&self) -> &TurnResponse {
        self.result.as_ref().expect("the turn succeeds")
    }

    fn err(&self) -> &ProviderError {
        self.result.as_ref().expect_err("the turn fails")
    }

    /// `{:?}` of everything the turn produced.
    fn everything(&self) -> String {
        format!("{:?} {:?}", self.result, self.events)
    }
}

fn ask_in(provider: &OpenAiProvider, request: &TurnRequest<'_>, cancel: &CancelToken) -> Outcome {
    let mut events = Vec::new();
    let result = provider.stream_turn(request, &mut |e| events.push(format!("{e:?}")), cancel);
    Outcome { result, events }
}

fn ask(provider: &OpenAiProvider, messages: &[Message]) -> Outcome {
    ask_in(
        provider,
        &request("gpt-4o", messages, &[]),
        &CancelToken::new(),
    )
}

fn hi() -> Vec<Message> {
    vec![Message::user_text("hi")]
}

fn spec(name: &'static str) -> ToolSpec {
    ToolSpec {
        name,
        description: "Lists tables.",
        input_schema: json!({"type": "object", "properties": {"connection_id": {"type": "string"}}}),
        output_schema: json!({}),
        example: json!({}),
    }
}

fn body_of(transport: &ScriptedTransport, index: usize) -> String {
    String::from_utf8(transport.request(index).body).unwrap()
}

fn json_body(transport: &ScriptedTransport, index: usize) -> Value {
    transport.request(index).json()
}

fn assert_protocol(error: &ProviderError, want: &ProtocolError) {
    match error {
        ProviderError::Protocol { source, .. } => assert_eq!(source, want),
        other => panic!("expected {want:?}, got {other:?}"),
    }
}

// ------------------------------------------------------------------- flavors and configuration

#[test]
fn flavors_know_their_address_and_whether_they_need_a_key() {
    assert_eq!(Flavor::OpenAi.default_base_url(), Some("https://api.openai.com/v1"));
    assert_eq!(Flavor::Ollama.default_base_url(), Some("http://localhost:11434/v1"));
    assert_eq!(Flavor::Compatible.default_base_url(), None);
    assert!(Flavor::OpenAi.requires_key());
    assert!(!Flavor::Compatible.requires_key());
    assert!(!Flavor::Ollama.requires_key());
    let as_path: fn(Flavor) -> bool = Flavor::requires_key;
    assert!(as_path(Flavor::OpenAi));
}

#[test]
fn a_config_starts_from_the_flavor() {
    for flavor in [Flavor::OpenAi, Flavor::Compatible, Flavor::Ollama] {
        let config = OpenAiConfig::for_flavor(flavor);
        assert_eq!(config.flavor, flavor);
        assert_eq!(config.base_url, flavor.default_base_url().unwrap_or(""));
        assert_eq!(config.max_tokens, 4096);
        assert!(config.enable_thinking);
        assert_eq!(config.retry, RetryPolicy::default());
        assert_eq!(config.timeouts, Timeouts::default());
    }
    let default = OpenAiConfig::default();
    assert_eq!(default.flavor, Flavor::OpenAi);
    assert_eq!(default.base_url, OPENAI_BASE_URL);
}

#[test]
fn validation_gives_what_the_anthropic_provider_gives() {
    let inputs = [
        "https://api.openai.com/v1",
        "  https://api.openai.com/v1///  ",
        "",
        "   ",
        "/",
        "localhost:11434",
        "api.openai.com",
        "http://localhost:11434/v1",
        "http://127.0.0.1:1234/v1/",
        "http://[::1]:8080",
        "http://example.com/v1",
        "http://10.0.0.5:8000",
        "https://user:pass@example.com/v1",
        "https://user@example.com",
        "https://example.com/v1?key=1",
        "https://example.com/v1#frag",
        "ftp://example.com",
        "https://",
        "https:///v1",
        "https://exa mple.com",
        "HTTPS://Example.com",
        "HTTP://LOCALHOST:1",
    ];
    for input in inputs {
        for flavor in [Flavor::OpenAi, Flavor::Compatible, Flavor::Ollama] {
            let ours = config(flavor, input).validate();
            let theirs = AnthropicConfig {
                base_url: input.to_owned(),
                ..AnthropicConfig::default()
            }
            .validate();
            assert_eq!(ours, theirs, "{input:?}");
        }
    }
    assert_eq!(config(Flavor::Compatible, "").validate(), Err(ConfigError::InvalidBaseUrl));
    assert_eq!(
        config(Flavor::Compatible, "http://example.com").validate(),
        Err(ConfigError::InsecureBaseUrl)
    );
    assert_eq!(
        config(Flavor::OpenAi, "https://u:p@example.com").validate(),
        Err(ConfigError::BaseUrlHasUserinfo)
    );
    assert_eq!(
        config(Flavor::OpenAi, "https://example.com/?x").validate(),
        Err(ConfigError::BaseUrlHasQueryOrFragment)
    );
    assert_eq!(config(Flavor::Ollama, "http://localhost:11434/v1").validate(), Ok(()));
}

#[test]
fn a_provider_keeps_the_base_url_trimmed_and_refuses_a_bad_one() {
    let transport = ScriptedTransport::new(vec![]);
    let make = |url: &str| {
        OpenAiProvider::new(
            Arc::clone(&transport) as Arc<dyn HttpTransport>,
            Arc::new(FixedKey),
            config(Flavor::Compatible, url),
        )
    };
    let provider = make("  https://llm.example/v1///\n").unwrap();
    assert_eq!(provider.config().base_url, "https://llm.example/v1");
    assert!(matches!(make("http://llm.example"), Err(ConfigError::InsecureBaseUrl)));
    assert!(matches!(make(""), Err(ConfigError::InvalidBaseUrl)));
    assert_eq!(transport.request_count(), 0);
}

#[test]
fn the_config_debug_output_has_no_key() {
    let rig = openai(vec![reply("x").into()]);
    ask(&rig.provider, &hi());
    let shown = format!("{:?}", rig.provider.config());
    assert!(!shown.contains(FAKE_KEY) && !shown.contains("ZZSECRETZZ"), "{shown}");
}

#[test]
fn each_flavor_asks_the_urls_derived_from_its_base() {
    let cases = [
        (Flavor::OpenAi, "https://api.openai.com/v1", "https://api.openai.com/v1/chat/completions", "https://api.openai.com/v1/models"),
        (Flavor::OpenAi, "https://api.openai.com", "https://api.openai.com/v1/chat/completions", "https://api.openai.com/v1/models"),
        (Flavor::OpenAi, "https://api.openai.com/v1/responses", "https://api.openai.com/v1/chat/completions", "https://api.openai.com/v1/models"),
        (Flavor::Compatible, "http://127.0.0.1:1234/v1", "http://127.0.0.1:1234/v1/chat/completions", "http://127.0.0.1:1234/v1/models"),
        (Flavor::Compatible, "http://localhost:8080", "http://localhost:8080/v1/chat/completions", "http://localhost:8080/v1/models"),
        (Flavor::Compatible, "https://gw.example/api/", "https://gw.example/api/chat/completions", "https://gw.example/api/models"),
        (Flavor::Compatible, "https://gw.example/v2/chat/completions", "https://gw.example/v2/chat/completions", "https://gw.example/v2/models"),
        (Flavor::Ollama, "http://localhost:11434/v1", "http://localhost:11434/v1/chat/completions", "http://localhost:11434/v1/models"),
    ];
    for (flavor, base, chat, models) in cases {
        let steps = vec![reply("x").into(), Reply::ok(json!({"data": []}).to_string()).into()];
        let rig = rig(flavor, base, steps);
        ask(&rig.provider, &hi()).ok();
        rig.provider.list_models(&CancelToken::new()).unwrap();
        assert_eq!(rig.transport.request(0).url, chat, "{base}");
        assert_eq!(rig.transport.request(0).method, "POST");
        assert_eq!(rig.transport.request(1).url, models, "{base}");
        assert_eq!(rig.transport.request(1).method, "GET");
    }
}

// ------------------------------------------------------------------------- key and run sessions

#[test]
fn openai_without_a_key_or_with_an_empty_one_sends_nothing() {
    let cases = [
        (Err(KeyError::NotSet), KeyError::NotSet),
        (Err(KeyError::Empty), KeyError::Empty),
        (Ok(ApiKey::new("")), KeyError::Empty),
        (Ok(ApiKey::new(" \n\t ")), KeyError::Empty),
    ];
    for (answer, want) in cases {
        let rig = rig_with(
            config(Flavor::OpenAi, OPENAI_BASE_URL),
            Keys::new(answer),
            vec![reply("x").into()],
        );
        let turn = ask(&rig.provider, &hi());
        assert!(matches!(turn.err(), ProviderError::Key(k) if *k == want), "{:?}", turn.err());
        assert!(matches!(rig.provider.begin_run(), Err(ProviderError::Key(k)) if k == want));
        let listed = rig.provider.list_models(&CancelToken::new());
        assert!(matches!(listed, Err(ProviderError::Key(k)) if k == want));
        assert_eq!(rig.transport.request_count(), 0);
    }
}

#[test]
fn compatible_and_ollama_work_without_a_key_and_send_no_credentials() {
    for flavor in [Flavor::Compatible, Flavor::Ollama] {
        for answer in [Err(KeyError::NotSet), Err(KeyError::Empty), Ok(ApiKey::new("  "))] {
            let steps = vec![reply("x").into(), Reply::ok(json!({"data": [{"id": "m"}]}).to_string()).into()];
            let rig = rig_with(config(flavor, "http://localhost:9/v1"), Keys::new(answer), steps);
            assert_eq!(ask(&rig.provider, &hi()).ok().content.len(), 1);
            // Ollama probes the model: the script has no answer, which keeps it.
            let models = rig.provider.list_models(&CancelToken::new()).unwrap();
            assert_eq!(models.len(), 1);
            for index in 0..rig.transport.request_count() {
                let sent = rig.transport.request(index);
                assert!(sent.header("authorization").is_none(), "{flavor:?}");
                assert!(sent.header("x-api-key").is_none());
            }
        }
    }
}

#[test]
fn any_other_key_error_is_always_returned() {
    for flavor in [Flavor::OpenAi, Flavor::Compatible, Flavor::Ollama] {
        let rig = rig_with(
            config(flavor, "http://localhost:9/v1"),
            Keys::new(Err(KeyError::Unavailable("locked".into()))),
            vec![reply("x").into()],
        );
        let error = ask(&rig.provider, &hi());
        assert!(
            matches!(error.err(), ProviderError::Key(KeyError::Unavailable(_))),
            "{flavor:?}"
        );
        assert!(matches!(
            rig.provider.list_models(&CancelToken::new()),
            Err(ProviderError::Key(KeyError::Unavailable(_)))
        ));
        assert_eq!(rig.transport.request_count(), 0);
    }
}

#[test]
fn a_key_goes_out_as_one_secret_bearer_header_to_every_flavor() {
    for flavor in [Flavor::OpenAi, Flavor::Compatible, Flavor::Ollama] {
        let steps = vec![reply("x").into(), Reply::ok(json!({"data": []}).to_string()).into()];
        let rig = rig(flavor, "http://localhost:9/v1", steps);
        // Plain http to localhost is fine for every flavor.
        ask(&rig.provider, &hi()).ok();
        rig.provider.list_models(&CancelToken::new()).unwrap();
        let (chat, list) = (rig.transport.request(0), rig.transport.request(1));
        for sent in [&chat, &list] {
            assert_eq!(sent.header("authorization"), Some(format!("Bearer {FAKE_KEY}").as_str()));
            assert!(sent.header("x-api-key").is_none());
            assert!(!sent.debug.contains("ZZSECRETZZ"), "{}", sent.debug);
            assert!(sent.debug.contains("***"));
        }
        assert_eq!(chat.header("content-type"), Some("application/json"));
        assert!(list.header("content-type").is_none());
        assert!(list.body.is_empty());
    }
}

#[test]
fn a_run_asks_the_key_once_for_all_its_turns() {
    let keys = Keys::new(Ok(ApiKey::new(FAKE_KEY)));
    let rig = rig_with(
        config(Flavor::OpenAi, OPENAI_BASE_URL),
        Arc::clone(&keys) as Arc<dyn KeySource>,
        vec![reply("a").into(), reply("b").into(), reply("c").into()],
    );
    let messages = hi();
    let run = rig.provider.begin_run().unwrap();
    let cancel = CancelToken::new();
    for _ in 0..2 {
        rig.provider
            .stream_turn_in_run(&run, &request("gpt-4o", &messages, &[]), &mut |_| {}, &cancel)
            .unwrap();
    }
    assert_eq!(keys.asked(), 1);
    ask(&rig.provider, &messages).ok();
    assert_eq!(keys.asked(), 2, "stream_turn is begin_run and a turn");
}

#[test]
fn a_run_session_of_another_provider_is_refused_without_a_request() {
    let first = openai(vec![reply("a").into()]);
    let second = openai(vec![reply("b").into()]);
    let anthropic = AnthropicProvider::new(
        Arc::clone(&second.transport) as Arc<dyn HttpTransport>,
        Arc::new(FixedKey),
        AnthropicConfig::default(),
    )
    .unwrap();
    let messages = hi();
    let cancel = CancelToken::new();
    for session in [
        first.provider.begin_run().unwrap(),
        anthropic.begin_run().unwrap(),
        RunSession::default(),
    ] {
        let error = second
            .provider
            .stream_turn_in_run(&session, &request("gpt-4o", &messages, &[]), &mut |_| {}, &cancel)
            .unwrap_err();
        assert!(matches!(error, ProviderError::ForeignRunSession), "{error:?}");
    }
    assert_eq!(second.transport.request_count(), 0);
    // Its own session works.
    let own = second.provider.begin_run().unwrap();
    second
        .provider
        .stream_turn_in_run(&own, &request("gpt-4o", &messages, &[]), &mut |_| {}, &cancel)
        .unwrap();
}

// ------------------------------------------------------------------------------------ the body

fn golden_conversation() -> Vec<Message> {
    vec![
        Message::user_text("list tables"),
        Message::System {
            content: "note one".into(),
        },
        Message::Assistant {
            content: vec![
                json!({"type": "reasoning", "text": "SECRET-REASONING"}),
                json!({"type": "thinking", "thinking": "SECRET-THINKING", "signature": "SIG"}),
                json!({"type": "redacted_thinking", "data": "SECRET-REDACTED"}),
                json!({"type": "text", "text": "Looking."}),
                json!({"type": "tool_use", "id": "call_1", "name": "list_tables",
                       "input": {"connection_id": "c1"}}),
            ],
        },
        Message::User {
            content: vec![
                UserBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "[\"users\"]".into(),
                    is_error: true,
                },
                UserBlock::Text {
                    text: "thanks".into(),
                },
            ],
        },
        Message::Assistant {
            content: vec![json!({"type": "text", "text": "Done"})],
        },
        Message::user_text("next"),
    ]
}

#[test]
fn the_body_has_a_fixed_key_order_and_the_documented_message_shapes() {
    let rig = openai(vec![reply("x").into()]);
    let messages = golden_conversation();
    let tools = [spec("list_tables")];
    let mut turn = request("gpt-4o", &messages, &tools);
    turn.system.context = Some(ContextBlock {
        text: "CTX",
        cacheable: true,
    });
    ask_in(&rig.provider, &turn, &CancelToken::new()).ok();
    let expected = concat!(
        r#"{"model":"gpt-4o","messages":["#,
        r#"{"role":"system","content":"CORE\n\nCTX"},"#,
        r#"{"role":"user","content":"list tables\n\n<system-reminder>\nnote one\n</system-reminder>"},"#,
        r#"{"role":"assistant","content":"Looking.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"list_tables","arguments":"{\"connection_id\":\"c1\"}"}}]},"#,
        r#"{"role":"tool","tool_call_id":"call_1","content":"[\"users\"]"},"#,
        r#"{"role":"user","content":"thanks"},"#,
        r#"{"role":"assistant","content":"Done"},"#,
        r#"{"role":"user","content":"next"}],"#,
        r#""tools":[{"type":"function","function":{"name":"list_tables","description":"Lists tables.","parameters":{"type":"object","properties":{"connection_id":{"type":"string"}}}}}],"#,
        r#""tool_choice":"auto","stream":true,"stream_options":{"include_usage":true},"max_tokens":4096}"#,
    );
    assert_eq!(body_of(&rig.transport, 0), expected);
}

#[test]
fn no_trace_of_reasoning_is_sent_back() {
    let rig = openai(vec![reply("x").into()]);
    ask(&rig.provider, &golden_conversation()).ok();
    let body = body_of(&rig.transport, 0);
    for secret in ["SECRET-REASONING", "SECRET-THINKING", "SECRET-REDACTED", "reasoning", "thinking", "SIG"] {
        assert!(!body.contains(secret), "{secret}");
    }
}

#[test]
fn a_request_without_tools_has_neither_tools_nor_tool_choice() {
    let rig = openai(vec![reply("x").into()]);
    ask(&rig.provider, &hi()).ok();
    let body = json_body(&rig.transport, 0);
    assert!(body.get("tools").is_none() && body.get("tool_choice").is_none());
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(body["messages"], json!([
        {"role": "system", "content": "CORE"}, {"role": "user", "content": "hi"}]));
}

#[test]
fn the_system_message_is_the_core_alone_or_the_core_and_the_context() {
    let rig = openai(vec![reply("x").into(), reply("x").into(), reply("x").into()]);
    let messages = hi();
    let cases = [(None, "CORE"), (Some(false), "CORE\n\nCTX"), (Some(true), "CORE\n\nCTX")];
    for (index, (context, want)) in cases.into_iter().enumerate() {
        let mut turn = request("gpt-4o", &messages, &[]);
        turn.system.context = context.map(|cacheable| ContextBlock { text: "CTX", cacheable });
        ask_in(&rig.provider, &turn, &CancelToken::new()).ok();
        assert_eq!(json_body(&rig.transport, index)["messages"][0]["content"], want);
    }
}

#[test]
fn tool_results_become_tool_messages_before_the_user_text() {
    let rig = openai(vec![reply("x").into()]);
    let messages = vec![
        Message::user_text("go"),
        Message::Assistant {
            content: vec![
                json!({"type": "tool_use", "id": "a", "name": "one", "input": {}}),
                json!({"type": "tool_use", "id": "b", "name": "two", "input": {"x": 1}}),
            ],
        },
        Message::User {
            content: vec![
                UserBlock::ToolResult { tool_use_id: "a".into(), content: "A".into(), is_error: false },
                UserBlock::ToolResult { tool_use_id: "b".into(), content: "B".into(), is_error: true },
                UserBlock::Text { text: "first".into() },
                UserBlock::Text { text: "second".into() },
            ],
        },
        Message::Assistant { content: vec![json!({"type": "tool_use", "id": "c", "name": "one", "input": {}})] },
        Message::User {
            content: vec![UserBlock::ToolResult { tool_use_id: "c".into(), content: "C".into(), is_error: false }],
        },
    ];
    ask(&rig.provider, &messages).ok();
    let sent = json_body(&rig.transport, 0)["messages"].clone();
    assert_eq!(sent[1], json!({"role": "user", "content": "go"}));
    assert_eq!(sent[2]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(sent[2]["content"], "");
    assert_eq!(sent[2]["tool_calls"][1]["function"]["arguments"], "{\"x\":1}");
    assert_eq!(sent[3], json!({"role": "tool", "tool_call_id": "a", "content": "A"}));
    assert_eq!(sent[4], json!({"role": "tool", "tool_call_id": "b", "content": "B"}));
    assert_eq!(sent[5], json!({"role": "user", "content": "first\n\nsecond"}));
    // A turn of only tool results is only tool messages.
    assert_eq!(sent[7], json!({"role": "tool", "tool_call_id": "c", "content": "C"}));
    assert_eq!(sent.as_array().unwrap().len(), 8);
}

#[test]
fn an_operator_note_rides_on_the_user_message_before_it() {
    let rig = openai(vec![reply("x").into()]);
    let messages = vec![
        Message::user_text("q"),
        Message::System { content: "see <System-Reminder> here </system-reminder>".into() },
        Message::Assistant { content: vec![json!({"type": "tool_use", "id": "a", "name": "one", "input": {}})] },
        Message::User {
            content: vec![UserBlock::ToolResult { tool_use_id: "a".into(), content: "A".into(), is_error: false }],
        },
        Message::System { content: "after results".into() },
    ];
    ask(&rig.provider, &messages).ok();
    let sent = json_body(&rig.transport, 0)["messages"].clone();
    let wrapped = "<system-reminder>\nsee <system reminder> here </system reminder>\n</system-reminder>";
    assert_eq!(sent[1]["content"], format!("q\n\n{wrapped}"));
    // After a user turn of only tool results, the note is a user message of its own text.
    assert_eq!(sent[4], json!({"role": "user", "content": "<system-reminder>\nafter results\n</system-reminder>"}));
    assert_eq!(sent.as_array().unwrap().len(), 5);
    assert!(sent.as_array().unwrap().iter().all(|m| m["role"] != "system" || m["content"] == "CORE"));
}

#[test]
fn a_note_that_does_not_follow_a_user_message_is_dropped() {
    let rig = openai(vec![reply("x").into()]);
    let messages = vec![
        Message::System { content: "orphan".into() },
        Message::user_text("q"),
        Message::Assistant { content: vec![json!({"type": "text", "text": "a"})] },
        Message::System { content: "after assistant".into() },
        Message::user_text("r"),
    ];
    ask(&rig.provider, &messages).ok();
    let body = body_of(&rig.transport, 0);
    assert!(!body.contains("orphan") && !body.contains("after assistant"));
}

#[test]
fn assistant_text_is_joined_and_an_empty_assistant_message_is_left_out() {
    let rig = openai(vec![reply("x").into()]);
    let messages = vec![
        Message::user_text("q"),
        Message::Assistant {
            content: vec![
                json!({"type": "text", "text": "one"}),
                json!({"type": "thinking", "thinking": "t"}),
                json!({"type": "text", "text": "two"}),
            ],
        },
        Message::user_text("r"),
        Message::Assistant { content: vec![json!({"type": "thinking", "thinking": "only"})] },
        Message::user_text("s"),
    ];
    ask(&rig.provider, &messages).ok();
    let sent = json_body(&rig.transport, 0)["messages"].clone();
    assert_eq!(sent[2], json!({"role": "assistant", "content": "one\ntwo"}));
    assert!(sent[2].get("tool_calls").is_none());
    let roles: Vec<_> = sent.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["system", "user", "assistant", "user", "user"]);
}

#[test]
fn the_history_form_is_not_consulted() {
    let rig = openai(vec![reply("x").into(), reply("x").into()]);
    let messages = golden_conversation();
    let plain = request("gpt-4o", &messages, &[]);
    let mut odd = plain;
    odd.history = HistoryForm { drop_thinking_before: 99, notes_in_user_turn: true };
    ask_in(&rig.provider, &plain, &CancelToken::new()).ok();
    ask_in(&rig.provider, &odd, &CancelToken::new()).ok();
    assert_eq!(body_of(&rig.transport, 0), body_of(&rig.transport, 1));
}

#[test]
fn the_token_limit_and_the_thinking_field_close_the_body() {
    let cases = [
        (Flavor::OpenAi, OPENAI_BASE_URL, true, "o4-mini", r#""max_completion_tokens":4096,"reasoning_effort":"medium"}"#),
        (Flavor::OpenAi, OPENAI_BASE_URL, true, "gpt-4o", r#""max_tokens":4096}"#),
        (Flavor::OpenAi, OPENAI_BASE_URL, false, "o4-mini", r#""max_completion_tokens":4096}"#),
        (Flavor::Compatible, "http://localhost:8080/v1", true, "gpt-5", r#""max_tokens":4096}"#),
        (Flavor::Compatible, "https://api.openai.com/v1", true, "gpt-5", r#""max_completion_tokens":4096}"#),
        (Flavor::Compatible, "http://localhost:8080/v1", false, "qwen3", r#""max_tokens":4096,"extra_body":{"chat_template_kwargs":{"enable_thinking":false}}}"#),
        (Flavor::Compatible, "http://localhost:8080/v1", false, "kimi-k2.5", r#""max_tokens":4096}"#),
        (Flavor::Ollama, OLLAMA_BASE_URL, true, "qwen3:0.6b", r#""max_tokens":4096,"reasoning_effort":"high"}"#),
        (Flavor::Ollama, OLLAMA_BASE_URL, false, "llama3.2", r#""max_tokens":4096,"reasoning_effort":"none"}"#),
    ];
    for (flavor, base, thinking, model, tail) in cases {
        let rig = rig_with(
            OpenAiConfig { enable_thinking: thinking, max_tokens: 4096, ..config(flavor, base) },
            Arc::new(FixedKey),
            vec![reply("x").into()],
        );
        let messages = hi();
        ask_in(&rig.provider, &request(model, &messages, &[]), &CancelToken::new()).ok();
        let body = body_of(&rig.transport, 0);
        assert!(
            body.ends_with(&format!(r#""stream_options":{{"include_usage":true}},{tail}"#)),
            "{flavor:?} {model}: {body}"
        );
    }
}

#[test]
fn the_configured_token_limit_is_sent() {
    let rig = rig_with(
        OpenAiConfig { max_tokens: 777, ..config(Flavor::OpenAi, OPENAI_BASE_URL) },
        Arc::new(FixedKey),
        vec![reply("x").into()],
    );
    ask(&rig.provider, &hi()).ok();
    assert_eq!(json_body(&rig.transport, 0)["max_tokens"], 777);
}

// ------------------------------------------------------------------------------------ one turn

#[test]
fn a_turn_streams_events_and_returns_content_usage_model_and_request_id() {
    let rig = openai(vec![Reply::ok(
        sse(&[
            text_chunk("Hel"),
            text_chunk("lo"),
            finish_chunk("stop"),
            usage_chunk(100, 7, 40),
        ]) + "data: [DONE]\n\n",
    )
    .header("x-request-id", "req_from_x")
    .into()]);
    let turn = ask(&rig.provider, &hi());
    let response = turn.ok();
    assert_eq!(turn.events, ["TextDelta(\"Hel\")", "TextDelta(\"lo\")"]);
    assert_eq!(response.content, [json!({"type": "text", "text": "Hello"})]);
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.usage,
        Usage { input_tokens: 60, output_tokens: 7, cache_read_input_tokens: 40, cache_creation_input_tokens: 0 }
    );
    assert_eq!(response.model.as_deref(), Some("gpt-4o-2024"));
    // `x-request-id` wins over `request-id` (the script sets both).
    assert_eq!(response.request_id.as_deref(), Some("req_from_x"));
    assert!(!response.served_by_fallback && response.fallback_to.is_none());
    assert_eq!(response.thinking_dropped, 0);
    assert!(response.invalid_tool_inputs.is_empty());
}

#[test]
fn the_request_id_falls_back_to_request_id() {
    let rig = openai(vec![reply("x").into()]);
    assert_eq!(ask(&rig.provider, &hi()).ok().request_id.as_deref(), Some("req_scripted"));
}

#[test]
fn a_tool_turn_reports_started_and_ready_and_keeps_calls_in_order() {
    let rig = openai(vec![Reply::ok(
        sse(&[
            json!({"choices": [{"delta": {"reasoning_content": "think "}}]}),
            json!({"choices": [{"delta": {"reasoning_content": "hard"}}]}),
            text_chunk("Let me look."),
            tool_chunk(0, "call_a", "list_tables", "{\"connection_id\":"),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "\"c1\"}"}}]}}]}),
            tool_chunk(1, "call_b", "describe_table", "{broken"),
            finish_chunk("tool_calls"),
        ]) + "data: [DONE]\n\n",
    )
    .into()]);
    let turn = ask(&rig.provider, &hi());
    let response = turn.ok();
    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.content,
        [
            json!({"type": "reasoning", "text": "think hard"}),
            json!({"type": "text", "text": "Let me look."}),
            json!({"type": "tool_use", "id": "call_a", "name": "list_tables", "input": {"connection_id": "c1"}}),
            json!({"type": "tool_use", "id": "call_b", "name": "describe_table", "input": {}}),
        ]
    );
    assert_eq!(
        response.invalid_tool_inputs,
        [crate::provider::InvalidToolInput { tool_use_id: "call_b".into(), raw: "{broken".into() }]
    );
    assert_eq!(turn.events[0], "ThinkingDelta(\"think \")");
    let started: Vec<_> = turn.events.iter().filter(|e| e.starts_with("ToolCallStarted")).collect();
    assert_eq!(started.len(), 2);
    assert!(turn.events.last().unwrap().contains("valid: false"));
}

#[test]
fn a_server_that_repeats_an_id_gets_the_plain_id_once_and_generated_ones_after() {
    let call = || {
        Reply::ok(
            sse(&[tool_chunk(0, "0", "list_tables", "{}"), finish_chunk("tool_calls")])
                + "data: [DONE]\n\n",
        )
    };
    let rig = openai(vec![call().into(), call().into(), call().into()]);
    let mut messages = hi();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let turn = ask(&rig.provider, &messages);
        let content = turn.ok().content.clone();
        let id = content[0]["id"].as_str().unwrap().to_owned();
        messages.push(Message::Assistant { content });
        messages.push(Message::User {
            content: vec![UserBlock::ToolResult { tool_use_id: id.clone(), content: "ok".into(), is_error: false }],
        });
        ids.push(id);
    }
    assert_eq!(ids[0], "0");
    assert!(ids[1].starts_with("call_") && ids[2].starts_with("call_"));
    assert_ne!(ids[1], ids[2]);
    // A result is written with the id its call carries.
    let last = json_body(&rig.transport, 2)["messages"].clone();
    let calls: Vec<_> = last.as_array().unwrap().iter().filter_map(|m| m["tool_calls"][0]["id"].as_str()).collect();
    let results: Vec<_> = last.as_array().unwrap().iter().filter_map(|m| m["tool_call_id"].as_str()).collect();
    assert_eq!(calls, ["0", ids[1].as_str()]);
    assert_eq!(calls, results);
}

#[test]
fn the_result_does_not_depend_on_chunking_or_line_endings() {
    let stream = full_turn("héllo 日本");
    let variants = [
        stream.clone(),
        stream.replace('\n', "\r\n"),
        stream.replace('\n', "\r"),
    ];
    let mut results = Vec::new();
    for (index, variant) in variants.iter().enumerate() {
        for size in [1usize, 2, 3, 7, 64, 10_000] {
            let rig = openai(vec![Reply::ok(variant).chunked(size).into()]);
            let turn = ask(&rig.provider, &hi());
            results.push((format!("{:?}", turn.ok()), turn.events.clone(), index, size));
        }
    }
    let (first, events, ..) = results[0].clone();
    for (result, evs, index, size) in &results {
        assert_eq!(result, &first, "variant {index} chunked by {size}");
        assert_eq!(evs, &events, "variant {index} chunked by {size}");
    }
}

#[test]
fn nothing_after_done_is_read() {
    let body = full_turn("a") + &sse(&[text_chunk("never")]);
    let rig = openai(vec![Reply::ok(body).into()]);
    let turn = ask(&rig.provider, &hi());
    assert_eq!(turn.ok().content, [json!({"type": "text", "text": "a"})]);
}

#[test]
fn a_body_that_ends_without_done_or_a_finish_reason_is_truncated_and_not_sent_again() {
    let bodies = [
        String::new(),
        sse(&[text_chunk("partial")]),
        ": only a comment\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"cut".to_owned(),
    ];
    for body in bodies {
        let rig = openai(vec![Reply::ok(body).into(), reply("again").into()]);
        let turn = ask(&rig.provider, &hi());
        assert_protocol(turn.err(), &ProtocolError::Truncated);
        assert_eq!(rig.transport.request_count(), 1);
        assert_eq!(rig.transport.steps_left(), 1);
    }
}

#[test]
fn a_finish_reason_ends_the_turn_without_done_when_the_body_ends() {
    let spy = Arc::new(Spy {
        inner: ScriptedTransport::new(vec![Reply::ok(sse(&[
            text_chunk("a"),
            finish_chunk("stop"),
            usage_chunk(5, 2, 0),
        ]))
        .into()]),
        tokens: Mutex::default(),
    });
    let provider = OpenAiProvider::new(
        Arc::clone(&spy) as Arc<dyn HttpTransport>,
        Arc::new(FixedKey),
        config(Flavor::OpenAi, OPENAI_BASE_URL),
    )
    .unwrap();
    let caller = CancelToken::new();
    let messages = hi();
    let turn = ask_in(&provider, &request("gpt-4o", &messages, &[]), &caller);
    assert_eq!(turn.ok().usage.output_tokens, 2);
    assert_eq!(turn.ok().stop_reason, StopReason::EndTurn);
    // The connection is let go once the turn is complete; the caller's token is not touched.
    assert!(lock(&spy.tokens)[0].is_cancelled());
    assert!(!caller.is_cancelled());
}

#[test]
fn a_server_that_keeps_the_connection_open_after_the_finish_reason_is_let_go_after_the_grace() {
    for _ in 0..5 {
        let head = sse(&[text_chunk("answer"), finish_chunk("stop")]);
        let tail = sse(&[usage_chunk(30, 4, 10)]);
        let spy = Arc::new(Spy {
            inner: ScriptedTransport::new(vec![Reply::ok(format!("{head}{tail}"))
                .split_at(head.len())
                .end(End::BlockUntilCancel(None))
                .into()]),
            tokens: Mutex::default(),
        });
        let provider = OpenAiProvider::new(
            Arc::clone(&spy) as Arc<dyn HttpTransport>,
            Arc::new(FixedKey),
            config(Flavor::OpenAi, OPENAI_BASE_URL),
        )
        .unwrap()
        .with_finish_grace(Duration::from_millis(100));
        let messages = hi();
        let started = Instant::now();
        let turn = ask_in(&provider, &request("gpt-4o", &messages, &[]), &CancelToken::new());
        assert!(started.elapsed() < Duration::from_secs(10), "the turn waited for the server");
        // What came within the grace is used: the trailing usage chunk.
        assert_eq!(turn.ok().usage, Usage { input_tokens: 20, output_tokens: 4, cache_read_input_tokens: 10, cache_creation_input_tokens: 0 });
        assert_eq!(turn.ok().content, [json!({"type": "text", "text": "answer"})]);
        assert!(lock(&spy.tokens)[0].is_cancelled(), "the connection is closed");
    }
}

#[test]
fn a_read_error_in_the_middle_of_a_stream_is_a_transport_error_and_is_not_retried() {
    let rig = openai(vec![
        Reply::ok(sse(&[text_chunk("half")])).end(End::Break).into(),
        reply("again").into(),
    ]);
    let turn = ask(&rig.provider, &hi());
    assert!(matches!(turn.err(), ProviderError::Transport { request_id: Some(_), .. }), "{:?}", turn.err());
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn a_turn_over_its_byte_budget_fails() {
    let rig = openai(vec![reply(&"x".repeat(500)).into()]);
    let provider = rig.provider.with_limits(TurnLimits { max_bytes: 200, ..TurnLimits::default() });
    let turn = ask(&provider, &hi());
    assert_protocol(turn.err(), &ProtocolError::TurnTooLarge { limit: 200 });
}

#[test]
fn too_many_calls_or_idle_chunks_fail_the_turn() {
    let calls: Vec<Value> = (0..4).map(|n| tool_chunk(n, &format!("id{n}"), "t", "{}")).collect();
    let rig = openai(vec![Reply::ok(sse(&calls)).into()]);
    let provider = rig.provider.with_limits(TurnLimits { max_blocks: 3, ..TurnLimits::default() });
    assert_protocol(ask(&provider, &hi()).err(), &ProtocolError::TooManyBlocks { limit: 3 });

    let idle = "data: {\"choices\":[]}\n\n".repeat(5);
    let rig = openai(vec![Reply::ok(idle).into()]);
    let provider = rig.provider.with_limits(TurnLimits { max_idle_events: 4, ..TurnLimits::default() });
    assert_protocol(ask(&provider, &hi()).err(), &ProtocolError::TooManyIdleEvents { limit: 4 });
}

// ----------------------------------------------------------------------- errors and retrying

#[test]
fn an_http_error_is_typed() {
    let cases = [
        (400, "invalid_request_error", "BadRequest"),
        (401, "invalid_api_key", "Authentication"),
        (403, "x", "Permission"),
        (404, "x", "NotFound"),
        (413, "x", "TooLarge"),
        (402, "x", "Rejected"),
    ];
    for (status, kind, name) in cases {
        let rig = openai(vec![Reply::error(status, kind, "no").into()]);
        let turn = ask(&rig.provider, &hi());
        let error = turn.err();
        assert!(format!("{error:?}").starts_with(name), "{status}: {error:?}");
        assert_eq!(error.api_failure().unwrap().status, Some(status));
        assert_eq!(rig.transport.request_count(), 1);
    }
}

#[test]
fn a_429_or_5xx_is_retried_honouring_retry_after_and_reported() {
    let rig = openai(vec![
        Reply::error(429, "rate_limit_exceeded", "slow down").header("retry-after", "2").into(),
        Reply::error(503, "server_error", "down").into(),
        reply("finally").into(),
    ]);
    let turn = ask(&rig.provider, &hi());
    assert_eq!(turn.ok().content[0]["text"], "finally");
    assert_eq!(rig.transport.request_count(), 3);
    assert_eq!(rig.sleeper.waits(), [Duration::from_secs(2), Duration::from_secs(2)]);
    assert!(turn.events[0].starts_with("Retrying { attempt: 1, after: 2s, cause: RateLimited"), "{:?}", turn.events);
    assert!(turn.events[1].contains("cause: Server"));
}

#[test]
fn retries_run_out_with_the_last_error() {
    let steps = (0..4).map(|_| Reply::error(500, "server_error", "down").into()).collect();
    let rig = openai(steps);
    let turn = ask(&rig.provider, &hi());
    assert!(matches!(turn.err(), ProviderError::Server(_)));
    assert_eq!(rig.transport.request_count(), 4);
}

#[test]
fn a_connection_failure_before_the_first_byte_is_retried_once() {
    let rig = openai(vec![
        Step::Fail(TransportError::Failed("refused".into())),
        reply("ok").into(),
    ]);
    let turn = ask(&rig.provider, &hi());
    turn.ok();
    assert!(turn.events[0].contains("cause: Transport"));
    let rig = openai(vec![
        Step::Fail(TransportError::Failed("refused".into())),
        Step::Fail(TransportError::Timeout),
        reply("ok").into(),
    ]);
    assert!(matches!(ask(&rig.provider, &hi()).err(), ProviderError::Transport { .. }));
    assert_eq!(rig.transport.request_count(), 2);
}

#[test]
fn an_error_event_before_anything_arrived_is_retried_like_the_http_error_it_stands_for() {
    let error_stream = |error: Value| Reply::ok(sse(&[json!({"error": error})]));
    let rig = openai(vec![
        error_stream(json!({"message": "busy", "type": "server_error"})).into(),
        error_stream(json!({"message": "quota", "code": "rate_limit_exceeded"})).into(),
        error_stream(json!({"message": "busy", "type": "engine_overloaded_error"})).into(),
        reply("done").into(),
    ]);
    let turn = ask(&rig.provider, &hi());
    assert_eq!(turn.ok().content[0]["text"], "done");
    assert_eq!(rig.transport.request_count(), 4);
    let causes: Vec<_> = turn.events.iter().filter(|e| e.starts_with("Retrying")).map(|e| e.rsplit("cause: ").next().unwrap().trim_end_matches(" }").to_owned()).collect();
    assert_eq!(causes, ["Server", "RateLimited", "Overloaded"]);
}

#[test]
fn an_error_event_after_content_is_final() {
    let body = sse(&[text_chunk("start"), json!({"error": {"message": "died", "type": "server_error"}})]);
    let rig = openai(vec![Reply::ok(body).into(), reply("again").into()]);
    let turn = ask(&rig.provider, &hi());
    let error = turn.err();
    assert!(matches!(error, ProviderError::Server(f) if f.status.is_none() && f.message == "died"), "{error:?}");
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn error_events_are_typed_and_a_bare_string_is_a_rejection() {
    let cases = [
        (json!({"message": "too long", "code": "context_length_exceeded"}), "BadRequest"),
        (json!({"message": "x", "type": "invalid_request_error"}), "BadRequest"),
        (json!({"message": "x", "code": "invalid_api_key"}), "Authentication"),
        (json!({"message": "x", "type": "authentication_error"}), "Authentication"),
        (json!({"message": "x", "type": "mystery"}), "Rejected"),
        (json!("model \"zz\" not found"), "Rejected"),
    ];
    for (error, name) in cases {
        let rig = openai(vec![Reply::ok(sse(&[json!({"error": error})])).into()]);
        let turn = ask(&rig.provider, &hi());
        assert!(format!("{:?}", turn.err()).starts_with(name), "{error}: {:?}", turn.err());
        assert_eq!(rig.transport.request_count(), 1);
    }
    let rig = openai(vec![Reply::ok(sse(&[json!({"error": "model \"zz\" not found"})])).into()]);
    let turn = ask(&rig.provider, &hi());
    assert_eq!(turn.err().api_failure().unwrap().message, "model \"zz\" not found");
}

// ------------------------------------------------------------------------------- cancellation

#[test]
fn a_cancel_before_sending_sends_nothing() {
    let rig = openai(vec![reply("x").into()]);
    let cancel = CancelToken::new();
    cancel.cancel();
    let messages = hi();
    let turn = ask_in(&rig.provider, &request("gpt-4o", &messages, &[]), &cancel);
    assert!(matches!(turn.err(), ProviderError::Cancelled));
    assert_eq!(rig.transport.request_count(), 0);
}

#[test]
fn a_cancel_while_the_stream_is_blocked_ends_the_turn_promptly() {
    for _ in 0..20 {
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let rig = openai(vec![Reply::ok(sse(&[text_chunk("half")])).end(End::BlockUntilCancel(Some(blocked_tx))).into()]);
        let cancel = CancelToken::new();
        let messages = hi();
        let started = Instant::now();
        let turn = std::thread::scope(|scope| {
            let worker = scope.spawn(|| ask_in(&rig.provider, &request("gpt-4o", &messages, &[]), &cancel));
            blocked_rx.recv().unwrap();
            cancel.cancel();
            worker.join().unwrap()
        });
        assert!(matches!(turn.err(), ProviderError::Cancelled), "{:?}", turn.err());
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(rig.transport.request_count(), 1);
        assert_eq!(turn.events, ["TextDelta(\"half\")"]);
    }
}

#[test]
fn a_cancel_during_a_retry_wait_sends_no_further_request() {
    for _ in 0..20 {
        let rig = openai(vec![Reply::error(503, "server_error", "down").into(), reply("x").into()]);
        let (entered_tx, entered_rx) = mpsc::channel();
        rig.sleeper.block_on_cancel(entered_tx);
        let cancel = CancelToken::new();
        let messages = hi();
        let turn = std::thread::scope(|scope| {
            let worker = scope.spawn(|| ask_in(&rig.provider, &request("gpt-4o", &messages, &[]), &cancel));
            entered_rx.recv().unwrap();
            cancel.cancel();
            worker.join().unwrap()
        });
        assert!(matches!(turn.err(), ProviderError::Cancelled), "{:?}", turn.err());
        assert_eq!(rig.transport.request_count(), 1);
    }
}

// ----------------------------------------------------------------------------------- the key

#[test]
fn the_key_is_in_no_error_event_stored_text_or_debug_output() {
    // A server that echoes the key everywhere it can.
    let key = FAKE_KEY;
    let body = sse(&[
        json!({"model": format!("m-{key}"), "choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": format!("id-{key}"), "function": {"name": format!("tool-{key}"),
            "arguments": format!("{{\"k\":\"{key}")}}]}}]}),
        tool_chunk(1, "ok_id", &format!("n{key}"), "{}"),
        finish_chunk(&format!("odd-{key}")),
    ]) + "data: [DONE]\n\n";
    let rig = openai(vec![Reply::ok(body).header("x-request-id", &format!("req-{key}")).into()]);
    let turn = ask(&rig.provider, &hi());
    let response = turn.ok();
    assert_eq!(response.model.as_deref(), Some("m-***"));
    assert_eq!(response.request_id.as_deref(), Some("req-***"));
    for block in &response.content {
        assert_eq!(block["name"], "invalid_tool_name");
        assert!(block["id"].as_str().unwrap().starts_with("call_") || block["id"] == "ok_id");
    }
    assert_eq!(response.invalid_tool_inputs.len(), 2);
    for output in [turn.everything(), format!("{:?}", rig.transport.request(0).debug)] {
        assert!(!output.contains(key) && !output.contains("ZZSECRETZZ"), "{output}");
    }
    // The next request holds the stored history, and no key either.
    let mut messages = hi();
    messages.push(Message::Assistant { content: response.content.clone() });
    messages.push(Message::user_text("again"));
    let rig = openai(vec![reply("x").into()]);
    ask(&rig.provider, &messages).ok();
    assert!(!body_of(&rig.transport, 0).contains("ZZSECRETZZ"));

    // An HTTP error that quotes the key.
    let rig = openai(vec![Reply::error(401, "invalid_api_key", &format!("bad key {key}")).into()]);
    let turn = ask(&rig.provider, &hi());
    assert!(!turn.everything().contains("ZZSECRETZZ"), "{}", turn.everything());
    assert!(turn.err().to_string().contains("***"));
    // And one that arrives as an error event.
    let rig = openai(vec![Reply::ok(sse(&[json!({"error": format!("nope {key}")})])).into()]);
    assert!(!ask(&rig.provider, &hi()).everything().contains("ZZSECRETZZ"));
}

// -------------------------------------------------------------------------------- model list

fn list_reply(body: Value) -> Step {
    Reply::ok(body.to_string()).into()
}

fn ids(models: &[ModelInfo]) -> Vec<&str> {
    models.iter().map(|m| m.id.as_str()).collect()
}

#[test]
fn the_openai_list_drops_what_cannot_chat() {
    let all = [
        "gpt-4o", "gpt-5.6", "o4-mini", "ft:gpt-4o-mini:acme::abc123", "models/gpt-4.1", "GPT-4.1-mini",
        "text-embedding-3-small", "gpt-image-1", "sora-2", "omni-moderation-latest", "gpt-4o-transcribe",
        "gpt-4o-realtime-preview", "tts-1-hd", "whisper-1", "dall-e-3", "babbage-002", "davinci-002",
        "gpt-4o-audio-preview", "gpt-4o-mini-tts", "computer-use-preview", "chatgpt-image-latest",
        "text-moderation-latest", "ft:whisper-1:acme::x", "org/tts-1",
    ];
    let data: Vec<Value> = all.iter().map(|id| json!({"id": id, "object": "model"})).collect();
    let rig = openai(vec![list_reply(json!({"object": "list", "data": data}))]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(
        ids(&models),
        ["gpt-4o", "gpt-5.6", "o4-mini", "ft:gpt-4o-mini:acme::abc123", "models/gpt-4.1", "GPT-4.1-mini"]
    );
    let sent = rig.transport.request(0);
    assert_eq!(sent.method, "GET");
    assert_eq!(sent.url, "https://api.openai.com/v1/models");
    assert!(sent.body.is_empty() && sent.header("content-type").is_none());
}

#[test]
fn a_compatible_list_is_not_filtered() {
    let rig = rig(Flavor::Compatible, "http://localhost:1234/v1", vec![list_reply(json!({"data": [
        {"id": "text-embedding-nomic"}, {"id": "whisper-large"}, {"id": "llama-3"}]}))]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(ids(&models), ["text-embedding-nomic", "whisper-large", "llama-3"]);
    assert_eq!(rig.transport.request_count(), 1);
}

#[test]
fn list_entries_are_read_leniently() {
    let rig = openai(vec![list_reply(json!({"data": [
        {"id": "a", "display_name": "Model A"},
        {"id": "b", "name": "Model B"},
        {"id": "c", "display_name": "Shown", "name": "Ignored"},
        {"id": "d", "display_name": "d"},
        {"id": "e", "name": "  "},
        {"id": "a", "display_name": "Again"},
        {"id": 7},
        {"id": "  "},
        {"id": null},
        {"name": "no id"},
        "junk",
        {"id": "f", "max_tokens": 5, "created_at": "x"},
    ]}))]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(ids(&models), ["a", "b", "c", "d", "e", "f"]);
    let names: Vec<_> = models.iter().map(|m| m.display_name.as_deref()).collect();
    assert_eq!(names, [Some("Model A"), Some("Model B"), Some("Shown"), None, None, None]);
    for model in &models {
        assert!(model.max_input_tokens.is_none() && model.max_tokens.is_none() && model.created_at.is_none());
    }
}

#[test]
fn list_entries_are_scrubbed_of_the_key() {
    let rig = openai(vec![list_reply(json!({"data": [
        {"id": format!("m-{FAKE_KEY}"), "display_name": format!("n {FAKE_KEY}")}]}))]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(models[0].id, "m-***");
    assert_eq!(models[0].display_name.as_deref(), Some("n ***"));
}

#[test]
fn a_list_of_the_wrong_shape_is_a_protocol_error() {
    for body in ["{}", r#"{"data": null}"#, r#"{"data": {"a": 1}}"#, "[1]", "[[]]", r#"[[{"id":"a"}]]"#, "not json", "", r#"{"data": "x"}"#] {
        let rig = openai(vec![Reply::ok(body).into()]);
        let error = rig.provider.list_models(&CancelToken::new()).unwrap_err();
        assert!(
            matches!(&error, ProviderError::Protocol { source: ProtocolError::MalformedBody(d), .. } if !d.is_empty()),
            "{body:?}: {error:?}"
        );
    }
}

#[test]
fn a_list_body_over_four_mebibytes_is_refused() {
    let big = format!(r#"{{"data": [], "pad": "{}"}}"#, "x".repeat(4 * 1024 * 1024));
    let rig = openai(vec![Reply::ok(big).into()]);
    let error = rig.provider.list_models(&CancelToken::new()).unwrap_err();
    assert_protocol(&error, &ProtocolError::TurnTooLarge { limit: 4 * 1024 * 1024 });
}

#[test]
fn a_list_request_is_typed_and_retried_like_a_turn() {
    let rig = openai(vec![Reply::error(401, "invalid_api_key", "no").into()]);
    assert!(matches!(
        rig.provider.list_models(&CancelToken::new()),
        Err(ProviderError::Authentication(_))
    ));
    let rig = openai(vec![Reply::error(429, "rate_limit_exceeded", "slow").into(), list_reply(json!({"data": [{"id": "gpt-4o"}]}))]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(ids(&models), ["gpt-4o"]);
    assert_eq!(rig.transport.request_count(), 2);
    assert_eq!(rig.sleeper.waits().len(), 1);
}

// ------------------------------------------------------------------------------------- Ollama

fn show(capabilities: Value) -> Step {
    Reply::ok(json!({"capabilities": capabilities}).to_string()).into()
}

fn ollama(steps: Vec<Step>) -> Rig {
    rig(Flavor::Ollama, OLLAMA_BASE_URL, steps)
}

fn listing(ids: &[&str]) -> Step {
    list_reply(json!({"data": ids.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()}))
}

#[test]
fn the_ollama_list_probes_each_model_and_keeps_what_can_complete() {
    let rig = ollama(vec![
        listing(&["chat", "embed", "old", "broken", "down", "bad-json", "vision"]),
        show(json!(["completion", "tools"])),
        show(json!(["embedding"])),
        show(json!([])),
        Reply::error(500, "server_error", "boom").into(),
        Step::Fail(TransportError::Failed("refused".into())),
        Reply::ok("{").into(),
        show(json!(["completion", "vision"])),
    ]);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(ids(&models), ["chat", "old", "broken", "down", "bad-json", "vision"]);
    // One list request, one probe per model, one attempt each: a failed probe is not retried.
    assert_eq!(rig.transport.request_count(), 8);
    assert!(rig.sleeper.waits().is_empty());
    for (index, id) in ["chat", "embed", "old", "broken", "down", "bad-json", "vision"].iter().enumerate() {
        let probe = rig.transport.request(index + 1);
        assert_eq!(probe.method, "POST");
        assert_eq!(probe.url, "http://localhost:11434/api/show");
        assert_eq!(probe.json(), json!({"model": id}));
        assert_eq!(probe.header("content-type"), Some("application/json"));
        assert_eq!(probe.header("authorization"), Some(format!("Bearer {FAKE_KEY}").as_str()));
    }
}

#[test]
fn a_probe_has_short_timeouts_and_no_credentials_without_a_key() {
    let rig = rig_with(
        config(Flavor::Ollama, OLLAMA_BASE_URL),
        no_key(),
        vec![listing(&["m"]), show(json!(["completion"]))],
    );
    rig.provider.list_models(&CancelToken::new()).unwrap();
    let probe = rig.transport.request(1);
    assert!(probe.header("authorization").is_none());
    assert!(probe.debug.contains("connect: 5s") && probe.debug.contains("first_byte: 5s") && probe.debug.contains("idle: 5s"), "{}", probe.debug);
    // The list itself uses the configured timeouts.
    assert!(rig.transport.request(0).debug.contains("connect: 15s"));
}

#[test]
fn at_most_sixty_four_models_are_probed_and_the_rest_are_kept() {
    let names: Vec<String> = (0..70).map(|n| format!("m{n}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut steps = vec![listing(&refs)];
    steps.extend((0..64).map(|_| show(json!(["embedding"]))));
    let rig = ollama(steps);
    let models = rig.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(rig.transport.request_count(), 65);
    // The 64 probed are all dropped, the 6 beyond the bound are kept.
    assert_eq!(ids(&models), ["m64", "m65", "m66", "m67", "m68", "m69"]);
}

fn model(id: &str) -> ModelInfo {
    ModelInfo { id: id.into(), display_name: None, max_input_tokens: None, max_tokens: None, created_at: None }
}

#[test]
fn probing_stops_when_the_time_budget_is_spent() {
    let rig = ollama(vec![show(json!(["embedding"]))]);
    let key = ApiKey::new(FAKE_KEY);
    let scrub = scrub_for(Some(&key));
    let kept = rig
        .provider
        .with_completion(Some(&key), &scrub, vec![model("a")], models::MAX_PROBES, Duration::ZERO, &CancelToken::new())
        .unwrap();
    assert_eq!(ids(&kept), ["a"]);
    assert_eq!(rig.transport.request_count(), 0);
}

/// Cancels the caller's token when the second request (the first probe) is sent.
struct CancelOnProbe {
    inner: Arc<ScriptedTransport>,
    token: CancelToken,
    sent: AtomicUsize,
}

impl HttpTransport for CancelOnProbe {
    fn send(
        &self,
        request: HttpRequest,
        cancel: &CancelToken,
    ) -> Result<HttpResponse, TransportError> {
        if self.sent.fetch_add(1, Ordering::SeqCst) == 1 {
            self.token.cancel();
            return Err(TransportError::Failed("cut by the cancel".into()));
        }
        self.inner.send(request, cancel)
    }
}

#[test]
fn a_cancel_inside_a_probe_ends_the_whole_list() {
    let cancel = CancelToken::new();
    let inner = ScriptedTransport::new(vec![listing(&["a", "b"]), show(json!(["completion"]))]);
    let transport = Arc::new(CancelOnProbe {
        inner: Arc::clone(&inner),
        token: cancel.clone(),
        sent: AtomicUsize::new(0),
    });
    let provider = OpenAiProvider::new(
        transport as Arc<dyn HttpTransport>,
        Arc::new(FixedKey),
        config(Flavor::Ollama, OLLAMA_BASE_URL),
    )
    .unwrap();
    assert!(matches!(provider.list_models(&cancel), Err(ProviderError::Cancelled)));
    // Only the list reached the script: the probe was cut, and `b` was never asked.
    assert_eq!(inner.request_count(), 1);
}

#[test]
fn a_cancel_before_a_probe_ends_the_whole_list() {
    let rig = ollama(vec![listing(&["a"]), show(json!(["completion"]))]);
    let key = ApiKey::new(FAKE_KEY);
    let scrub = scrub_for(Some(&key));
    let cancel = CancelToken::new();
    cancel.cancel();
    let models = vec![model("a")];
    let result = rig
        .provider
        .with_completion(Some(&key), &scrub, models, models::MAX_PROBES, models::PROBE_BUDGET, &cancel);
    assert!(matches!(result, Err(ProviderError::Cancelled)));
    assert_eq!(rig.transport.request_count(), 0);
}

#[test]
fn tools_are_only_asked_of_ollama() {
    for flavor in [Flavor::OpenAi, Flavor::Compatible] {
        let keys = Keys::new(Err(KeyError::Unavailable("must not be asked".into())));
        let rig = rig_with(config(flavor, "http://localhost:9/v1"), Arc::clone(&keys) as Arc<dyn KeySource>, vec![]);
        assert_eq!(rig.provider.model_supports_tools("gpt-4o", &CancelToken::new()).unwrap(), None);
        assert_eq!(rig.transport.request_count(), 0);
        assert_eq!(keys.asked(), 0);
    }
}

#[test]
fn ollama_says_whether_a_model_takes_tools() {
    let cases = [
        (json!(["completion", "tools"]), Some(true)),
        (json!(["completion"]), Some(false)),
        (json!(["completion", "vision"]), Some(false)),
        (json!([]), None),
        (json!(null), None),
    ];
    for (capabilities, want) in cases {
        let rig = ollama(vec![show(capabilities.clone())]);
        let got = rig.provider.model_supports_tools("  qwen3:8b \n", &CancelToken::new()).unwrap();
        assert_eq!(got, want, "{capabilities}");
        assert_eq!(rig.transport.request(0).json(), json!({"model": "qwen3:8b"}));
        assert_eq!(rig.transport.request_count(), 1);
    }
    // No capabilities field at all.
    let rig = ollama(vec![Reply::ok("{}").into()]);
    assert_eq!(rig.provider.model_supports_tools("m", &CancelToken::new()).unwrap(), None);
}

#[test]
fn a_failed_tools_probe_is_an_error_and_so_is_a_key_error() {
    let rig = ollama(vec![Reply::error(404, "x", "model not found").into()]);
    assert!(matches!(rig.provider.model_supports_tools("m", &CancelToken::new()), Err(ProviderError::NotFound(_))));
    let rig = ollama(vec![Reply::ok("not json").into()]);
    assert!(matches!(rig.provider.model_supports_tools("m", &CancelToken::new()), Err(ProviderError::Protocol { .. })));
    let rig = ollama(vec![Step::Fail(TransportError::Failed("down".into()))]);
    assert!(matches!(rig.provider.model_supports_tools("m", &CancelToken::new()), Err(ProviderError::Transport { .. })));
    assert_eq!(rig.transport.request_count(), 1);
    let rig = rig_with(
        config(Flavor::Ollama, OLLAMA_BASE_URL),
        Keys::new(Err(KeyError::Unavailable("locked".into()))),
        vec![],
    );
    assert!(matches!(rig.provider.model_supports_tools("m", &CancelToken::new()), Err(ProviderError::Key(KeyError::Unavailable(_)))));
    assert_eq!(rig.transport.request_count(), 0);
}
