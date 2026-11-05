//! The key stays out of everything. Every run of the harness is checked (see
//! `Harness::assert_no_secret`); these tests aim at the paths where it could get in.

use crate::provider::{KeyError, KeySource, ProviderError};
use crate::test_support::*;
use crate::transport::{Header, HeaderValue};
use crate::ApiKey;
use crate::{AnthropicConfig, AnthropicProvider, AssistantError, HttpTransport, TransportError};
use engine_agent::AgentAccess;
use std::sync::Arc;

#[test]
fn the_secret_header_prints_stars() {
    let header = Header::secret("x-api-key", ApiKey::new(FAKE_KEY));
    assert!(header.value.is_secret());
    assert_eq!(
        format!("{header:?}"),
        "Header { name: \"x-api-key\", value: *** }"
    );
    assert_eq!(format!("{:?}", HeaderValue::plain("a")), "\"a\"");
    assert_eq!(header.value.expose(), FAKE_KEY);
}

#[test]
fn an_api_message_that_echoes_the_key_is_scrubbed() {
    let mut harness = Harness::new(vec![Reply::error(
        401,
        "authentication_error",
        &format!("invalid x-api-key: {FAKE_KEY}"),
    )
    .into()]);
    let error = harness.run("hi").unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(text.contains("invalid x-api-key: ***"), "{text}");
}

#[test]
fn a_transport_message_and_a_stream_error_that_echo_the_key_are_scrubbed() {
    let mut harness = Harness::new(vec![
        Step::Fail(TransportError::Failed(format!("proxy rejected {FAKE_KEY}"))),
        Step::Fail(TransportError::Failed(format!("proxy rejected {FAKE_KEY}"))),
    ]);
    let error = harness.run("hi").unwrap_err();
    assert!(format!("{error}").contains("proxy rejected ***"));

    let body = Stream::new()
        .error_event("api_error", &format!("boom {FAKE_KEY}"))
        .unfinished();
    let mut harness = Harness::new(vec![Reply::ok(body).into()]);
    let AssistantError::Provider(error) = harness.run("hi").unwrap_err() else {
        panic!()
    };
    assert!(error.to_string().contains("boom ***"));
}

struct Missing;

impl KeySource for Missing {
    fn api_key(&self) -> Result<ApiKey, KeyError> {
        Err(KeyError::NotSet)
    }
}

#[test]
fn without_a_key_nothing_is_sent() {
    let transport = ScriptedTransport::new(vec![]);
    let provider = AnthropicProvider::new(
        Arc::clone(&transport) as Arc<dyn HttpTransport>,
        Arc::new(Missing),
        AnthropicConfig::default(),
    )
    .unwrap();
    let layer = Harness::with(
        vec![],
        AnthropicConfig::default(),
        Some(AgentAccess::Read),
        false,
    )
    .layer;
    let mut conversation = crate::Conversation::new(None);
    let error = crate::Assistant::new(&provider, &layer, Default::default())
        .run(
            &mut conversation,
            "hi".into(),
            &mut |_| {},
            &engine_agent::CancelToken::new(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        AssistantError::Provider(ProviderError::Key(KeyError::NotSet))
    ));
    assert_eq!(transport.request_count(), 0);
    assert!(conversation.is_empty());
}

#[test]
fn the_key_is_asked_for_at_request_time() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Counting(AtomicUsize);
    impl KeySource for Counting {
        fn api_key(&self) -> Result<ApiKey, KeyError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ApiKey::new(FAKE_KEY))
        }
    }
    let keys = Arc::new(Counting(AtomicUsize::new(0)));
    let transport = ScriptedTransport::new(vec![text_reply("a").into(), text_reply("b").into()]);
    let provider = AnthropicProvider::new(
        Arc::clone(&transport) as Arc<dyn HttpTransport>,
        Arc::clone(&keys) as Arc<dyn KeySource>,
        AnthropicConfig::default(),
    )
    .unwrap();
    let layer = Harness::with(
        vec![],
        AnthropicConfig::default(),
        Some(AgentAccess::Read),
        false,
    )
    .layer;
    let mut conversation = crate::Conversation::new(None);
    let assistant = crate::Assistant::new(&provider, &layer, Default::default());
    for text in ["one", "two"] {
        assistant
            .run(
                &mut conversation,
                text.into(),
                &mut |_| {},
                &engine_agent::CancelToken::new(),
            )
            .unwrap();
    }
    assert_eq!(
        keys.0.load(Ordering::SeqCst),
        2,
        "once per request, never cached"
    );
}
