use super::*;

#[tokio::test]
async fn codex_usage_poll_confirms_resets_for_http_and_websocket_header_refusals() {
    use crate::subscriptions::cooldown::{Transport, classify_http};
    use axum::{Json, Router, routing::get};
    use time::{Duration, OffsetDateTime};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let mut service = QuotaService::new(reqwest::Client::new(), store.clone());
    let reset = OffsetDateTime::now_utc() + Duration::hours(1);
    let next_reset = (reset + Duration::weeks(1)).unix_timestamp();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service.codex_usage_url = format!("http://{}/usage", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let router = Router::new().route(
            "/usage",
            get(move || async move {
                Json(json!({"rate_limit":{"primary_window":{
                    "used_percent":0, "limit_window_seconds":604800, "reset_at":next_reset
                }}}))
            }),
        );
        axum::serve(listener, router).await.unwrap();
    });
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "10080".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-at",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    for (id, transport) in [("http", Transport::Http), ("ws", Transport::WebSocket)] {
        let summary = store
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":id}),
                None,
            )
            .await
            .unwrap();
        let credential = store.get(&summary.id).await.unwrap();
        let received = OffsetDateTime::now_utc();
        service
            .observe_response_headers_at(&credential, &headers, received, false)
            .await;
        let refusal = classify_http(
            credential.provider,
            "gpt-test",
            429,
            &headers,
            None,
            None,
            received,
            transport,
        )
        .unwrap();
        store.record_refusal(&summary.id, refusal).await.unwrap();
        let snapshot = service.refresh(&summary.id).await.unwrap();
        assert_eq!(snapshot.windows[0].id, "weekly");
        assert_eq!(snapshot.windows[0].remaining_percent, Some(100.0));
        let account = store.get(&summary.id).await.unwrap();
        assert!(account.quota.refusals.is_empty());
        assert_eq!(account.summary().status, "ready");
    }
    upstream.abort();
}

#[tokio::test]
async fn codex_reset_after_seconds_matches_retry_after_with_subsecond_received() {
    use crate::subscriptions::cooldown::{Transport, classify_http};
    use axum::{Json, Router, routing::get};
    use time::{Duration, OffsetDateTime};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let mut service = QuotaService::new(reqwest::Client::new(), store.clone());
    let received = (OffsetDateTime::now_utc() - Duration::seconds(2))
        .replace_nanosecond(789_000_000)
        .unwrap();
    let next_reset = (received + Duration::weeks(1)).unix_timestamp();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    service.codex_usage_url = format!("http://{}/usage", listener.local_addr().unwrap());
    let upstream = tokio::spawn(async move {
        let router = Router::new().route(
            "/usage",
            get(move || async move {
                Json(json!({"rate_limit":{"primary_window":{
                    "used_percent":0, "limit_window_seconds":18000, "reset_at":next_reset
                }}}))
            }),
        );
        axum::serve(listener, router).await.unwrap();
    });
    let summary = store
        .import(
            json!({"type":"codex","access_token":"fixture","account_id":"subsec"}),
            None,
        )
        .await
        .unwrap();
    let credential = store.get(&summary.id).await.unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    headers.insert(
        "x-codex-primary-reset-after-seconds",
        "3600".parse().unwrap(),
    );
    headers.insert("retry-after", "3600".parse().unwrap());
    service
        .observe_response_headers_at(&credential, &headers, received, false)
        .await;
    let refusal = classify_http(
        credential.provider,
        "gpt-test",
        429,
        &headers,
        None,
        None,
        received,
        Transport::Http,
    )
    .unwrap();
    assert_eq!(refusal.reset_at, refusal.retry_after_at);
    store.record_refusal(&summary.id, refusal).await.unwrap();
    let snapshot = service.refresh(&summary.id).await.unwrap();
    assert_eq!(snapshot.windows[0].remaining_percent, Some(100.0));
    let account = store.get(&summary.id).await.unwrap();
    assert!(account.quota.refusals.is_empty());
    assert_eq!(account.summary().status, "ready");
    upstream.abort();
}

#[tokio::test]
async fn successful_codex_zero_header_before_poll_does_not_pause_and_jitter_does_not_rewrite() {
    use time::{Duration, OffsetDateTime};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let summary = store
        .import(
            json!({"type":"codex","access_token":"fixture","account_id":"codex-headers"}),
            None,
        )
        .await
        .unwrap();
    let credential = store.get(&summary.id).await.unwrap();
    let file = directory.path().join(format!("{}.json", summary.id));
    let at = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();

    let mut positive = reqwest::header::HeaderMap::new();
    positive.insert("x-codex-primary-used-percent", "20".parse().unwrap());
    positive.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    positive.insert(
        "x-codex-primary-reset-after-seconds",
        "3600".parse().unwrap(),
    );
    let before = std::fs::read(&file).unwrap();
    service
        .observe_response_headers_at(&credential, &positive, at, true)
        .await;
    positive.insert("x-codex-primary-used-percent", "25".parse().unwrap());
    positive.insert(
        "x-codex-primary-reset-after-seconds",
        "3601".parse().unwrap(),
    );
    service
        .observe_response_headers_at(&credential, &positive, at + Duration::seconds(2), true)
        .await;
    assert!(
        std::fs::read(&file).unwrap() == before,
        "positive codex countdown jitter rewrote the account file"
    );

    let mut zero = reqwest::header::HeaderMap::new();
    zero.insert("x-codex-primary-used-percent", "100".parse().unwrap());
    zero.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
    zero.insert(
        "x-codex-primary-reset-after-seconds",
        "3590".parse().unwrap(),
    );
    service
        .observe_response_headers_at(&credential, &zero, at + Duration::seconds(5), true)
        .await;
    let account = store.get(&summary.id).await.unwrap();
    assert_eq!(account.summary().status, "ready");
    assert!(account.quota.exhaustion.is_empty());
    assert!(account.quota.zeros.contains_key("five-hour"));
}

async fn claude_fixture() -> (
    tempfile::TempDir,
    Arc<AccountStore>,
    QuotaService,
    AccountCredential,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(dir.path()).unwrap());
    let summary = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let account = store.get(&summary.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    (dir, store, service, account)
}

fn claude_headers(used: Option<&str>, reset: time::OffsetDateTime) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(used) = used {
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            used.parse().unwrap(),
        );
    }
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    headers
}

#[tokio::test]
async fn reset_only_claude_header_drops_previous_cycle_usage() {
    let (_dir, store, service, account) = claude_fixture().await;
    let now = time::OffsetDateTime::now_utc();
    let reset = now + time::Duration::hours(1);
    service
        .observe_response_headers_at(&account, &claude_headers(Some("1"), reset), now, false)
        .await;
    service
        .observe_response_headers_at(
            &account,
            &claude_headers(None, reset),
            now + time::Duration::seconds(1),
            true,
        )
        .await;
    assert_eq!(
        service.get(&account.id).await.unwrap().windows[0].remaining_percent,
        Some(0.0)
    );
    let next = reset + time::Duration::hours(5);
    service
        .observe_response_headers_at(
            &account,
            &claude_headers(None, next),
            now + time::Duration::seconds(2),
            true,
        )
        .await;
    let snapshot = service.get(&account.id).await.unwrap();
    assert_eq!(snapshot.windows[0].remaining_percent, None);
    assert_eq!(snapshot.windows[0].used, None);
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
}

#[tokio::test]
async fn complete_poll_drops_obsolete_windows_but_keeps_concurrent_headers_and_proofs() {
    let (dir, _store, service, account) = claude_fixture().await;
    let at = time::OffsetDateTime::now_utc();
    let seconds = time::Duration::seconds;
    let observe = |value, received| Observation {
        windows: parse_claude(&value, received),
        received,
        plan: None,
        subscription: None,
        credits: vec![],
        organization_id: None,
    };
    let initial = observe(
        json!({
            "seven_day": {"utilization":100},
            "five_hour": {"utilization":100,"resets_at":"2099-01-01T00:00:00Z"},
            "seven_day_opus": {"utilization":100}
        }),
        at,
    );
    service
        .publish_observation(&account, initial, at, true)
        .await
        .unwrap();
    let reset = at + time::Duration::hours(1);
    service
        .observe_response_headers_at(
            &account,
            &claude_headers(Some("0"), reset),
            at + seconds(2),
            true,
        )
        .await;
    service.save().await;
    let complete = observe(
        json!({"seven_day_sonnet":{"utilization":10}}),
        at + seconds(3),
    );
    let snapshot = service
        .publish_observation(&account, complete, at + seconds(1), true)
        .await
        .unwrap();
    assert_eq!(
        snapshot
            .windows
            .iter()
            .map(|w| w.id.as_str())
            .collect::<std::collections::BTreeSet<_>>(),
        ["five-hour", "seven-day-sonnet"].into()
    );
    let empty = observe(json!({}), at + seconds(4));
    let snapshot = service
        .publish_observation(&account, empty, at + seconds(4), true)
        .await
        .unwrap();
    assert!(snapshot.windows.is_empty());
    service
        .observe_response_headers_at(
            &account,
            &claude_headers(Some("1"), reset),
            at + seconds(3),
            false,
        )
        .await;
    assert!(service.get(&account.id).await.unwrap().windows.is_empty());
    let reopened = Arc::new(AccountStore::open(dir.path()).unwrap());
    assert_eq!(
        reopened.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
    assert!(
        reopened
            .get(&account.id)
            .await
            .unwrap()
            .quota
            .resets
            .contains_key("five-hour")
    );
    let service = QuotaService::new(reqwest::Client::new(), reopened);
    assert!(service.get(&account.id).await.unwrap().windows.is_empty());
}

#[tokio::test]
async fn poll_metadata_ignores_header_ticks_and_older_full_polls() {
    let (_dir, store, service, account) = claude_fixture().await;
    let at = time::OffsetDateTime::now_utc();
    let seconds = time::Duration::seconds;
    let reset = at + time::Duration::hours(1);
    service
        .observe_response_headers_at(
            &account,
            &claude_headers(Some("0.25"), reset),
            at + seconds(2),
            true,
        )
        .await;
    let observe = |plan: &str, received| Observation {
        windows: parse_claude(&json!({"five_hour":{"utilization":70}}), received),
        received,
        plan: Some(plan.into()),
        subscription: None,
        credits: vec![QuotaCredits {
            id: plan.into(),
            label: plan.into(),
            remaining: Some(50.0),
            used: None,
            limit: None,
            unit: "USD".into(),
            reset_at: None,
            unlimited: None,
            enabled: Some(true),
        }],
        organization_id: Some(format!("org-{plan}")),
    };
    service
        .publish_observation(
            &account,
            observe("max", at + seconds(3)),
            at + seconds(1),
            true,
        )
        .await
        .unwrap();
    service
        .publish_observation(&account, observe("pro", at + seconds(4)), at, true)
        .await
        .unwrap();
    let current = store.get(&account.id).await.unwrap();
    assert_eq!(current.plan.as_deref(), Some("max"));
    assert_eq!(current.organization_id.as_deref(), Some("org-max"));
    let snapshot = service.get(&account.id).await.unwrap();
    assert_eq!(snapshot.credits[0].id, "max");
    assert_eq!(snapshot.windows[0].remaining_percent, Some(75.0));
}

#[tokio::test]
async fn positive_claude_ticks_do_not_rewrite_the_account_file() {
    let (dir, _, service, account) = claude_fixture().await;
    let file = dir.path().join(format!("{}.json", account.id));
    let at = time::OffsetDateTime::now_utc();
    let reset = at + time::Duration::hours(1);
    let tick = |used, offset| {
        let headers = claude_headers(Some(used), reset);
        let service = &service;
        let account = &account;
        async move {
            service
                .observe_response_headers_at(
                    account,
                    &headers,
                    at + time::Duration::seconds(offset),
                    true,
                )
                .await;
        }
    };
    let before = std::fs::read(&file).unwrap();
    tick("0.1", 0).await;
    tick("0.2", 1).await;
    assert!(
        std::fs::read(&file).unwrap() == before,
        "ordinary header tick rewrote the account"
    );
    assert_eq!(
        service.get(&account.id).await.unwrap().windows[0].remaining_percent,
        Some(80.0)
    );
    tick("1", 2).await;
    let reopened = AccountStore::open(dir.path()).unwrap();
    let persisted = reopened.get(&account.id).await.unwrap();
    assert_eq!(persisted.summary().status, "exhausted");
    assert!(persisted.quota.zeros.contains_key("five-hour"));
    let before = std::fs::read(&file).unwrap();
    tick("1", 3).await;
    assert!(
        std::fs::read(&file).unwrap() == before,
        "unchanged zero proof rewrote the account"
    );
    tick("0", 4).await;
    let reopened = AccountStore::open(dir.path()).unwrap();
    let persisted = reopened.get(&account.id).await.unwrap();
    assert_eq!(persisted.summary().status, "ready");
    assert!(persisted.quota.resets.contains_key("five-hour"));
    let before = std::fs::read(&file).unwrap();
    tick("0", 5).await;
    assert!(
        std::fs::read(&file).unwrap() == before,
        "unchanged reset proof rewrote the account"
    );
}

#[tokio::test]
async fn claude_headers_confirm_a_reset_after_partial_read_failure_and_restart() {
    use crate::subscriptions::cooldown::{Transport, classify_http};
    use time::{Duration, OffsetDateTime};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let credential = store.get(&account.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let before = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::seconds(15);
    let reset = before + Duration::hours(1);
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "1".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-reset",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-status",
        "rejected".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-representative-claim",
        "five_hour".parse().unwrap(),
    );
    headers.insert("retry-after", "3598".parse().unwrap());
    service
        .observe_response_headers_at(&credential, &headers, before, false)
        .await;
    let classify = |headers: &reqwest::header::HeaderMap| {
        classify_http(
            credential.provider,
            "claude-sonnet-5-5",
            429,
            headers,
            None,
            None,
            before + Duration::seconds(2),
            Transport::Http,
        )
        .unwrap()
    };
    let refusal = classify(&headers);
    assert_eq!(
        refusal.retry_after_at.as_deref(),
        Some(timestamp(reset).as_str())
    );
    // The refusal arrives thirteen seconds before it can be stored.
    store
        .record_refusal(&account.id, refusal.clone())
        .await
        .unwrap();
    headers.insert("retry-after", "3600".parse().unwrap());
    let independent = classify(&headers);
    store
        .record_refusal(&account.id, independent.clone())
        .await
        .unwrap();
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "exhausted"
    );
    headers.remove("anthropic-ratelimit-unified-5h-reset");
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0.01".parse().unwrap(),
    );
    service
        .observe_response_headers_at(&credential, &headers, before + Duration::seconds(5), true)
        .await;
    service
        .complete_refresh(
            &account.id,
            &credential,
            Err(SubscriptionError::Upstream(500)),
            Trigger::Manual,
        )
        .await
        .unwrap();
    assert!(service.get(&account.id).await.unwrap().error.is_some());
    drop(service);
    drop(store);

    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let credential = store.get(&account.id).await.unwrap();
    headers.insert(
        "anthropic-ratelimit-unified-5h-reset",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-5h-utilization",
        "0".parse().unwrap(),
    );
    service
        .observe_response_headers_at(&credential, &headers, OffsetDateTime::now_utc(), true)
        .await;
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.quota.refusals.len(), 1);
    assert_eq!(
        after.quota.refusals[0].retry_after_at,
        independent.retry_after_at
    );
    assert_eq!(
        service.get(&account.id).await.unwrap().windows[0].remaining_percent,
        Some(100.0)
    );
    // A matching refusal whose body was still in flight cannot park it again.
    store.record_refusal(&account.id, refusal).await.unwrap();
    assert_eq!(
        store.get(&account.id).await.unwrap().quota.refusals.len(),
        1
    );
}

#[tokio::test]
async fn a_fable_overage_refusal_is_released_by_its_canonical_budget() {
    use crate::subscriptions::cooldown::{Transport, classify_http};
    use time::{Duration, OffsetDateTime};

    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let first = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::seconds(30);
    let reset = first + Duration::days(3);
    let observation = |used, received| Observation {
        received,
        windows: parse_claude(
            &json!({
                "seven_day": {"utilization": 12, "resets_at": timestamp(reset)},
                "iguana_necktie": {"resets_at": timestamp(reset)},
                "limits": [{"kind": "weekly_scoped", "percent": used,
                    "scope": {"model": {"id": "claude-fable-5-2", "display_name": "Fable"}}}]
            }),
            received,
        ),
        plan: None,
        subscription: None,
        credits: vec![],
        organization_id: None,
    };
    let credential = store.get(&account.id).await.unwrap();
    service
        .complete_refresh(
            &account.id,
            &credential,
            Ok(observation(100, first)),
            Trigger::Manual,
        )
        .await
        .unwrap();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-status",
        "rejected".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-representative-claim",
        "seven_day_overage_included".parse().unwrap(),
    );
    headers.insert(
        "anthropic-ratelimit-unified-reset",
        reset.unix_timestamp().to_string().parse().unwrap(),
    );
    let refusal = classify_http(
        credential.provider,
        "claude-fable-5-2",
        429,
        &headers,
        None,
        None,
        first + Duration::seconds(1),
        Transport::Http,
    )
    .unwrap();
    assert_eq!(refusal.scope, QuotaScope::Model("claude-fable-5-2".into()));
    assert_eq!(refusal.window.as_deref(), Some("seven-day-fable"));
    store.record_refusal(&account.id, refusal).await.unwrap();
    let before = store.get(&account.id).await.unwrap();
    assert!(!before.serves("claude-fable-5-2"));
    assert!(before.serves("claude-opus-5-5"));
    let mut renewed = before.clone();
    renewed.access_token = "fixture-renewed".into();
    store.update_credential(&before, renewed).await.unwrap();
    service
        .complete_refresh(
            &account.id,
            &before,
            Ok(observation(0, OffsetDateTime::now_utc())),
            Trigger::Manual,
        )
        .await
        .unwrap();
    let after = store.get(&account.id).await.unwrap();
    assert!(after.quota.refusals.is_empty());
    assert!(after.serves("claude-fable-5-2"));
    assert_eq!(after.access_token, "fixture-renewed");
}

#[tokio::test]
async fn exact_model_exhaustion_uses_the_latest_bucket_deadline() {
    use time::{Duration, OffsetDateTime};
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"gemini","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let received = OffsetDateTime::now_utc();
    let window = |bucket, reset| QuotaWindow {
        id: format!("model:gemini-fixture:{bucket}"),
        label: "Synthetic allowance".into(),
        remaining_percent: Some(0.0),
        reset_at: Some(timestamp(reset)),
        used: None,
        limit: None,
    };
    let later = received + Duration::days(2);
    let credential = store.get(&account.id).await.unwrap();
    service
        .complete_refresh(
            &account.id,
            &credential,
            Ok(Observation {
                received,
                windows: vec![
                    window("weekly", later),
                    window("daily", received + Duration::days(1)),
                ],
                plan: None,
                subscription: None,
                credits: vec![],
                organization_id: None,
            }),
            Trigger::Manual,
        )
        .await
        .unwrap();
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.retry_at("gemini-fixture"), Some(later));
    assert_eq!(
        after.model_exhaustion["gemini-fixture"],
        Some(timestamp(later))
    );
    assert!(!after.serves("gemini-fixture"));
    assert!(after.serves("gemini-fixture-preview"));
}

#[tokio::test]
async fn meta_zero_session_without_duration_blocks_the_account() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"meta","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    service
        .observe_meta_event(
            &account.id,
            &json!({"type":"response.subscription_usage",
        "subscription": {"window": {"used_percent": 100}}}),
        )
        .await;
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "exhausted"
    );
    assert_eq!(
        service.get(&account.id).await.unwrap().windows[0].id,
        "session"
    );
    service
        .observe_meta_event(
            &account.id,
            &json!({"type":"response.subscription_usage",
        "subscription": {"window": {"used_percent": 90}}}),
        )
        .await;
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
}
use serde_json::json;

async fn billing_fixture() -> (tempfile::TempDir, Arc<AccountStore>, QuotaService, String) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({
                "type": "openai_compatible", "route": "refuseless",
                "base_url": "https://api.refuseless.com/v1", "api_key": "fixture-old"
            }),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    (directory, store, service, account.id)
}

#[tokio::test]
async fn compatible_quota_does_not_persist_a_key_balance() {
    let (_directory, store, service, id) = billing_fixture().await;
    let credential = store.get(&id).await.unwrap();
    let reading = Observation {
        received: time::OffsetDateTime::now_utc(),
        windows: vec![],
        plan: None,
        subscription: None,
        organization_id: None,
        credits: BillingApi::Refuseless
            .parse(
                &json!({"available": 7, "unit": "usd"}),
                time::OffsetDateTime::now_utc(),
            )
            .unwrap(),
    };
    service
        .complete_refresh(&id, &credential, Ok(reading), Trigger::Manual)
        .await
        .unwrap();
    service.save().await;
    let restarted = QuotaService::new(reqwest::Client::new(), store);
    assert!(restarted.get(&id).await.is_none());
}

#[tokio::test]
async fn compatible_quota_drops_old_balance_and_backoff_on_key_rotation() {
    let (_directory, store, service, id) = billing_fixture().await;
    let old = store.get(&id).await.unwrap();
    service
        .complete_refresh(
            &id,
            &old,
            Err(SubscriptionError::Upstream(503)),
            Trigger::Manual,
        )
        .await
        .unwrap();
    store
        .update_endpoint(
            &id,
            super::super::EndpointPatch {
                api_key: Some("fixture-new".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        service
            .complete_refresh(
                &id,
                &old,
                Err(SubscriptionError::Upstream(503)),
                Trigger::Manual
            )
            .await,
        Err(SubscriptionError::NotFound)
    ));
    assert!(
        service.refresh_background(&id).await.is_some(),
        "the old key delayed a read for the new key"
    );
    assert!(service.get(&id).await.unwrap().credits.is_empty());
}

#[tokio::test]
async fn compatible_quota_reads_only_documented_billing_endpoints() {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    let vibe_forbidden = Arc::new(AtomicBool::new(false));
    let forbid_vibe = vibe_forbidden.clone();
    let app = Router::new()
        .route(
            "/api/v1/key",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-openrouter");
                Json(json!({"data": {
                    "limit": null, "limit_remaining": null, "usage": 3.5,
                    "free_model_daily_requests": {"limit": 50, "used": 50, "remaining": 0}
                }}))
            }),
        )
        .route(
            "/v1/credits",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-refuseless");
                Json(json!({"available": 0, "unit": "usd", "key_limit": 0, "key_spent": 1.25}))
            }),
        )
        .route(
            "/v1/billing/budget",
            get(move |headers: HeaderMap| {
                let forbidden = forbid_vibe.load(Ordering::Relaxed);
                async move {
                    let budget = match headers["authorization"].to_str().unwrap() {
                        "Bearer fixture-mistral-vibe" if forbidden => {
                            return StatusCode::FORBIDDEN.into_response();
                        }
                        "Bearer fixture-mistral-vibe" => json!({
                            "initial_budget": 20,
                            "usage_percentage": 37.5,
                            "reset_at": "2026-12-01T00:00:00Z"
                        }),
                        "Bearer fixture-mistral-studio" => Value::Null,
                        _ => panic!("unexpected fixture key"),
                    };
                    Json(json!({"vibe_budget": budget})).into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let mut service = QuotaService::new(reqwest::Client::new(), store.clone());
    service.billing_test_origin = Some(origin);

    for (name, base, supported) in [
        ("openrouter", "https://openrouter.ai/api/v1/", true),
        ("refuseless", "https://api.refuseless.com/v1", true),
        ("mistral-vibe", "https://api.mistral.ai/v1", true),
        ("mistral-studio", "https://api.mistral.ai/v1", true),
        ("hinoki", "https://inference.hinoki.security/v1", false),
        ("private", "https://api.refuseless.com.example/v1", false),
    ] {
        let account = store
            .import(
                json!({
                    "type": "openai_compatible", "route": name, "base_url": base,
                    "api_key": format!("fixture-{name}")
                }),
                None,
            )
            .await
            .unwrap();
        assert_eq!(QuotaService::supports(&account), supported);
        let now = time::OffsetDateTime::now_utc();
        assert_eq!(
            service
                .due(time::Duration::minutes(5), now)
                .await
                .contains(&account.id),
            supported
        );
        if !supported {
            assert!(matches!(
                service.refresh(&account.id).await,
                Err(SubscriptionError::Unsupported)
            ));
            assert!(service.get(&account.id).await.is_none());
            continue;
        }

        let credential = store.get(&account.id).await.unwrap();
        let mut refusal = crate::subscriptions::cooldown::classify_http(
            credential.provider,
            "fixture-model",
            402,
            &reqwest::header::HeaderMap::new(),
            None,
            None,
            time::OffsetDateTime::now_utc(),
            crate::subscriptions::cooldown::Transport::Http,
        )
        .unwrap();
        refusal.budget_revision = Some(credential.budget_revision);
        store.record_refusal(&account.id, refusal).await.unwrap();
        let first = service.refresh(&account.id).await.unwrap();
        assert!(first.error.is_none());
        assert!(first.windows.is_empty());
        match name {
            "openrouter" => {
                assert_eq!(first.credits.len(), 2);
                assert_eq!(first.credits[0].unlimited, Some(true));
                assert_eq!(first.credits[0].remaining, None);
                assert_eq!(first.credits[1].unit, "requests");
                assert_eq!(first.credits[1].remaining, Some(0.0));
            }
            "refuseless" => {
                assert_eq!(first.credits.len(), 2);
                assert_eq!(first.credits[0].remaining, Some(0.0));
                assert_eq!(first.credits[1].limit, None);
                assert_eq!(first.credits[1].remaining, Some(0.0));
            }
            "mistral-vibe" => {
                assert_eq!(first.credits.len(), 1);
                let budget = &first.credits[0];
                assert_eq!(budget.id, "mistral-vibe-monthly");
                assert_eq!(budget.unit, "%");
                assert_eq!(budget.remaining, Some(62.5));
                assert_eq!(budget.used, Some(37.5));
                assert_eq!(budget.reset_at.as_deref(), Some("2026-12-01T00:00:00Z"));

                vibe_forbidden.store(true, Ordering::Relaxed);
                let unavailable = service.refresh(&account.id).await.unwrap();
                assert!(unavailable.error.is_none());
                assert!(unavailable.credits.is_empty());
                assert!(service.get(&account.id).await.unwrap().credits.is_empty());
            }
            "mistral-studio" => assert!(first.credits.is_empty()),
            _ => unreachable!(),
        }
        let saved = store.get(&account.id).await.unwrap();
        assert_eq!(saved.last_error.as_deref(), Some("upstream quota exceeded"));
        assert!(service.get(&account.id).await.is_some());
    }
    server.abort();
}

#[tokio::test]
async fn meta_session_uses_the_known_reserve_window_and_unknown_duration_stays_unknown() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"meta","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store);
    service.observe_meta_event(&account.id,&json!({"type":"response.subscription_usage","subscription":{"weekly":{"used_percent":30,"resets_at":1792368000},"window":{"used_percent":80,"resets_at":1792272519,"window_duration_mins":300}}})).await;
    let snapshot = service.get(&account.id).await.unwrap();
    let session = snapshot
        .windows
        .iter()
        .find(|w| w.label == "Session")
        .unwrap();
    assert_eq!(reserve_kind(&session.id), Some("five-hour"));
    assert_eq!(session.remaining_percent, Some(20.0));
    let older = parse_timestamp(&snapshot.updated_at).unwrap() - time::Duration::seconds(1);
    service.observe_meta_event_at(&account.id, &json!({"type":"response.subscription_usage","subscription":{"weekly":{"used_percent":0}}}), older).await;
    assert_eq!(service.get(&account.id).await.unwrap().windows.len(), 2);
    assert_eq!(
        service.get(&account.id).await.unwrap().updated_at,
        snapshot.updated_at
    );
    service.observe_meta_event(&account.id,&json!({"type":"response.subscription_usage","subscription":{"window":{"used_percent":10}}})).await;
    let snapshot = service.get(&account.id).await.unwrap();
    assert_eq!(reserve_kind(&snapshot.windows[0].id), None);
}

#[test]
fn copilot_premium_requests_get_a_bar_and_unlimited_ones_none() {
    let student = json!({
        "copilot_plan":"individual","access_type_sku":"free_educational_quota",
        "quota_reset_date":"2026-11-01","quota_reset_date_utc":"2026-11-01T00:00:00.000Z",
        "quota_snapshots":{
            "chat":{"unlimited":true,"entitlement":0,"remaining":0,"percent_remaining":100.0},
            "completions":{"unlimited":true,"entitlement":0,"remaining":0,"percent_remaining":100.0},
            "premium_interactions":{"unlimited":false,"entitlement":200,"remaining":150,"percent_remaining":75.0}
        }
    });
    let windows = parse_copilot(&student, observed());
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].id, "premium");
    assert_eq!(windows[0].remaining_percent, Some(75.0));
    assert_eq!(windows[0].used, Some(50.0));
    assert_eq!(windows[0].limit, Some(200.0));
    assert_eq!(windows[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
    // An empty premium bar must not take the account out: included models
    // keep answering.
    assert!(!is_global(&windows[0].id));
    assert_eq!(
        SubscriptionTier::detect(
            SubscriptionProvider::Copilot,
            Some("free_educational_quota"),
            None,
            "provider_usage"
        )
        .map(|tier| tier.label),
        Some("Copilot Student".into())
    );
    // A free plan's limited chat gets its own bar, read from counts alone.
    let free = json!({"quota_reset_date":"2026-11-01","quota_snapshots":{
            "chat":{"unlimited":false,"entitlement":50,"remaining":10}}});
    let windows = parse_copilot(&free, observed());
    assert_eq!(windows[0].id, "chat");
    assert_eq!(windows[0].remaining_percent, Some(20.0));
    assert_eq!(windows[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
}

fn observed() -> time::OffsetDateTime {
    parse_timestamp("2026-10-03T12:00:00Z").unwrap()
}

#[test]
fn reserve_counts_only_known_account_windows_before_their_reset() {
    let window = |id: &str, remaining: Option<f64>, reset: &str| QuotaWindow {
        id: id.into(),
        label: id.into(),
        remaining_percent: remaining,
        reset_at: Some(reset.into()),
        used: None,
        limit: Some(100.0),
    };
    let snapshot = |windows| QuotaSnapshot {
        account_id: "fixture".into(),
        provider: SubscriptionProvider::Claude,
        windows,
        updated_at: now(),
        error: None,
        subscription: None,
        credits: vec![],
        verification_required: false,
        verification_available: false,
    };
    let later = "2026-10-05T12:00:00Z";
    let earlier = "2026-10-03T11:00:00Z";
    // Only the longest window decides: a nearly empty five-hour window
    // beside a weekly one never trips the reserve.
    let both = snapshot(vec![
        window("seven-day", Some(40.0), later),
        window("five-hour", Some(2.0), later),
    ]);
    assert_eq!(both.reserve_window_remaining(&[], observed()), Some(40.0));
    assert!(!both.reserve_reached(5.0, &[], observed()));
    assert!(both.reserve_reached(40.0, &[], observed()));
    assert!(!both.reserve_reached(39.0, &[], observed()));
    // OpenCode Go shape: monthly outranks weekly and five-hour.
    let go = snapshot(vec![
        window("five-hour", Some(100.0), later),
        window("weekly", Some(88.0), later),
        window("monthly", Some(3.0), later),
    ]);
    assert_eq!(go.reserve_window_remaining(&[], observed()), Some(3.0));
    assert!(go.reserve_reached(5.0, &[], observed()));
    let go_weekly_low = snapshot(vec![
        window("weekly", Some(1.0), later),
        window("monthly", Some(60.0), later),
    ]);
    assert!(!go_weekly_low.reserve_reached(5.0, &[], observed()));
    // Limits lift only near the reset of the longest window, a week or
    // more, whatever its id.
    let at = observed().unix_timestamp();
    let in_hours = |hours: i64| {
        (observed() + time::Duration::hours(hours))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let fortnight = snapshot(vec![window("window-1209600s", Some(3.0), &in_hours(5))]);
    assert!(fortnight.longest_window_ends_within(6 * 3600, at));
    assert!(!fortnight.longest_window_ends_within(4 * 3600, at));
    let fortnight_later = snapshot(vec![
        window("weekly", Some(3.0), &in_hours(5)),
        window("window-1209600s", Some(50.0), &in_hours(72)),
    ]);
    assert!(!fortnight_later.longest_window_ends_within(6 * 3600, at));
    let short = snapshot(vec![window("daily", Some(3.0), &in_hours(5))]);
    assert!(!short.longest_window_ends_within(6 * 3600, at));
    // The longest window past its reset no longer holds the account
    // back, and a shorter one does not take over.
    let reset = snapshot(vec![
        window("seven-day", Some(1.0), earlier),
        window("five-hour", Some(1.0), later),
    ]);
    assert_eq!(reset.reserve_window_remaining(&[], observed()), None);
    assert!(!reset.reserve_reached(5.0, &[], observed()));
    assert!(reset.reports_account_window());
    // Each alias counts when it is the only account-wide window.
    for id in [
        "five-hour",
        "daily",
        "seven-day",
        "seven_day",
        "weekly",
        "monthly",
        "window-1",
    ] {
        let single = snapshot(vec![window(id, Some(3.0), later)]);
        assert_eq!(
            single.reserve_window_remaining(&[], observed()),
            Some(3.0),
            "{id}"
        );
        assert!(single.reserve_reached(5.0, &[], observed()), "{id}");
    }
    // A parsed window length ranks by its seconds; an unparsed one last.
    let parsed = snapshot(vec![
        window("window-2592000s", Some(50.0), later),
        window("weekly", Some(2.0), later),
        window("window-period", Some(0.0), later),
    ]);
    assert_eq!(parsed.reserve_window_remaining(&[], observed()), Some(50.0));
    // Model-scoped and unknown windows never invent a limit.
    let scoped = snapshot(vec![
        window("seven-day-opus", Some(0.0), later),
        window("seven-day", None, later),
    ]);
    assert_eq!(scoped.reserve_window_remaining(&[], observed()), None);
    assert!(!scoped.reserve_reached(5.0, &[], observed()));
    assert!(!scoped.reports_account_window());
    // A non-finite share is unknown, not a measurement.
    let broken = snapshot(vec![window("seven-day", Some(f64::NAN), later)]);
    assert_eq!(broken.reserve_window_remaining(&[], observed()), None);
    assert!(!broken.reports_account_window());
    // Chosen kinds replace the longest-window default: five-hour alone,
    // both, or a kind the account does not report.
    let pick = |kinds: &[&str]| kinds.iter().map(|k| k.to_string()).collect::<Vec<_>>();
    let five = pick(&["five-hour"]);
    assert_eq!(both.reserve_window_remaining(&five, observed()), Some(2.0));
    assert!(both.reserve_reached(5.0, &five, observed()));
    let pair = pick(&["five-hour", "weekly"]);
    let weekly_low = snapshot(vec![
        window("seven-day", Some(3.0), later),
        window("five-hour", Some(90.0), later),
    ]);
    assert!(weekly_low.reserve_reached(5.0, &pair, observed()));
    assert!(both.reserve_reached(5.0, &pair, observed()));
    assert!(!weekly_low.reserve_reached(5.0, &five, observed()));
    let monthly = pick(&["monthly"]);
    assert_eq!(both.reserve_window_remaining(&monthly, observed()), None);
    assert!(!both.reports_reserve_window(&monthly));
    assert!(both.reports_reserve_window(&five));
    // A chosen window past its reset stops counting until the next read.
    assert_eq!(reset.reserve_window_remaining(&five, observed()), Some(1.0));
    assert_eq!(
        reset.reserve_window_remaining(&pick(&["weekly"]), observed()),
        None
    );
}

#[test]
fn claude_fable_weekly_limit_reads_from_the_scoped_limit_even_when_not_binding() {
    // The usage reply of 2026-11-09: the Fable weekly limit lives only in
    // `limits`, scoped by display name, with `is_active` false.
    let windows = parse_claude(
        &json!({
            "five_hour": {"utilization": 92.0, "resets_at": "2026-11-10T02:10:00+00:00"},
            "seven_day": {"utilization": 12.0, "resets_at": "2026-11-11T09:00:00+00:00"},
            "iguana_necktie": null,
            "limits": [
                {"kind": "session", "group": "session", "percent": 92, "resets_at": "2026-11-10T02:10:00+00:00", "scope": null, "is_active": true},
                {"kind": "weekly_all", "group": "weekly", "percent": 12, "resets_at": "2026-11-11T09:00:00+00:00", "scope": null, "is_active": false},
                {"kind": "weekly_scoped", "group": "weekly", "percent": 21, "resets_at": "2026-11-11T09:00:00+00:00",
                 "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": false}
            ]
        }),
        observed(),
    );
    let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["five-hour", "seven-day", "seven-day-fable"]);
    let fable = &windows[2];
    assert_eq!(fable.label, "Fable weekly");
    assert_eq!(fable.remaining_percent, Some(79.0));
    assert!(
        fable
            .reset_at
            .as_deref()
            .is_some_and(|r| r.starts_with("2026-11-11T09:00:00"))
    );
    // The legacy field and the scoped limit name the same window once.
    let both = parse_claude(
        &json!({
            "iguana_necktie": {"utilization": 30, "resets_at": "2026-11-11T09:00:00Z"},
            "limits": [{"kind": "weekly_scoped", "percent": 30, "resets_at": "2026-11-11T09:00:00Z",
                        "scope": {"model": {"display_name": "Fable"}}, "is_active": false}]
        }),
        observed(),
    );
    assert_eq!(both.iter().filter(|w| w.id == "seven-day-fable").count(), 1);
}

#[test]
fn claude_session_limit_is_the_five_hour_window_not_a_third_one() {
    let reset = "2026-10-03T16:00:00Z";
    let windows = parse_claude(
        &json!({
            "five_hour": {"utilization": 53, "resets_at": reset},
            "seven_day": {"utilization": 32, "resets_at": "2026-10-07T23:00:00Z"},
            "limits": [{"kind": "session", "percent": 53, "resets_at": reset}]
        }),
        observed(),
    );
    let ids: Vec<_> = windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["five-hour", "seven-day"]);
    // Without the legacy field, the session limit still reads as five hours.
    let only = parse_claude(
        &json!({"limits": [{"kind": "session", "percent": 10, "resets_at": reset}]}),
        observed(),
    );
    assert_eq!(only[0].id, "five-hour");
    assert_eq!(only[0].remaining_percent, Some(90.0));
}

#[tokio::test]
async fn verification_action_lifecycle_preserves_observations_and_clears_on_recovery() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let summary = store
        .import(
            json!({"type":"antigravity","access_token":"fixture","project_id":"fixture-project"}),
            None,
        )
        .await
        .unwrap();
    let account = store.get(&summary.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let observation = || Observation {
        received: time::OffsetDateTime::now_utc(),
        windows: parse_antigravity(
            &json!({"groups":[{"displayName":"Models","buckets":[{"remainingFraction":0.3,"resetTime":"2030-10-04T12:00:00Z"}]}]}),
            observed(),
        ),
        plan: None,
        subscription: None,
        credits: Vec::new(),
        organization_id: None,
    };
    let first = service
        .complete_refresh(&summary.id, &account, Ok(observation()), Trigger::Manual)
        .await
        .unwrap();
    assert!(!first.verification_required);
    assert!(!first.verification_available);
    assert!(service.verification_url(&summary.id).await.is_none());

    let challenge = "https://accounts.google.com/signin/continue?challenge=private-fixture";
    let blocked = service
        .complete_refresh(
            &summary.id,
            &account,
            Err(SubscriptionError::verification_required(Some(challenge))),
            Trigger::Manual,
        )
        .await
        .unwrap();
    assert!(blocked.verification_required);
    assert!(blocked.verification_available);
    assert_eq!(blocked.updated_at, first.updated_at);
    assert_eq!(blocked.windows[0].remaining_percent, Some(30.0));
    assert_eq!(
        service.verification_url(&summary.id).await.as_deref(),
        Some(challenge)
    );
    let public = serde_json::to_string(&service.list().await).unwrap();
    assert!(!public.contains("private-fixture"));
    assert!(!public.contains("accounts.google.com"));
    assert!(!format!("{blocked:?}").contains("private-fixture"));

    let without_action = service
        .complete_refresh(
            &summary.id,
            &account,
            Err(SubscriptionError::verification_required(Some(
                "https://evil.test/challenge",
            ))),
            Trigger::Manual,
        )
        .await
        .unwrap();
    assert!(without_action.verification_required);
    assert!(!without_action.verification_available);
    assert!(service.verification_url(&summary.id).await.is_none());

    service
        .complete_refresh(
            &summary.id,
            &account,
            Err(SubscriptionError::verification_required(Some(challenge))),
            Trigger::Manual,
        )
        .await
        .unwrap();
    let recovered = service
        .complete_refresh(&summary.id, &account, Ok(observation()), Trigger::Manual)
        .await
        .unwrap();
    assert!(!recovered.verification_required);
    assert!(!recovered.verification_available);
    assert!(recovered.error.is_none());
    assert!(service.verification_url(&summary.id).await.is_none());

    service
        .complete_refresh(
            &summary.id,
            &account,
            Err(SubscriptionError::verification_required(Some(challenge))),
            Trigger::Manual,
        )
        .await
        .unwrap();
    store.delete(&summary.id).await.unwrap();
    assert!(service.verification_url(&summary.id).await.is_none());
    assert!(service.list().await.is_empty());
    assert!(service.verification_urls.read().await.is_empty());
}

#[test]
fn retry_delay_widens_up_to_its_cap() {
    let seconds: Vec<_> = (1..=8).map(|n| retry_delay(n).whole_seconds()).collect();
    assert_eq!(seconds, [30, 60, 120, 240, 480, 960, 1800, 1800]);
    assert_eq!(retry_delay(u32::MAX).whole_seconds(), 1800);
}

#[tokio::test]
async fn background_failures_stay_quiet_back_off_and_clear_on_success() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let summary = store
        .import(
            json!({"type":"antigravity","access_token":"fixture","project_id":"fixture-project"}),
            None,
        )
        .await
        .unwrap();
    let account = store.get(&summary.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let id = summary.id.as_str();
    let observation = || Observation {
        received: time::OffsetDateTime::now_utc(),
        windows: parse_antigravity(
            &json!({"groups":[{"displayName":"Models","buckets":[{"remainingFraction":0.3,"resetTime":"2030-10-04T12:00:00Z"}]}]}),
            observed(),
        ),
        plan: None,
        subscription: None,
        credits: Vec::new(),
        organization_id: None,
    };
    let limited = || Err(SubscriptionError::Upstream(429));
    let message = "provider is rate-limiting this account's quota reads (HTTP 429); retry later";
    let outage = || Err(SubscriptionError::Upstream(503));
    let shown_outage = "upstream authentication or quota request failed (HTTP 503)";
    let interval = time::Duration::minutes(5);
    let second = time::Duration::seconds(1);
    // An account with no numbers at all.
    let forget = || async {
        let mut empty = observation();
        empty.windows.clear();
        service
            .complete_refresh(id, &account, Ok(empty), Trigger::Background)
            .await
            .unwrap();
    };

    service
        .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
        .await
        .unwrap();
    let start = time::OffsetDateTime::now_utc();
    assert!(service.due(interval, start).await.is_empty());
    assert_eq!(service.due(interval, start + interval).await, [id]);

    for count in 1..FAILURES_SHOWN {
        let quiet = service
            .complete_refresh(id, &account, outage(), Trigger::Background)
            .await
            .unwrap();
        assert!(quiet.error.is_none(), "failure {count}");
        assert_eq!(quiet.windows[0].remaining_percent, Some(30.0));
        // The next read waits for a delay that widens with each failure.
        let now = time::OffsetDateTime::now_utc();
        let delay = retry_delay(count);
        assert!(
            service
                .due(interval, now + delay - 2 * second)
                .await
                .is_empty()
        );
        assert_eq!(service.due(interval, now + delay + second).await, [id]);
        assert!(service.refresh_background(id).await.is_none());
    }
    let shown = service
        .complete_refresh(id, &account, outage(), Trigger::Background)
        .await
        .unwrap();
    assert_eq!(shown.error.as_deref(), Some(shown_outage));
    let sticky = service
        .complete_refresh(id, &account, outage(), Trigger::Background)
        .await
        .unwrap();
    assert_eq!(sticky.error.as_deref(), Some(shown_outage));

    // One good read removes the error and the retry schedule at once.
    let recovered = service
        .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
        .await
        .unwrap();
    assert!(recovered.error.is_none());
    assert!(service.get(id).await.unwrap().error.is_none());
    let now = time::OffsetDateTime::now_utc();
    assert!(service.due(interval, now).await.is_empty());

    // A 429 never hides real numbers, by hand or past the fifth failure,
    // yet it still backs off.
    for trigger in [Trigger::Manual; 3]
        .into_iter()
        .chain([Trigger::Background; 4])
    {
        let quiet = service
            .complete_refresh(id, &account, limited(), trigger)
            .await
            .unwrap();
        assert!(quiet.error.is_none());
        assert_eq!(quiet.windows[0].remaining_percent, Some(30.0));
    }
    assert!(service.refresh_background(id).await.is_none());
    // A read asked by hand reports any other first failure.
    let manual_outage = service
        .complete_refresh(id, &account, outage(), Trigger::Manual)
        .await
        .unwrap();
    assert_eq!(manual_outage.error.as_deref(), Some(shown_outage));
    service
        .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
        .await
        .unwrap();
    // With no numbers to show, the 429 is the answer.
    forget().await;
    let manual = service
        .complete_refresh(id, &account, limited(), Trigger::Manual)
        .await
        .unwrap();
    assert_eq!(manual.error.as_deref(), Some(message));
    service
        .complete_refresh(id, &account, Ok(observation()), Trigger::Background)
        .await
        .unwrap();

    // A failure that cannot pass on its own shows at once.
    let revoked = service
        .complete_refresh(
            id,
            &account,
            Err(SubscriptionError::SessionRevoked),
            Trigger::Background,
        )
        .await
        .unwrap();
    assert!(revoked.error.is_some());

    // A deleted account leaves no retry state behind.
    store.delete(id).await.unwrap();
    assert!(service.due(interval, now + interval).await.is_empty());
    assert!(service.failures.read().await.is_empty());
}

#[test]
fn a_snapshot_saved_before_plan_weights_loads_with_them() {
    // Written by a douane without default_weight: the pool must not count
    // this Codex Pro like a Plus until the next successful read.
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("quota-snapshots.json");
    std::fs::write(&path, serde_json::to_vec(&json!({"pro":{"account_id":"pro","provider":"codex","windows":[],"updated_at":"2026-11-21T12:00:00Z","error":null,"subscription":{"id":"pro","label":"Codex Pro","multiplier":null,"source":"provider_usage"}}})).unwrap()).unwrap();
    let loaded = load_snapshots(&path);
    assert_eq!(
        loaded["pro"].subscription.as_ref().unwrap().default_weight,
        Some(20)
    );
}

#[tokio::test]
async fn snapshots_outlive_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let summary = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let account = store.get(&summary.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "anthropic-ratelimit-unified-7d-utilization",
        "0.97".parse().unwrap(),
    );
    service.observe_claude_headers(&account, &headers).await;
    service.save().await;
    // The account file scan skips the snapshot file.
    let reopened = Arc::new(AccountStore::open(directory.path()).unwrap());
    assert_eq!(reopened.list().await.len(), 1);
    let restarted = QuotaService::new(reqwest::Client::new(), reopened);
    let snapshot = restarted.get(&summary.id).await.unwrap();
    assert_eq!(snapshot.windows[0].used, Some(97.0));
    // A 429 after the restart keeps those numbers and shows nothing.
    let limited = restarted
        .complete_refresh(
            &summary.id,
            &account,
            Err(SubscriptionError::Upstream(429)),
            Trigger::Manual,
        )
        .await
        .unwrap();
    assert!(limited.error.is_none());
    assert_eq!(limited.windows[0].used, Some(97.0));
}

#[tokio::test]
async fn claude_inference_headers_refresh_windows_and_end_a_failure_streak() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let summary = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let account = store.get(&summary.id).await.unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let id = summary.id.as_str();
    let header = |pairs: &[(&'static str, &str)]| {
        let mut map = reqwest::header::HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, value.parse().unwrap());
        }
        map
    };
    // No unified headers, as on count_tokens: nothing is recorded.
    service.observe_claude_headers(&account, &header(&[])).await;
    assert!(service.get(id).await.is_none());

    let usage = Observation {
        received: time::OffsetDateTime::now_utc(),
        windows: parse_claude(
            &json!({
                "five_hour": {"utilization": 10, "resets_at": "2030-10-03T16:00:00Z"},
                "seven_day": {"utilization": 50, "resets_at": "2030-10-07T23:00:00Z"},
                "seven_day_opus": {"utilization": 20, "resets_at": "2030-10-07T23:00:00Z"}
            }),
            observed(),
        ),
        plan: None,
        subscription: None,
        credits: Vec::new(),
        organization_id: None,
    };
    service
        .complete_refresh(id, &account, Ok(usage), Trigger::Background)
        .await
        .unwrap();
    let mut aged = service.get(id).await.unwrap();
    aged.updated_at = timestamp(time::OffsetDateTime::now_utc() - time::Duration::hours(1));
    aged.error = Some("old".into());
    service.insert_for_test(aged).await;
    service
        .complete_refresh(
            id,
            &account,
            Err(SubscriptionError::Upstream(429)),
            Trigger::Background,
        )
        .await
        .unwrap();
    assert!(!service.failures.read().await.is_empty());

    service
        .observe_claude_headers(
            &account,
            &header(&[
                ("anthropic-ratelimit-unified-5h-utilization", "0.0"),
                ("anthropic-ratelimit-unified-5h-reset", "1792263000"),
                ("anthropic-ratelimit-unified-7d-utilization", "0.97"),
                ("anthropic-ratelimit-unified-7d-reset", "1792537200"),
            ]),
        )
        .await;
    let snapshot = service.get(id).await.unwrap();
    assert!(snapshot.error.is_none());
    let window = |id: &str| snapshot.windows.iter().find(|w| w.id == id).unwrap();
    assert_eq!(window("five-hour").remaining_percent, Some(100.0));
    assert_eq!(window("seven-day").used, Some(97.0));
    assert_eq!(window("seven-day").remaining_percent, Some(3.0));
    assert_eq!(
        window("seven-day").reset_at.as_deref(),
        Some("2026-10-20T23:00:00Z")
    );
    // Windows the headers do not carry keep the usage read's numbers.
    assert_eq!(window("seven-day-opus").used, Some(20.0));
    // The streak ends and the poller waits a full interval again.
    assert!(service.failures.read().await.is_empty());
    let now = time::OffsetDateTime::now_utc();
    assert!(
        service
            .due(time::Duration::minutes(5), now)
            .await
            .is_empty()
    );
    // Other providers are left alone.
    let codex = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    let codex = store.get(&codex.id).await.unwrap();
    service
        .observe_claude_headers(
            &codex,
            &header(&[("anthropic-ratelimit-unified-7d-utilization", "0.5")]),
        )
        .await;
    assert!(service.get(&codex.id).await.is_none());
}

#[test]
fn credits_preserve_provider_units_balances_and_unknown_limits() {
    let codex = parse_credits(
        SubscriptionProvider::Codex,
        &json!({"credits":{"has_credits":true,"unlimited":false,"balance":"42.5"}}),
        observed(),
    );
    assert_eq!(codex[0].remaining, Some(42.5));
    assert_eq!(codex[0].unit, "credits");
    assert_eq!(codex[0].used, None);
    assert_eq!(codex[0].limit, None);
    let claude = parse_credits(
        SubscriptionProvider::Claude,
        &json!({"extra_usage":{"is_enabled":true,"monthly_limit":5000,"used_credits":1250,"utilization":25}}),
        observed(),
    );
    assert_eq!(claude[0].used, Some(12.5));
    assert_eq!(claude[0].limit, Some(50.0));
    assert_eq!(claude[0].remaining, Some(37.5));
    assert_eq!(claude[0].unit, "USD");
    assert_eq!(claude[0].reset_at, None);
    let grok = parse_credits(
        SubscriptionProvider::Xai,
        &json!({"config":{"monthlyLimit":{"val":1000},"used":{},"prepaidBalance":{"val":350},"onDemandCap":{"val":500},"currentPeriod":{"end":"2026-10-10T00:00:00Z"}}}),
        observed(),
    );
    assert_eq!(grok[0].used, Some(0.0));
    assert_eq!(grok[0].remaining, Some(10.0));
    assert_eq!(grok[1].used, None);
    assert_eq!(grok[1].remaining, None);
    assert_eq!(grok[2].remaining, Some(3.5));
    assert_eq!(grok[2].reset_at, None);
}

#[test]
fn credit_only_snapshots_and_old_snapshot_shapes_remain_valid() {
    let old: QuotaSnapshot = serde_json::from_value(json!({"account_id":"fixture","provider":"codex","windows":[],"updated_at":"2026-10-03T12:00:00Z","error":null})).unwrap();
    assert!(old.subscription.is_none());
    assert!(old.credits.is_empty());
    assert!(!old.verification_required);
    assert!(!old.verification_available);
    assert!(
        parse_credits(
            SubscriptionProvider::OpencodeGo,
            &json!({"usage":{"weekly":{"percent":10}}}),
            observed()
        )
        .is_empty()
    );
    let missing = parse_credits(
        SubscriptionProvider::Codex,
        &json!({"credits":{"unlimited":true}}),
        observed(),
    );
    assert_eq!(missing[0].remaining, None);
    assert_eq!(missing[0].unlimited, Some(true));
}

#[test]
fn opencode_go_accepts_relative_resets_from_the_native_usage_reader() {
    let windows = parse_opencode_go(
        &json!({"usage":{"weekly":{"percent":97,"resetInSec":7200}}}),
        observed(),
    );
    assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-03T14:00:00Z"));
}

#[test]
fn grok_proto3_zero_usage_is_available_when_current_period_is_reported() {
    let windows = parse_xai(
        &json!({"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-10-08T00:00:00Z"}}}),
        observed(),
    );
    assert_eq!(windows[0].remaining_percent, Some(100.0));
}

#[test]
fn grok_missing_period_type_does_not_invent_a_weekly_window() {
    let windows = parse_xai(
        &json!({"config":{"creditUsagePercent":12,"currentPeriod":{"end":"2026-10-08T00:00:00Z"}}}),
        observed(),
    );
    assert_eq!(windows[0].id, "window-period");
    assert_eq!(windows[0].remaining_percent, Some(88.0));
}

#[test]
fn gemini_model_buckets_keep_real_resets_and_unknown_amounts() {
    let windows = parse_gemini(
        &json!({"buckets":[
            {"modelId":"gemini-2.5-pro","tokenType":"REQUESTS","remainingFraction":0.25,"resetTime":"2026-10-04T12:00:00Z"},
            {"modelId":"gemini-2.5-flash","tokenType":"REQUESTS","resetTime":"2026-10-04T13:00:00Z"}
        ]}),
        observed(),
    );
    assert_eq!(windows[0].id, "model:gemini-2.5-pro:REQUESTS");
    assert_eq!(windows[0].remaining_percent, Some(25.0));
    assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-04T12:00:00Z"));
    assert_eq!(windows[1].remaining_percent, None);
    assert!(
        windows
            .iter()
            .all(|window| window.used.is_none() && window.limit.is_none())
    );
    let snapshot = QuotaSnapshot {
        account_id: "fixture".into(),
        provider: SubscriptionProvider::Gemini,
        windows,
        updated_at: now(),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    assert_eq!(snapshot.weekly_reset(), None);
    assert_eq!(snapshot.weekly_remaining_percent(), None);
}

#[test]
fn codex_percentages_and_reset_instants_come_from_the_same_windows() {
    let windows = parse_codex(
        &json!({"rate_limit":{"primary_window":{"used_percent":21,"reset_after_seconds":3600},
            "secondary_window":{"used_percent":"76.5","reset_at":1791633600,"limit_window_seconds":604800}},
            "code_review_rate_limit":{"primary_window":{"used_percent":100,"reset_at":1791633600000i64}}}),
        observed(),
    );
    assert_eq!(windows[0].remaining_percent, Some(79.0));
    assert_eq!(windows[0].reset_at.as_deref(), Some("2026-10-03T13:00:00Z"));
    assert_eq!(windows[1].remaining_percent, Some(23.5));
    assert_eq!(windows[1].reset_at, windows[2].reset_at);
    assert_eq!(windows[2].id, "code-review-five-hour");
}

#[test]
fn codex_primary_weekly_window_drives_weekly_routing() {
    let windows = parse_codex(
        &json!({"rate_limit":{
            "primary_window":{"used_percent":6,"reset_at":1791580593,"reset_after_seconds":560546,"limit_window_seconds":604800},
            "secondary_window":null
        }}),
        observed(),
    );
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].id, "weekly");
    assert_eq!(windows[0].label, "Codex Weekly");
    assert_eq!(windows[0].remaining_percent, Some(94.0));
    let snapshot = QuotaSnapshot {
        account_id: "fixture".into(),
        provider: SubscriptionProvider::Codex,
        windows,
        updated_at: now(),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    assert_eq!(snapshot.weekly_remaining_percent(), Some(94.0));
    assert_eq!(snapshot.weekly_reset(), Some("2026-10-09T21:16:33Z"));
}

#[test]
fn claude_missing_utilization_stays_unknown_instead_of_looking_available() {
    let windows = parse_claude(
        &json!({"five_hour":{"utilization":100,"resets_at":"2026-10-03T17:00:00Z"},
            "seven_day":{"resets_at":null},"seven_day_opus":{"utilization":42,"resets_at":"2026-10-07T12:00:00Z"}}),
        observed(),
    );
    assert_eq!(windows[0].remaining_percent, Some(0.0));
    assert_eq!(windows[1].remaining_percent, None);
    assert_eq!(windows[1].reset_at, None);
    assert_eq!(windows[2].remaining_percent, Some(58.0));
}

#[test]
fn grok_weekly_reset_does_not_borrow_a_monthly_billing_rollover() {
    let weekly = parse_xai(
        &json!({"config":{"creditUsagePercent":39.0,"currentPeriod":{"type":"weekly","start":"2026-10-01T00:00:00Z","end":"2026-10-08T00:00:00Z"}}}),
        observed(),
    );
    assert_eq!(weekly[0].remaining_percent, Some(61.0));
    assert_eq!(weekly[0].reset_at.as_deref(), Some("2026-10-08T00:00:00Z"));
    let monthly = parse_xai(
        &json!({"config":{"used":{"val":350},"monthlyLimit":{"val":1000},"billingPeriodEnd":"2026-11-01T00:00:00Z"}}),
        observed(),
    );
    assert_eq!(monthly[0].id, "monthly");
    assert_eq!(monthly[0].remaining_percent, Some(65.0));
    assert_eq!(monthly[0].reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
}

#[test]
fn opencode_go_reads_all_reported_subscription_windows() {
    let windows = parse_opencode_go(
        &json!({"usage":{"rolling":{"status":"ok","percent":18.5,"resetsAt":"2026-10-03T17:00:00Z"},
            "weekly":{"status":"rate-limited","percent":100,"resetsAt":"2026-10-09T00:00:00Z"},
            "monthly":{"status":"ok","percent":37,"resetsAt":"2026-11-03T00:00:00Z"}}}),
        observed(),
    );
    assert_eq!(windows.len(), 3);
    assert_eq!(windows[0].remaining_percent, Some(81.5));
    assert_eq!(windows[1].remaining_percent, Some(0.0));
    assert_eq!(windows[1].reset_at.as_deref(), Some("2026-10-09T00:00:00Z"));
}

#[test]
fn antigravity_fraction_and_kimi_counts_remain_real_observations() {
    let antigravity = parse_antigravity(
        &json!({"groups":[{"displayName":"Gemini","buckets":[{"bucketId":"gemini-5h","remainingFraction":"0.3","resetTime":"2026-10-03T15:00:00Z"}]}]}),
        observed(),
    );
    assert_eq!(antigravity[0].remaining_percent, Some(30.0));
    let kimi = parse_kimi(
        &json!({"limits":[{"name":"Requests","detail":{"used":6,"limit":10,"reset_in":120}}]}),
        observed(),
    );
    assert_eq!(kimi[0].remaining_percent, Some(40.0));
    assert_eq!(kimi[0].reset_at.as_deref(), Some("2026-10-03T12:02:00Z"));
}

#[tokio::test]
async fn claude_reset_survives_oauth_renewal() {
    claude_reset_through_refresh(false).await;
}

#[tokio::test]
async fn claude_reset_survives_a_failed_quota_read() {
    claude_reset_through_refresh(true).await;
}

async fn claude_reset_through_refresh(failed_read: bool) {
    use axum::{
        Json, Router,
        http::HeaderMap,
        routing::{get, post},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"claude", "access_token":"expired-fixture",
            "refresh_token":"fixture-refresh", "expires_at":"2020-01-01T00:00:00Z"}),
            None,
        )
        .await
        .unwrap();
    store
        .cooldown_with_retry_after(&account.id, 86_400, "upstream quota exceeded", true)
        .await
        .unwrap();
    let account = store.get(&account.id).await.unwrap();
    let reset = account.cooldown_until.clone().unwrap();
    let renewals = Arc::new(AtomicUsize::new(0));
    let count = renewals.clone();
    let app = Router::new()
            .route("/token", post(move || { let count = count.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"access_token":"renewed-fixture", "refresh_token":"rotated-fixture", "expires_in":3600}))
            }}))
            .route("/api/oauth/usage", get(move |headers: HeaderMap| { let reset = reset.clone(); async move {
                assert_eq!(headers["authorization"], "Bearer renewed-fixture");
                Json(json!({"seven_day":{"utilization":0, "resets_at":reset}}))
            }}))
            .route("/api/oauth/profile", get(|| async { Json(json!({})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let mut oauth = OAuthManager::new(client.clone(), store.clone());
    oauth.claude_token_url = format!("{base}/token");
    let mut service = QuotaService::with_oauth(client, store.clone(), Arc::new(oauth));
    service.claude_api = base;
    service
        .insert_for_test(QuotaSnapshot {
            account_id: account.id.clone(),
            provider: SubscriptionProvider::Claude,
            windows: parse_claude(
                &json!({"seven_day":{"utilization":100,"resets_at":account.cooldown_until}}),
                observed(),
            ),
            updated_at: timestamp(time::OffsetDateTime::now_utc() - time::Duration::minutes(1)),
            error: None,
            subscription: None,
            credits: vec![],
            verification_required: false,
            verification_available: false,
        })
        .await;
    if failed_read {
        service
            .complete_refresh(
                &account.id,
                &account,
                Err(SubscriptionError::Transport),
                Trigger::Manual,
            )
            .await
            .unwrap();
        assert!(service.get(&account.id).await.unwrap().error.is_some());
    }
    service.refresh(&account.id).await.unwrap();
    assert_eq!(renewals.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
    service.refresh(&account.id).await.unwrap();
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
    server.abort();
}

/// ChatGPT can revoke a Codex access token days before its `exp`. The
/// usage read then answers 401 until the token is renewed, so one forced
/// renewal must follow a 401 instead of showing it until expiry.
#[tokio::test]
async fn codex_usage_401_renews_the_token_once_and_reads_again() {
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
            .import(
                json!({"type":"codex", "access_token":"revoked-fixture", "account_id":"identity-fixture",
            "refresh_token":"fixture-refresh", "expires_at":"2099-01-01T00:00:00Z"}),
                None,
            )
            .await
            .unwrap();
    let renewals = Arc::new(AtomicUsize::new(0));
    let reads = Arc::new(AtomicUsize::new(0));
    let (count, read) = (renewals.clone(), reads.clone());
    let app = Router::new()
            .route("/token", post(move || { let count = count.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                Json(json!({"access_token":"renewed-fixture", "refresh_token":"rotated-fixture", "expires_in":864000}))
            }}))
            .route("/usage", get(move |headers: HeaderMap| { let read = read.clone(); async move {
                read.fetch_add(1, Ordering::SeqCst);
                if headers["authorization"] != "Bearer renewed-fixture" {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                Json(json!({"rate_limit":{"primary_window":{"used_percent":25,"limit_window_seconds":18000,"reset_after_seconds":600}}})).into_response()
            }}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let mut oauth = OAuthManager::new(client.clone(), store.clone());
    oauth.codex_token_url = format!("{base}/token");
    let mut service = QuotaService::with_oauth(client, store.clone(), Arc::new(oauth));
    service.codex_usage_url = format!("{base}/usage");
    let snapshot = service.refresh(&account.id).await.unwrap();
    assert_eq!(snapshot.error, None);
    assert_eq!(snapshot.windows[0].remaining_percent, Some(75.0));
    assert_eq!(renewals.load(Ordering::SeqCst), 1);
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    let renewed = store.get(&account.id).await.unwrap();
    assert_eq!(renewed.access_token, "renewed-fixture");
    assert_eq!(renewed.refresh_token.as_deref(), Some("rotated-fixture"));
    // A token the provider still refuses after renewal shows the refusal
    // and renews no more than once per read.
    let refusals = Arc::new(AtomicUsize::new(0));
    let refused_reads = refusals.clone();
    let app = Router::new().route(
        "/usage",
        get(move || {
            let refused_reads = refused_reads.clone();
            async move {
                refused_reads.fetch_add(1, Ordering::SeqCst);
                StatusCode::UNAUTHORIZED
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refused = format!("http://{}", listener.local_addr().unwrap());
    let second = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    service.codex_usage_url = format!("{refused}/usage");
    let snapshot = service.refresh(&account.id).await.unwrap();
    assert!(snapshot.error.unwrap().contains("HTTP 401"));
    assert_eq!(renewals.load(Ordering::SeqCst), 2);
    assert_eq!(refusals.load(Ordering::SeqCst), 2);
    server.abort();
    second.abort();
}

#[tokio::test]
async fn claude_confirmed_reset_releases_only_matching_cooldowns() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"claude","access_token":"fixture-access"}),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    store
        .cooldown_with_retry_after(&account.id, 86_400, "upstream quota exceeded", true)
        .await
        .unwrap();
    for family in ["fable", "opus"] {
        store
            .scoped_cooldown(
                &account.id,
                crate::subscriptions::cooldown::QuotaScope::Family(family.into()),
                86_400,
                true,
            )
            .await
            .unwrap();
    }
    store
        .modify(&account.id, |a| {
            let deadline = a.cooldown_until.clone().unwrap();
            for refusal in &mut a.quota.refusals {
                refusal.retry_at.clone_from(&deadline);
                refusal.retry_after_at = Some(deadline.clone());
            }
        })
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    let reset = before.cooldown_until.as_deref().unwrap();
    let previous = QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Claude,
        windows: parse_claude(
            &json!({
                "seven_day":{"utilization":100,"resets_at":reset},
                "iguana_necktie":{"utilization":100,"resets_at":reset},
                "seven_day_opus":{"utilization":50,"resets_at":reset}
            }),
            observed(),
        ),
        updated_at: timestamp(time::OffsetDateTime::now_utc() - time::Duration::minutes(1)),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    service.insert_for_test(previous.clone()).await;
    service.save().await;
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    // The reset can be redeemed outside Douane, with the same weekly deadline.
    let mut recovered = previous.clone();
    recovered.updated_at = now();
    for window in &mut recovered.windows {
        window.used = Some(0.0);
        window.remaining_percent = Some(100.0);
    }
    for case in [
        "partial",
        "missing",
        "deadline",
        "stale",
        "error",
        "provider",
        "account",
        "not-exhausted",
        "other-empty",
        "newer-refusal",
    ] {
        let mut old = service.get(&account.id).await.unwrap();
        let mut new = recovered.clone();
        match case {
            "partial" => new.windows[0].remaining_percent = Some(99.0),
            "missing" => {
                new.windows.remove(0);
            }
            "deadline" => {
                old.windows[0].reset_at = Some(timestamp(
                    time::OffsetDateTime::now_utc() + time::Duration::days(2),
                ))
            }
            "stale" => new.updated_at = old.updated_at.clone(),
            "error" => new.error = Some("quota read failed".into()),
            "provider" => new.provider = SubscriptionProvider::Codex,
            "account" => new.account_id = "another-account".into(),
            "not-exhausted" => old.windows[0].remaining_percent = Some(1.0),
            "other-empty" => {
                let mut empty = new.windows[0].clone();
                empty.id = "five-hour".into();
                empty.remaining_percent = Some(0.0);
                new.windows.push(empty);
            }
            _ => (),
        }
        service.insert_for_test(old.clone()).await;
        let mut started = store.get(&account.id).await.unwrap();
        if case == "newer-refusal" {
            started.quota.refusals.clear();
        }
        service
            .apply_observation(&account.id, &new, None, &started)
            .await
            .unwrap();
        let after = store.get(&account.id).await.unwrap();
        assert_eq!(after.cooldown_until, before.cooldown_until, "{case}");
        // Restore the refusal metadata, including scopes that a legitimate
        // family reset may have cleared while the global limit stayed put.
        store
            .modify(&account.id, |a| {
                a.quota = before.quota.clone();
            })
            .await
            .unwrap();
        service.insert_for_test(previous.clone()).await;
    }
    let started = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &recovered, None, &started)
        .await
        .unwrap();
    let reloaded = AccountStore::open(directory.path()).unwrap();
    let after = reloaded.get(&account.id).await.unwrap();
    assert!(
        after.cooldown_until.is_none(),
        "the confirmed global reset must reopen the account"
    );
    assert!(!after.cooldown_retry_after);
    assert!(after.last_error.is_none());
    assert!(after.serves("claude-fable-5-2"));
    assert!(!after.serves("claude-opus-5-5"));
    assert_eq!(after.scoped_cooldowns.len(), 1);
    assert_eq!(
        after.scoped_cooldowns[0].until,
        before.scoped_cooldowns[1].until
    );
}

/// A Codex weekly limit refused over the websocket rests the account for
/// at most a day, often before the window's own reset. When OpenAI resets
/// the window early, the next read showing it full reopens the account.
#[tokio::test]
async fn codex_confirmed_reset_releases_a_quota_cooldown() {
    for reason in [
        "websocket upstream quota exceeded",
        "upstream quota exceeded",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(AccountStore::open(directory.path()).unwrap());
        let account = store
            .import(
                json!({"type":"codex","access_token":"fixture","account_id":"a","name":"a"}),
                None,
            )
            .await
            .unwrap();
        let service = QuotaService::new(reqwest::Client::new(), store.clone());
        store.cooldown(&account.id, 86_400, reason).await.unwrap();
        store
            .modify(&account.id, |a| a.quota.refusals[0].legacy = true)
            .await
            .unwrap();
        let before = store.get(&account.id).await.unwrap();
        let in_days = |days| {
            Some(timestamp(
                time::OffsetDateTime::now_utc() + time::Duration::days(days),
            ))
        };
        let window = |left: f64, reset_at| QuotaWindow {
            id: "weekly".into(),
            label: "Codex Weekly".into(),
            remaining_percent: Some(left),
            reset_at,
            used: None,
            limit: None,
        };
        // The window was due to reset in four days; the cooldown stops at one.
        let previous = QuotaSnapshot {
            account_id: account.id.clone(),
            provider: SubscriptionProvider::Codex,
            windows: vec![window(0.0, in_days(4))],
            updated_at: timestamp(time::OffsetDateTime::now_utc() - time::Duration::minutes(1)),
            error: None,
            subscription: None,
            credits: Vec::new(),
            verification_required: false,
            verification_available: false,
        };
        let mut recovered = previous.clone();
        recovered.updated_at = now();
        recovered.windows = vec![window(100.0, in_days(7))];
        for case in [
            "partial",
            "not-exhausted",
            "earlier-reset",
            "retry-after",
            "stale",
            "error",
            "account",
            "newer-refusal",
        ] {
            let mut old = previous.clone();
            let mut new = recovered.clone();
            match case {
                "partial" => new.windows[0].remaining_percent = Some(99.0),
                "not-exhausted" => old.windows[0].remaining_percent = Some(1.0),
                // An exhausted window due before the deadline did not set it.
                "earlier-reset" => {
                    old.windows[0].reset_at = Some(timestamp(
                        time::OffsetDateTime::now_utc() + time::Duration::hours(1),
                    ))
                }
                "stale" => new.updated_at = old.updated_at.clone(),
                "error" => new.error = Some("quota read failed".into()),
                "account" => new.account_id = "another-account".into(),
                _ => (),
            }
            // An explicit Retry-After is exact: a later window reset does
            // not match it.
            store
                .modify(&account.id, |a| {
                    a.quota = before.quota.clone();
                    if case == "retry-after" {
                        let refusal = &mut a.quota.refusals[0];
                        refusal.provenance = crate::subscriptions::cooldown::Provenance::RetryAfter;
                        refusal.retry_after_at = Some(refusal.retry_at.clone());
                    }
                })
                .await
                .unwrap();
            service.insert_for_test(old).await;
            let mut started = store.get(&account.id).await.unwrap();
            if case == "newer-refusal" {
                started.quota.refusals.clear();
            }
            service
                .apply_observation(&account.id, &new, None, &started)
                .await
                .unwrap();
            let after = store.get(&account.id).await.unwrap();
            assert_eq!(
                after.cooldown_until, before.cooldown_until,
                "{reason}: {case}"
            );
            assert_eq!(after.last_error.as_deref(), Some(reason), "{case}");
        }
        store
            .modify(&account.id, |a| a.quota = before.quota.clone())
            .await
            .unwrap();
        service.insert_for_test(previous).await;
        // An unrelated change during the read, such as a renewed token,
        // keeps the evidence of the reset.
        let mut started = store.get(&account.id).await.unwrap();
        started.updated_at = "renewed-during-read".into();
        service
            .apply_observation(&account.id, &recovered, None, &started)
            .await
            .unwrap();
        let after = AccountStore::open(directory.path())
            .unwrap()
            .get(&account.id)
            .await
            .unwrap();
        assert!(
            after.cooldown_until.is_none(),
            "{reason}: reset must reopen"
        );
        assert!(after.last_error.is_none(), "{reason}");
    }
}

#[tokio::test]
async fn fresh_claude_quota_reopens_an_inferred_quota_cooldown() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"claude","access_token":"fixture-access"}),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let snapshot = QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Claude,
        windows: parse_claude(
            &json!({"seven_day":{"utilization":1,"resets_at":"2030-10-05T18:34:26Z"},
                    "limits":[{"kind":"weekly_scoped","percent":0,"resets_at":"2030-10-05T18:34:26Z",
                        "scope":{"model":{"display_name":"Fable"}}}]}),
            observed(),
        ),
        updated_at: now(),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    store
        .mark_cooldown(&account.id, 86_400, "upstream quota exceeded")
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &snapshot, None, &before)
        .await
        .unwrap();
    let recovered = store.get(&account.id).await.unwrap();
    assert_eq!(recovered.summary().status, "ready");
    assert!(recovered.cooldown_until.is_none());
    assert!(recovered.last_error.is_none());
    assert!(recovered.serves("claude-fable-5-2"));

    // A family-specific limit stays scoped to that family when the
    // account-wide cooldown is superseded by fresh global quota.
    let mut scoped = snapshot.clone();
    scoped.windows[1].remaining_percent = Some(0.0);
    store
        .mark_cooldown(&account.id, 86_400, "upstream quota exceeded")
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &scoped, None, &before)
        .await
        .unwrap();
    let recovered = store.get(&account.id).await.unwrap();
    assert_eq!(recovered.summary().status, "ready");
    assert!(!recovered.serves("claude-fable-5-2"));
    assert!(recovered.serves("claude-opus-5-5"));

    store
        .scoped_cooldown(
            &account.id,
            crate::subscriptions::cooldown::QuotaScope::Family("fable".into()),
            172_800,
            true,
        )
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    // A successful quota read may clear inferred global cooldowns, but
    // cannot shorten an explicit refusal for this model family.
    service
        .apply_observation(&account.id, &snapshot, None, &before)
        .await
        .unwrap();
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.summary().status, "ready");
    assert_eq!(
        after.scoped_cooldowns[0].until,
        before.scoped_cooldowns[0].until
    );
    assert!(!after.serves("claude-fable-5-2"));
    assert!(after.serves("claude-opus-5-5"));

    for case in [
        "empty",
        "unknown",
        "missing",
        "retry-after",
        "newer-refusal",
    ] {
        let mut control = snapshot.clone();
        match case {
            "empty" => control.windows[0].remaining_percent = Some(0.0),
            "unknown" => control.windows[0].remaining_percent = None,
            "missing" => {
                control.windows.remove(0);
            }
            _ => (),
        };
        let previous = store.get(&account.id).await.unwrap();
        store
            .cooldown_with_retry_after(
                &account.id,
                86_400,
                "upstream quota exceeded",
                case == "retry-after",
            )
            .await
            .unwrap();
        let cooling = store.get(&account.id).await.unwrap();
        let started = if case == "newer-refusal" {
            assert_ne!(previous.updated_at, cooling.updated_at);
            &previous
        } else {
            &cooling
        };
        service
            .apply_observation(&account.id, &control, None, started)
            .await
            .unwrap();
        let after = store.get(&account.id).await.unwrap();
        assert_eq!(after.cooldown_until, cooling.cooldown_until, "{case}");
        assert_eq!(after.last_error, cooling.last_error, "{case}");
    }
}

#[tokio::test]
async fn successful_quota_clears_an_older_error_without_active_cooldown() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"xai","access_token":"fixture-access","refresh_token":"fixture-refresh"}),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let snapshot = QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Xai,
        windows: parse_xai(
            &json!({"config":{"creditUsagePercent":57.0,"currentPeriod":{"type":"weekly","end":"2030-10-05T18:34:26Z"}}}),
            observed(),
        ),
        updated_at: now(),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    store
        .mark_error(&account.id, Some("fixture transient failure".into()))
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    assert_eq!(before.summary().status, "error");
    service
        .apply_observation(&account.id, &snapshot, None, &before)
        .await
        .unwrap();
    let recovered = store.get(&account.id).await.unwrap();
    assert_eq!(recovered.summary().status, "ready");
    assert!(recovered.last_error.is_none());
    assert_eq!(recovered.access_token, before.access_token);
    assert_eq!(recovered.refresh_token, before.refresh_token);
    assert!(
        AccountStore::open(directory.path())
            .unwrap()
            .get(&account.id)
            .await
            .unwrap()
            .last_error
            .is_none()
    );

    store
        .mark_cooldown(&account.id, 60, "provider request failed")
        .await
        .unwrap();
    let cooling = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &snapshot, None, &cooling)
        .await
        .unwrap();
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.summary().status, "cooldown");
    assert_eq!(after.cooldown_until, cooling.cooldown_until);
    assert_eq!(after.last_error, cooling.last_error);

    // An expired refusal disappears before a subsequent successful read.
    store
        .mark_cooldown(&account.id, 10, "upstream connection failed")
        .await
        .unwrap();
    store
        .modify(&account.id, |account| {
            for refusal in &mut account.quota.refusals {
                refusal.retry_at = "2020-01-01T00:00:00Z".into();
            }
        })
        .await
        .unwrap();
    let expired = store.get(&account.id).await.unwrap();
    assert_eq!(expired.summary().status, "ready");
    assert_eq!(expired.summary().last_error, None);
    service
        .apply_observation(&account.id, &snapshot, None, &expired)
        .await
        .unwrap();
    let recovered = store.get(&account.id).await.unwrap();
    assert_eq!(recovered.summary().status, "ready");
    assert!(recovered.last_error.is_none());

    store
        .modify(&account.id, |account| account.quota.refusals.clear())
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    store
        .mark_error(&account.id, Some("fixture newer failure".into()))
        .await
        .unwrap();
    let newer = store.get(&account.id).await.unwrap();
    assert_ne!(newer.updated_at, before.updated_at);
    service
        .apply_observation(&account.id, &snapshot, None, &before)
        .await
        .unwrap();
    assert_eq!(
        store.get(&account.id).await.unwrap().last_error,
        newer.last_error
    );
}

#[tokio::test]
async fn exhausted_global_window_blocks_selection_but_opus_window_only_blocks_opus() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let mut snapshot = QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Claude,
        windows: vec![QuotaWindow {
            id: "seven-day-opus".into(),
            label: "Opus".into(),
            remaining_percent: Some(0.0),
            reset_at: Some("2030-01-01T00:00:00Z".into()),
            used: Some(100.0),
            limit: Some(100.0),
        }],
        updated_at: now(),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    service
        .apply_observation(
            &account.id,
            &snapshot,
            None,
            &store.get(&account.id).await.unwrap(),
        )
        .await
        .unwrap();
    assert!(
        store
            .eligible(SubscriptionProvider::Claude, Some("claude-opus-4-6"))
            .await
            .is_empty()
    );
    assert_eq!(
        store
            .eligible(SubscriptionProvider::Claude, Some("claude-sonnet-4-6"))
            .await
            .len(),
        1
    );
    snapshot.windows[0].id = "five-hour".into();
    service
        .apply_observation(
            &account.id,
            &snapshot,
            None,
            &store.get(&account.id).await.unwrap(),
        )
        .await
        .unwrap();
    assert!(
        store
            .eligible(SubscriptionProvider::Claude, Some("claude-sonnet-4-6"))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn codex_usage_credits_keep_an_exhausted_account_selectable() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let usage = |credits: Value| {
        json!({
            "rate_limit": {
                "primary_window": {"used_percent": 100, "limit_window_seconds": 18000, "reset_at": 1_900_000_000},
                "secondary_window": {"used_percent": 100, "limit_window_seconds": 604800, "reset_at": 1_900_000_000}
            },
            "credits": credits
        })
    };
    let observe = |value: Value| QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Codex,
        windows: parse_codex(&value, observed()),
        updated_at: now(),
        error: None,
        subscription: None,
        credits: parse_credits(SubscriptionProvider::Codex, &value, observed()),
        verification_required: false,
        verification_available: false,
    };
    let later = parse_timestamp("2031-01-01T00:00:00Z").unwrap();
    let eligible = || async {
        store
            .eligible(SubscriptionProvider::Codex, Some("gpt-5.5"))
            .await
            .len()
    };
    for (credits, selectable) in [
        (
            json!({"has_credits":true,"unlimited":false,"balance":"42.5"}),
            true,
        ),
        (
            json!({"has_credits":true,"unlimited":true,"balance":null}),
            true,
        ),
        (
            json!({"has_credits":true,"unlimited":false,"balance":"0"}),
            false,
        ),
        (
            json!({"has_credits":false,"unlimited":false,"balance":"10"}),
            false,
        ),
        (Value::Null, false),
    ] {
        let snapshot = observe(usage(credits.clone()));
        assert!(snapshot.window_exhausted(observed()), "{credits}");
        assert_eq!(snapshot.credits_only(observed()), selectable, "{credits}");
        assert!(!snapshot.window_exhausted(later), "{credits}");
        service
            .apply_observation(
                &account.id,
                &snapshot,
                None,
                &store.get(&account.id).await.unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(eligible().await, usize::from(selectable), "{credits}");
        let status = store.get(&account.id).await.unwrap().summary().status;
        assert_eq!(status == "exhausted", !selectable, "{credits}");
    }
}

#[test]
fn claude_fallback_limit_kinds_do_not_collide_with_standard_windows() {
    let items: Vec<Value> = (0..8)
        .map(|i| json!({"utilization": (i + 1) * 10, "resets_at": "2030-01-01T05:00:00Z"}))
        .collect();
    let windows = parse_claude(&json!({"limits": items}), observed());
    assert!(
        windows
            .iter()
            .all(|w| w.id != "five-hour" && w.id != "seven-day"),
        "fallback limit-5/limit-7 must not masquerade as five-hour/seven-day: {windows:?}"
    );
    let upper = parse_claude(
        &json!({
            "limits": [
                {"kind": "Session", "utilization": 25, "resets_at": "2030-01-01T05:00:00Z"},
                {"kind": "Weekly", "utilization": 40, "resets_at": "2030-01-08T00:00:00Z"}
            ]
        }),
        observed(),
    );
    assert!(upper.iter().any(|w| w.id == "five-hour"));
    assert!(upper.iter().any(|w| w.id == "seven-day"));
}

#[tokio::test]
async fn meta_weekly_and_seven_day_window_stay_distinct() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"meta","access_token":"fixture","account_id":"m","name":"m"}),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    service
        .observe_meta_event(
            &account.id,
            &json!({
                "subscription": {
                    "weekly": {"used_percent": 100.0, "resets_at": 1893456000},
                    "window": {"used_percent": 50.0, "window_duration_mins": 10080, "resets_at": 1893456000}
                }
            }),
        )
        .await;
    let snapshot = service.get(&account.id).await.unwrap();
    assert_eq!(snapshot.windows.len(), 2, "{:?}", snapshot.windows);
    let weekly = snapshot.windows.iter().find(|w| w.id == "weekly").unwrap();
    let session = snapshot
        .windows
        .iter()
        .find(|w| w.id == "window-604800s")
        .unwrap();
    assert_eq!(weekly.remaining_percent, Some(0.0));
    assert_eq!(session.remaining_percent, Some(50.0));
}

#[tokio::test]
async fn fresh_zero_with_inherited_reset_is_recorded_for_confirmed_reset() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(AccountStore::open(directory.path()).unwrap());
    let account = store
        .import(
            json!({"type":"claude","access_token":"fixture","account_id":"c","name":"c"}),
            None,
        )
        .await
        .unwrap();
    let service = QuotaService::new(reqwest::Client::new(), store.clone());
    let reset = timestamp(time::OffsetDateTime::now_utc() + time::Duration::hours(4));
    let first = QuotaSnapshot {
        account_id: account.id.clone(),
        provider: SubscriptionProvider::Claude,
        windows: vec![QuotaWindow {
            id: "five-hour".into(),
            label: "5 hours".into(),
            remaining_percent: Some(20.0),
            reset_at: Some(reset.clone()),
            used: Some(80.0),
            limit: Some(100.0),
        }],
        updated_at: timestamp(time::OffsetDateTime::now_utc() - time::Duration::minutes(2)),
        error: None,
        subscription: None,
        credits: Vec::new(),
        verification_required: false,
        verification_available: false,
    };
    let started = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &first, None, &started)
        .await
        .unwrap();
    let zero_without_reset = QuotaSnapshot {
        windows: vec![QuotaWindow {
            id: "five-hour".into(),
            label: "5 hours".into(),
            remaining_percent: Some(0.0),
            reset_at: None,
            used: Some(100.0),
            limit: Some(100.0),
        }],
        updated_at: now(),
        ..first
    };
    let started = store.get(&account.id).await.unwrap();
    service
        .apply_observation(&account.id, &zero_without_reset, None, &started)
        .await
        .unwrap();
    let stored = store.get(&account.id).await.unwrap();
    let zero = stored
        .quota
        .zeros
        .get("five-hour")
        .expect("zero with inherited reset_at must be tracked");
    assert_eq!(zero.window.reset_at.as_deref(), Some(reset.as_str()));
}
