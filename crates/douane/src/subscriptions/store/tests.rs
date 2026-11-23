use super::*;

#[tokio::test]
async fn renewal_clears_only_auth_refusals_seen_on_the_old_token() {
    use crate::subscriptions::cooldown::{RefusalKind, Transport, classify_http};

    let dir = tempfile::tempdir().unwrap();
    let store = AccountStore::open(dir.path()).unwrap();
    let summary = store
        .import(json!({"type":"codex","access_token":"old-fixture"}), None)
        .await
        .unwrap();
    let auth = |delay: &str| {
        classify_http(
            SubscriptionProvider::Codex,
            "gpt-test",
            401,
            &[("retry-after".parse().unwrap(), delay.parse().unwrap())]
                .into_iter()
                .collect(),
            None,
            None,
            time::OffsetDateTime::now_utc(),
            Transport::Http,
        )
        .unwrap()
    };
    store
        .record_refusal(&summary.id, auth("300"))
        .await
        .unwrap();
    store
        .mark_cooldown(&summary.id, 3600, "upstream quota exceeded")
        .await
        .unwrap();
    let started = store.get(&summary.id).await.unwrap();
    let old_auth = started
        .quota
        .refusals
        .iter()
        .find(|r| r.kind == RefusalKind::Auth)
        .unwrap()
        .id;
    store
        .record_refusal(&summary.id, auth("600"))
        .await
        .unwrap();
    let before = store.get(&summary.id).await.unwrap();
    let mut renewed = started.clone();
    renewed.access_token = "renewed-fixture".into();
    store.update_credential(&started, renewed).await.unwrap();
    let reopened = AccountStore::open(dir.path()).unwrap();
    let current = reopened.get(&summary.id).await.unwrap();
    assert!(current.access_token == "renewed-fixture");
    let ids: Vec<_> = current.quota.refusals.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        before
            .quota
            .refusals
            .iter()
            .filter(|r| r.id != old_auth)
            .map(|r| r.id)
            .collect::<Vec<_>>()
    );
}
use serde_json::json;

#[tokio::test]
async fn legacy_quota_state_survives_restart_and_restore_discards_it() {
    use time::{Duration, OffsetDateTime};
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let path = directory.path().join(format!("{}.json", account.id));
    let mut record = serde_json::to_value(store.export().await.remove(0)).unwrap();
    let now = OffsetDateTime::now_utc();
    let until = timestamp(now + Duration::days(2));
    record.as_object_mut().unwrap().remove("quota");
    record["last_error"] = json!("upstream quota exceeded");
    record["cooldown_until"] = json!(until);
    record["cooldown_retry_after"] = json!(true);
    record["scoped_cooldowns"] =
        json!([{"scope":{"kind":"family","id":"fable"},"until":until,"retry_after":true}]);
    record["exhausted_until"] = json!(until);
    record["exhausted_without_reset"] = json!(true);
    record["model_exhaustion"] = json!({"opus":until});
    fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    drop(store);

    let store = AccountStore::open(directory.path()).unwrap();
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.summary().status, "exhausted");
    assert_eq!(after.quota.refusals.len(), 2);
    assert!(after.quota.refusals[0].id < after.quota.refusals[1].id);
    assert_eq!(
        after.summary().last_error.as_deref(),
        Some("upstream quota exceeded")
    );
    assert!(!after.serves("claude-opus-5-5"));
    let projected = serde_json::to_value(store.export().await.remove(0)).unwrap();
    for key in [
        "last_error",
        "cooldown_until",
        "cooldown_retry_after",
        "scoped_cooldowns",
        "exhausted_until",
        "exhausted_without_reset",
        "model_exhaustion",
    ] {
        assert_eq!(projected[key], record[key], "{key}");
    }
    store.restore(projected).await.unwrap();
    drop(store);

    let store = AccountStore::open(directory.path()).unwrap();
    let restored = store.get(&account.id).await.unwrap();
    assert_eq!(restored.summary().status, "ready");
    assert!(restored.last_error.is_none() && restored.quota.exhaustion.is_empty());
    assert!(restored.serves("claude-fable-5-2"));
    record["cooldown_until"] = json!(timestamp(now - Duration::hours(1)));
    record["scoped_cooldowns"] = json!([]);
    record["exhausted_until"] = Value::Null;
    record["exhausted_without_reset"] = json!(false);
    record["model_exhaustion"] = json!({});
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();
    drop(store);
    let store = AccountStore::open(directory.path()).unwrap();
    let expired = store.get(&account.id).await.unwrap();
    assert_eq!(expired.summary().status, "ready");
    assert!(expired.last_error.is_none());
}

#[tokio::test]
async fn a_failing_account_write_keeps_the_published_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let path = directory.path().join(format!("{}.json", account.id));
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        store
            .mark_cooldown(&account.id, 60, "upstream quota exceeded")
            .await,
        Err(SubscriptionError::Storage)
    ));
    let after = store.get(&account.id).await.unwrap();
    assert_eq!(after.summary().status, "ready");
    assert!(after.quota.refusals.is_empty());
}

#[test]
fn native_claude_tier_preserves_the_explicit_allowance_multiplier() {
    let account = parse_import(json!({"claudeAiOauth":{"accessToken":"fixture-private","subscriptionType":"max","rateLimitTier":"default_claude_max_20x"},"oauthAccount":{"organizationUuid":"fixture-organization"}}), None).unwrap();
    let tier = account.summary().subscription.unwrap();
    assert_eq!(tier.label, "Claude Max x20");
    assert_eq!(tier.multiplier, Some(20));
    assert_eq!(
        account.organization_id.as_deref(),
        Some("fixture-organization")
    );
    let public = serde_json::to_string(&account.summary()).unwrap();
    assert!(!public.contains("fixture-private"));
    assert!(!public.contains("fixture-organization"));
}

#[test]
fn tier_detection_uses_provider_metadata_and_keeps_unknowns_unknown() {
    let claude = subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_5x"}}), "provider_profile").unwrap();
    assert_eq!(claude.label, "Claude Max x5");
    // Anthropic renamed Max x20 to Ultra: the profile names the plan and
    // the rate tier still carries the allowance multiplier.
    let ultra = subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"claude_max","rate_limit_tier":"default_claude_max_20x","plan_display_name":"Ultra"}}), "provider_profile").unwrap();
    assert_eq!(
        (ultra.id.as_str(), ultra.label.as_str(), ultra.multiplier),
        ("ultra", "Claude Ultra", Some(20))
    );
    let pro = subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"claude_pro","rate_limit_tier":"default_claude_ai"}}), "provider_profile").unwrap();
    assert_eq!(
        (pro.id.as_str(), pro.label.as_str(), pro.multiplier),
        ("pro", "Claude Pro", None)
    );
    let codex = subscription_from_value(
        SubscriptionProvider::Codex,
        &json!({"plan_type":"pro"}),
        "provider_usage",
    )
    .unwrap();
    assert_eq!(codex.label, "Codex Pro");
    assert_eq!(codex.multiplier, None);
    // An account file saved before default_weight existed gets it back.
    let mut stored = parse_import(json!({"type":"codex","access_token":"fixture"}), None).unwrap();
    stored.subscription = Some(SubscriptionTier {
        default_weight: None,
        ..codex.clone()
    });
    assert_eq!(
        stored.summary().subscription.unwrap().default_weight,
        Some(20)
    );
    // OpenAI publishes Pro at 20x Plus: the pool weight, never the label.
    assert_eq!(codex.default_weight, Some(20));
    assert_eq!(codex.weight(SubscriptionProvider::Codex), 20.0);
    assert_eq!(
        subscription_from_value(
            SubscriptionProvider::Codex,
            &json!({"plan_type":"pro","usage_multiplier":20}),
            "provider_usage"
        )
        .unwrap()
        .multiplier,
        Some(20)
    );
    assert_eq!(
        subscription_from_value(
            SubscriptionProvider::Xai,
            &json!({"subscription_tier_display":"SuperGrok Heavy"}),
            "provider_settings"
        )
        .unwrap()
        .label,
        "SuperGrok Heavy"
    );
    assert_eq!(
        subscription_from_value(
            SubscriptionProvider::Xai,
            &json!({"subscription_tier":"premium"}),
            "provider_settings"
        )
        .unwrap()
        .label,
        "X Premium"
    );
    assert_eq!(
        subscription_from_value(
            SubscriptionProvider::Xai,
            &json!({"subscription_tier_display":"X Premium+"}),
            "provider_settings"
        )
        .unwrap()
        .label,
        "X Premium+"
    );
    assert!(subscription_from_value(SubscriptionProvider::Claude, &json!({"organization":{"organization_type":"unknown-fixture","rate_limit_tier":"unknown-private-value"}}), "provider_profile").is_none());
}

#[tokio::test]
async fn an_account_file_saved_before_plan_weights_loads_with_them() {
    // Quota snapshots copy the stored tier as is, so the account file
    // itself must come back with the published ratio.
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let imported = store
        .import(
            json!({"type":"codex","access_token":"fixture-private"}),
            None,
        )
        .await
        .unwrap();
    let path = directory.path().join(format!("{}.json", imported.id));
    let mut saved: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    saved["subscription"] =
        json!({"id":"pro","label":"Codex Pro","multiplier":null,"source":"provider_usage"});
    fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    let reopened = AccountStore::open(directory.path()).unwrap();
    let account = reopened.get(&imported.id).await.unwrap();
    assert_eq!(account.subscription.unwrap().default_weight, Some(20));
}

#[tokio::test]
async fn account_update_is_atomic_persisted_and_survives_credential_refresh() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let imported = store.import(json!({"type":"claude","access_token":"fixture-private","account_id":"fixture-account"}), None).await.unwrap();
    let stale = store.get(&imported.id).await.unwrap();
    let updated = store
        .update(
            &imported.id,
            Some("New label".into()),
            Some(false),
            None,
            Some(Some(5.0)),
            Some(vec!["weekly".into(), "five-hour".into(), "weekly".into()]),
        )
        .await
        .unwrap();
    assert_eq!(updated.name, "New label");
    // Watched kinds are stored once each, shortest first.
    assert_eq!(updated.reserve_windows, ["five-hour", "weekly"]);
    assert!(!updated.enabled);
    assert_eq!(updated.reserve_percent, Some(5.0));
    store
        .update_credential(&stale, stale.clone())
        .await
        .unwrap();
    let reopened = AccountStore::open(directory.path()).unwrap();
    assert_eq!(reopened.list().await[0].name, "New label");
    assert!(!reopened.list().await[0].enabled);
    assert_eq!(reopened.list().await[0].reserve_percent, Some(5.0));
    assert_eq!(
        reopened.list().await[0].reserve_windows,
        ["five-hour", "weekly"]
    );
    // A reimport keeps the reserve; an explicit removal clears it.
    reopened.import(json!({"type":"claude","access_token":"rotated-fixture","account_id":"fixture-account"}), None).await.unwrap();
    assert_eq!(reopened.list().await[0].reserve_percent, Some(5.0));
    assert_eq!(
        reopened.list().await[0].reserve_windows,
        ["five-hour", "weekly"]
    );
    assert!(matches!(
        reopened
            .update(
                &imported.id,
                None,
                None,
                None,
                None,
                Some(vec!["hourly".into()])
            )
            .await,
        Err(SubscriptionError::InvalidReserveWindows)
    ));
    let longest = reopened
        .update(&imported.id, None, None, None, None, Some(vec![]))
        .await
        .unwrap();
    assert!(longest.reserve_windows.is_empty());
    let cleared = reopened
        .update(&imported.id, None, None, None, Some(None), None)
        .await
        .unwrap();
    assert_eq!(cleared.reserve_percent, None);
    // The store refuses a reserve outside 0 < percent < 100.
    for invalid in [-1.0, 0.0, 100.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            reopened
                .update(&imported.id, None, None, None, Some(Some(invalid)), None)
                .await,
            Err(SubscriptionError::InvalidReserve)
        ));
    }
    assert_eq!(reopened.list().await[0].reserve_percent, None);
}

#[test]
fn account_storage_without_metadata_fields_remains_compatible() {
    let old: AccountCredential = serde_json::from_value(json!({"id":"fixture","name":"Old account","provider":"codex","access_token":"private-fixture","plan":"pro","enabled":true,"created_at":"2026-10-03T12:00:00Z","updated_at":"2026-10-03T12:00:00Z"})).unwrap();
    assert!(old.subscription.is_none());
    assert!(old.organization_id.is_none());
    assert_eq!(old.summary().subscription.unwrap().label, "Codex Pro");
}

#[tokio::test]
async fn reimport_without_a_label_preserves_the_renamed_account() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(
            json!({"type":"codex","access_token":"fixture","account_id":"fixture-account"}),
            None,
        )
        .await
        .unwrap();
    store
        .modify(&account.id, |account| account.name = "Renamed".into())
        .await
        .unwrap();
    let reimported = store
        .import(
            json!({"type":"codex","access_token":"rotated-fixture","account_id":"fixture-account"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(reimported.name, "Renamed");
}

#[tokio::test]
async fn a_hidden_account_stays_hidden_and_routable_through_reload_and_reimport() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let fixture = json!({"type":"codex","access_token":"fixture","account_id":"fixture-account"});
    let account = store.import(fixture, None).await.unwrap();
    assert!(!account.hidden);
    let hidden = store
        .update(&account.id, None, None, Some(true), None, None)
        .await
        .unwrap();
    assert!(hidden.hidden && hidden.enabled);
    assert_eq!(
        store
            .eligible(SubscriptionProvider::Codex, None)
            .await
            .len(),
        1
    );
    let reopened = AccountStore::open(directory.path()).unwrap();
    assert!(reopened.list().await[0].hidden);
    let rotated = json!({"type":"codex","access_token":"rotated","account_id":"fixture-account"});
    assert!(reopened.import(rotated, None).await.unwrap().hidden);
    let shown = reopened
        .update(&account.id, None, None, Some(false), None, None)
        .await
        .unwrap();
    assert!(!shown.hidden);
    let raw = fs::read_to_string(directory.path().join(format!("{}.json", account.id))).unwrap();
    assert!(!raw.contains("hidden"));
}

#[tokio::test]
async fn imports_native_credentials_without_exposing_tokens_and_persists_disabled_state() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let summary = store.import(json!({"tokens":{"access_token":"fixture-access-private","refresh_token":"fixture-refresh-private","account_id":"account-1"}}), Some("Personal".into())).await.unwrap();
    let public = serde_json::to_string(&summary).unwrap();
    assert!(!public.contains("fixture-access-private"));
    assert!(!public.contains("fixture-refresh-private"));
    store.set_enabled(&summary.id, false).await.unwrap();
    assert!(
        store
            .eligible(SubscriptionProvider::Codex, None)
            .await
            .is_empty()
    );
    let reopened = AccountStore::open(directory.path()).unwrap();
    assert_eq!(reopened.list().await[0].status, "disabled");
    let imported = reopened
        .import(
            json!({"type":"codex","access_token":"rotation","account_id":"account-1"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(imported.id, summary.id);
    assert!(!imported.enabled);
    assert_eq!(reopened.list().await.len(), 1);
}

#[test]
fn imports_claude_native_and_cli_proxy_gemini_shapes() {
    let claude = parse_import(json!({"claudeAiOauth":{"accessToken":"fixture","refreshToken":"refresh","expiresAt":2_000_000_000_000i64,"subscriptionType":"max"}}), None).unwrap();
    assert_eq!(claude.provider, SubscriptionProvider::Claude);
    assert_eq!(claude.plan.as_deref(), Some("max"));
    assert!(
        claude
            .expires_at
            .as_deref()
            .and_then(parse_timestamp)
            .is_some()
    );
    let gemini = parse_import(json!({"type":"gemini","token":{"access_token":"fixture","refresh_token":"refresh","expiry":"2030-01-01T00:00:00Z"},"project_id":"project"}), None).unwrap();
    assert_eq!(gemini.project_id.as_deref(), Some("project"));
    let grok = parse_import(json!({"https://auth.x.ai::b1a00492-073a-47ea-816f-4c329264a828":{
            "key":"native-fixture", "refresh_token":"refresh", "user_id":"native-subject", "email":"fixture@example.test", "expires_at":2_000_000_000i64
        }}), None).unwrap();
    assert_eq!(grok.provider, SubscriptionProvider::Xai);
    assert_eq!(grok.account_id.as_deref(), Some("native-subject"));
    assert_eq!(
        grok.base_url.as_deref(),
        Some("https://cli-chat-proxy.grok.com/v1")
    );
}

#[test]
fn rejects_unrecognized_provider_without_echoing_untrusted_values() {
    let error = parse_import(
        json!({"type":"secret-in-type","access_token":"secret-in-access"}),
        None,
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("secret-in"));
}

#[test]
fn native_codex_expiration_uses_access_token_while_identity_uses_id_token() {
    let jwt = |claims: Value| {
        format!(
            "e30.{}.fixture",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    };
    let expired_identity = jwt(json!({"email":"fixture@example.test","exp":1_600_000_000}));
    let valid_access = jwt(json!({"exp":2_000_000_000}));
    let account = parse_import(json!({"tokens":{
            "id_token":expired_identity,"access_token":valid_access,"refresh_token":"fixture-refresh"
        }}),None).unwrap();
    assert_eq!(account.email.as_deref(), Some("fixture@example.test"));
    assert_eq!(
        account.expires_at,
        time::OffsetDateTime::from_unix_timestamp(2_000_000_000)
            .ok()
            .map(timestamp)
    );
    assert!(!account.needs_refresh());

    let opaque_access = parse_import(json!({"type":"codex","id_token":jwt(json!({"exp":1_600_000_000})),"access_token":"opaque-fixture"}),None).unwrap();
    assert_eq!(opaque_access.expires_at, None);
    assert_eq!(opaque_access.summary().status, "ready");
}

#[tokio::test]
async fn cooldown_excludes_an_account_until_it_expires() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    store
        .mark_cooldown(&account.id, 30, "rate limited")
        .await
        .unwrap();
    assert!(
        store
            .eligible(SubscriptionProvider::Codex, None)
            .await
            .is_empty()
    );
    store
        .modify(&account.id, |a| {
            a.quota.refusals[0].retry_at = "2020-01-01T00:00:00Z".into()
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .eligible(SubscriptionProvider::Codex, None)
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn explicit_retry_after_survives_restart_without_the_inferred_delay_cap() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    let before = time::OffsetDateTime::now_utc();
    store
        .cooldown_with_retry_after(&account.id, 172_800, "upstream rate limit reached", true)
        .await
        .unwrap();
    let reopened = AccountStore::open(directory.path()).unwrap();
    let cooling = reopened.get(&account.id).await.unwrap();
    let until = parse_timestamp(cooling.cooldown_until.as_deref().unwrap()).unwrap();
    assert!(until >= before + time::Duration::days(2));
    assert!(!cooling.resting());
    assert!(
        reopened
            .eligible(SubscriptionProvider::Codex, None)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn scoped_retry_delays_survive_renewal_and_coexist_with_global_limits() {
    use super::super::cooldown::QuotaScope;
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"claude","access_token":"fixture"}), None)
        .await
        .unwrap();
    let original = store.get(&account.id).await.unwrap();
    store
        .scoped_cooldown(
            &account.id,
            QuotaScope::Family("fable".into()),
            172_800,
            true,
        )
        .await
        .unwrap();
    store
        .scoped_cooldown(&account.id, QuotaScope::Family("sonnet".into()), 300, true)
        .await
        .unwrap();
    let before = store.get(&account.id).await.unwrap();
    store
        .scoped_cooldown(&account.id, QuotaScope::Family("fable".into()), 10, false)
        .await
        .unwrap();
    store
        .update_credential(&original, original.clone())
        .await
        .unwrap();
    let refreshed = store.get(&account.id).await.unwrap();
    assert_eq!(refreshed.scoped_cooldowns.len(), 2);
    assert_eq!(
        refreshed.scoped_cooldowns[0].until,
        before.scoped_cooldowns[0].until
    );
    assert!(
        parse_timestamp(&refreshed.scoped_cooldowns[0].until).unwrap()
            > time::OffsetDateTime::now_utc() + time::Duration::hours(47)
    );
    assert!(!refreshed.serves("claude-fable-5-1"));
    assert!(!refreshed.serves("claude-sonnet-5-5"));
    assert!(refreshed.serves("claude-opus-5-5"));
    store
        .cooldown_with_retry_after(&account.id, 600, "upstream rate limit reached", true)
        .await
        .unwrap();
    let restarted = AccountStore::open(directory.path()).unwrap();
    assert!(
        restarted
            .eligible(SubscriptionProvider::Claude, Some("claude-opus-5-5"))
            .await
            .is_empty()
    );
    let mut cooling = restarted.get(&account.id).await.unwrap();
    assert_eq!(cooling.scoped_cooldowns.len(), 2);
    for refusal in &mut cooling.quota.refusals {
        if refusal.scope == QuotaScope::Family("fable".into()) {
            refusal.retry_at = "2000-01-01T00:00:00Z".into();
        }
    }
    assert!(
        cooling.serves("claude-fable-5-2"),
        "only the expired scoped delay clears"
    );
    assert!(!cooling.serves("claude-sonnet-5-5"));
}

#[tokio::test]
async fn an_expired_cooldown_leaves_no_account_error() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    store
        .cooldown(&account.id, 30, "upstream temporarily unavailable")
        .await
        .unwrap();
    let cooling = store.get(&account.id).await.unwrap();
    assert_eq!(cooling.summary().status, "cooldown");
    assert!(cooling.last_error.is_some());

    store
        .modify(&account.id, |a| {
            a.quota.refusals[0].retry_at = "2020-01-01T00:00:00Z".into()
        })
        .await
        .unwrap();
    assert_eq!(
        store.get(&account.id).await.unwrap().summary().status,
        "ready"
    );
    let cleared = store.get(&account.id).await.unwrap();
    assert_eq!(cleared.summary().status, "ready");
    assert!(cleared.last_error.is_none() && cleared.cooldown_until.is_none());
    let reopened = AccountStore::open(directory.path()).unwrap();
    assert!(
        reopened
            .get(&account.id)
            .await
            .unwrap()
            .last_error
            .is_none()
    );
}

#[tokio::test]
async fn a_legacy_global_cooldown_without_a_reason_is_not_a_retryable_rate_limit() {
    let directory = tempfile::tempdir().unwrap();
    let store = AccountStore::open(directory.path()).unwrap();
    let account = store
        .import(json!({"type":"codex","access_token":"fixture"}), None)
        .await
        .unwrap();
    let path = directory.path().join(format!("{}.json", account.id));
    let mut record = serde_json::to_value(store.export().await.remove(0)).unwrap();
    let fields = record.as_object_mut().unwrap();
    fields.remove("quota");
    fields.remove("last_error");
    record["cooldown_until"] = json!(timestamp(
        time::OffsetDateTime::now_utc() + time::Duration::hours(1)
    ));
    fs::write(path, serde_json::to_vec(&record).unwrap()).unwrap();

    let reopened = AccountStore::open(directory.path()).unwrap();
    let account = reopened.get(&account.id).await.unwrap();
    assert_eq!(account.summary().status, "cooldown");
    assert!(!account.resting());
}
