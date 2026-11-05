use crate::provider::{ChatProvider, ProviderError};
use crate::test_support::*;
use engine_agent::CancelToken;
use serde_json::json;

#[test]
fn models_are_listed_page_by_page() {
    let page = |last: &str, more: bool, id: &str| {
        Reply::ok(
            json!({"data": [{"id": id, "display_name": format!("Name {id}"),
                             "max_input_tokens": 1_000_000, "max_tokens": 128_000,
                             "capabilities": {"thinking": {"supported": true}}}],
                   "has_more": more, "first_id": id, "last_id": last})
            .to_string(),
        )
    };
    let harness = Harness::new(vec![
        page("claude-a", true, "claude-a").into(),
        page("claude b/", false, "claude-b").into(),
    ]);
    let models = harness.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["claude-a", "claude-b"]
    );
    assert_eq!(models[0].display_name.as_deref(), Some("Name claude-a"));
    assert_eq!(models[0].max_input_tokens, Some(1_000_000));
    assert_eq!(models[0].max_tokens, Some(128_000));
    let (first, second) = (harness.transport.request(0), harness.transport.request(1));
    assert_eq!(first.method, "GET");
    assert_eq!(first.url, "https://api.anthropic.com/v1/models?limit=100");
    assert_eq!(
        second.url,
        "https://api.anthropic.com/v1/models?limit=100&after_id=claude-a"
    );
    assert_eq!(first.header("x-api-key"), Some(FAKE_KEY));
    assert_eq!(first.header("anthropic-version"), Some("2023-06-01"));
    assert!(first.header("content-type").is_none());
    assert!(first.header("anthropic-beta").is_none());
}

#[test]
fn a_page_cursor_is_percent_encoded() {
    let harness = Harness::new(vec![
        Reply::ok(json!({"data": [], "has_more": true, "last_id": "a b/&"}).to_string()).into(),
        Reply::ok(json!({"data": [], "has_more": false}).to_string()).into(),
    ]);
    harness.provider.list_models(&CancelToken::new()).unwrap();
    assert_eq!(
        harness.transport.request(1).url,
        "https://api.anthropic.com/v1/models?limit=100&after_id=a%20b%2F%26"
    );
}

#[test]
fn listing_models_fails_with_the_typed_error() {
    let harness = Harness::new(vec![Reply::error(
        401,
        "authentication_error",
        "invalid x-api-key",
    )
    .into()]);
    let error = harness
        .provider
        .list_models(&CancelToken::new())
        .unwrap_err();
    assert!(
        matches!(error, ProviderError::Authentication(_)),
        "{error:?}"
    );
    assert_eq!(error.request_id(), Some("req_scripted"));
}

#[test]
fn a_list_longer_than_the_page_cap_is_an_error_not_a_cut_list() {
    let page =
        || Reply::ok(json!({"data": [], "has_more": true, "last_id": "x"}).to_string()).into();
    let harness = Harness::new((0..30).map(|_| page()).collect());
    let error = harness
        .provider
        .list_models(&CancelToken::new())
        .unwrap_err();
    assert!(
        matches!(
            error,
            ProviderError::Protocol {
                source: crate::ProtocolError::TooManyPages { limit: 20 },
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(harness.transport.request_count(), 20);
}

#[test]
fn a_body_that_breaks_after_a_cancel_is_a_cancel() {
    let cancel = CancelToken::new();
    let hook = cancel.clone();
    let reply = Reply::ok(r#"{"data": ["#)
        .hook_at(3, move || hook.cancel())
        .end(End::Break);
    let harness = Harness::new(vec![reply.into()]);
    let error = harness.provider.list_models(&cancel).unwrap_err();
    assert!(matches!(error, ProviderError::Cancelled), "{error:?}");
    // Without the cancel it is a transport failure.
    let harness = Harness::new(vec![Reply::ok(r#"{"data": ["#).end(End::Break).into()]);
    let error = harness
        .provider
        .list_models(&CancelToken::new())
        .unwrap_err();
    assert!(
        matches!(error, ProviderError::Transport { .. }),
        "{error:?}"
    );
    // A page that is not the list is a typed body error.
    let harness = Harness::new(vec![Reply::ok("[1,2]").into()]);
    let error = harness
        .provider
        .list_models(&CancelToken::new())
        .unwrap_err();
    assert!(matches!(
        error,
        ProviderError::Protocol {
            source: crate::ProtocolError::MalformedBody(_),
            ..
        }
    ));
}
