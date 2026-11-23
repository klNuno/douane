use super::*;

#[test]
fn no_client_key_header_reaches_the_provider() {
    let account: AccountCredential = serde_json::from_value(json!({
        "id": "a", "name": "a", "provider": "antigravity", "access_token": "upstream",
        "enabled": true, "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap();
    let mut incoming = HeaderMap::new();
    for name in ["authorization", "x-api-key", "x-goog-api-key", "api-key"] {
        incoming.insert(name, "dk_client".parse().unwrap());
    }
    let sent = upstream_headers(&account, &incoming);
    assert!(sent.values().all(|v| v != "dk_client"), "{sent:?}");
}

fn fixture_account(provider: SubscriptionProvider) -> AccountCredential {
    serde_json::from_value(json!({
        "id":"fixture-account", "name":"fixture", "provider":provider,
        "access_token":"fixture-not-a-real-token", "enabled":true,
        "created_at":"2026-10-03T12:00:00Z", "updated_at":"2026-10-03T12:00:00Z"
    }))
    .unwrap()
}

#[tokio::test]
async fn anthropic_cache_write_then_read_counts_input_once_across_usage_events() {
    let ledger = Ledger::open(std::path::Path::new(":memory:")).unwrap();
    let account = fixture_account(SubscriptionProvider::Claude);
    let mut writing = CallGuard::new(ledger.clone(), &account, "claude-sonnet-4-6");
    let write_id = writing.call.id.clone();
    writing.observe(&json!({"type":"message_start","message":{"usage":{
            "input_tokens":13,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":7231
        }}}));
    assert_eq!(writing.call.input_tokens, 7244);
    writing.observe(&json!({"type":"message_delta","usage":{"output_tokens":22}}));
    writing.observe(&json!({"type":"message_delta","usage":{"output_tokens":22,"cache_creation_input_tokens":7231}}));
    assert_eq!(writing.call.input_tokens, 7244);
    writing.observe(&json!({"type":"message_stop"}));
    drop(writing);

    let mut reading = CallGuard::new(ledger.clone(), &account, "claude-sonnet-4-6");
    let read_id = reading.call.id.clone();
    reading.observe(&json!({"type":"message_start","message":{"usage":{
            "input_tokens":19,"output_tokens":0,"cache_read_input_tokens":7231,"cache_creation_input_tokens":0
        }}}));
    assert_eq!(reading.call.input_tokens, 7250);
    reading.observe(&json!({"type":"message_delta","usage":{"output_tokens":16}}));
    reading.observe(&json!({"type":"message_delta","usage":{"output_tokens":16,"cache_read_input_tokens":7231}}));
    assert_eq!(reading.call.input_tokens, 7250);
    reading.observe(&json!({"type":"message_stop"}));
    drop(reading);

    assert_eq!(ledger.requests.load(Ordering::Relaxed), 2);
    assert_eq!(ledger.errors.load(Ordering::Relaxed), 0);
    assert_eq!(ledger.input_tokens.load(Ordering::Relaxed), 14494);
    assert_eq!(ledger.output_tokens.load(Ordering::Relaxed), 38);
    assert_eq!(ledger.cache_read_tokens.load(Ordering::Relaxed), 7231);
    assert_eq!(ledger.cache_write_tokens.load(Ordering::Relaxed), 7231);
    assert_eq!(ledger.in_flight.load(Ordering::Relaxed), 0);
    let calls = ledger.calls(2).await;
    let written = calls.iter().find(|call| call.id == write_id).unwrap();
    let read = calls.iter().find(|call| call.id == read_id).unwrap();
    assert_eq!(
        (
            written.input_tokens,
            written.cache_read_tokens,
            written.cache_write_tokens
        ),
        (7244, 0, 7231)
    );
    assert_eq!(
        (
            read.input_tokens,
            read.cache_read_tokens,
            read.cache_write_tokens
        ),
        (7250, 7231, 0)
    );
}

#[test]
fn google_refusal_keeps_enum_fields_and_retry_delay_only() {
    let body = br#"{"error":{"code":429,"message":"No capacity on project secret-project-42 for you@example.com","status":"RESOURCE_EXHAUSTED",
            "details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"MODEL_CAPACITY_EXHAUSTED","domain":"cloudcode-pa.googleapis.com","metadata":{"model":"gemini-3.8-flash-high"}},
            {"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"2.5s"}]}}"#;
    let refusal =
        crate::subscriptions::cooldown::google_refusal(body, "gemini-3.8-flash-high").unwrap();
    assert_eq!(
        refusal.reason,
        "RESOURCE_EXHAUSTED/MODEL_CAPACITY_EXHAUSTED"
    );
    assert_eq!(refusal.retry_seconds, Some(3));
    let quota = br#"{"error":{"status":"RESOURCE_EXHAUSTED","details":[{"reason":"QUOTA_EXHAUSTED","metadata":{"quotaResetDelay":"1h2m3.4s"}}]}}"#;
    assert_eq!(
        crate::subscriptions::cooldown::google_refusal(quota, "gemini-3.8-flash-high")
            .unwrap()
            .retry_seconds,
        Some(3724)
    );
    assert_eq!(
        crate::subscriptions::cooldown::google_duration("750ms"),
        Some(1)
    );
    assert_eq!(
        crate::subscriptions::cooldown::google_duration("soon"),
        None
    );
    let free_text = br#"{"error":{"status":"Project secret-project-42 is blocked"}}"#;
    assert_eq!(
        crate::subscriptions::cooldown::google_refusal(free_text, "gemini-3.8-flash-high"),
        None
    );
    assert_eq!(
        crate::subscriptions::cooldown::google_refusal(b"not json", "gemini-3.8-flash-high"),
        None
    );
}

#[test]
fn google_refusal_keeps_the_longest_explicit_delay_in_any_order() {
    let details = [
        json!({"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"60s"}),
        json!({"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED","metadata":{"model":"gemini-4-argon","quotaResetDelay":"172800s"}}),
    ];
    for details in [json!(details), json!([details[1], details[0]])] {
        let body =
            serde_json::to_vec(&json!({"error":{"status":"RESOURCE_EXHAUSTED","details":details}}))
                .unwrap();
        assert_eq!(
            crate::subscriptions::cooldown::google_refusal(&body, "gemini-4-argon")
                .unwrap()
                .retry_seconds,
            Some(172800)
        );
    }
}

#[tokio::test]
async fn codex_input_total_already_includes_cache_reads_and_is_not_added_again() {
    let ledger = Ledger::open(std::path::Path::new(":memory:")).unwrap();
    let account = fixture_account(SubscriptionProvider::Codex);
    let mut call = CallGuard::new(ledger.clone(), &account, "gpt-fixture");
    let completed = json!({"type":"response.completed","response":{"usage":{
        "input_tokens":900,"output_tokens":40,"input_tokens_details":{"cached_tokens":800}
    }}});
    // Buffered collection can observe both the terminal event and its response.
    call.observe(&completed);
    call.observe(&completed["response"]);
    assert_eq!(call.call.input_tokens, 900);
    assert_eq!(call.call.cache_read_tokens, 800);
    assert_eq!(call.call.cache_write_tokens, 0);
    drop(call);
    assert_eq!(ledger.input_tokens.load(Ordering::Relaxed), 900);
    assert_eq!(ledger.cache_read_tokens.load(Ordering::Relaxed), 800);
    assert_eq!(ledger.requests.load(Ordering::Relaxed), 1);
    assert_eq!(ledger.errors.load(Ordering::Relaxed), 0);
    let recorded = ledger.calls(1).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (
            recorded[0].input_tokens,
            recorded[0].output_tokens,
            recorded[0].cache_read_tokens
        ),
        (900, 40, 800)
    );
}
