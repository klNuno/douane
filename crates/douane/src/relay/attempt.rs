use super::*;
use crate::subscriptions::cooldown::{Transport, classify_http, google_refusal};
use time::OffsetDateTime;
/// A signed Gemini turn whose owner rests: the client waits for that account.
pub(super) fn owner_rate_limit(seconds: u64) -> Response {
    let mut response = error(
        StatusCode::TOO_MANY_REQUESTS,
        "the Antigravity account that signed this conversation is temporarily unavailable; retry after Retry-After",
    );
    response.headers_mut().insert(
        header::RETRY_AFTER,
        seconds.max(1).to_string().parse().unwrap(),
    );
    response
}

pub async fn forward(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (request, candidates) = match RelayRequest::parse(&state, uri.path(), headers, body).await {
        Ok(request) => request,
        Err(answer) => return answer,
    };
    // This bounds each wait on upstream. A long stream may keep sending events.
    let idle = Duration::from_secs(state.config.read().await.settings.request_timeout_seconds);
    let mut last_status = StatusCode::BAD_GATEWAY;
    let mut last_native = None;
    let mut last_reason = None;
    for candidate in candidates {
        if request.owner.is_some() && candidate.blocked(&request.model) {
            return owner_rate_limit(retry_seconds(&candidate, &request.model));
        }
        let mut account = match state.oauth.credential(&candidate.id).await {
            Ok(account) => account,
            Err(_) => {
                let _ = state
                    .accounts
                    .cooldown(&candidate.id, 300, "authentication needs reconnecting")
                    .await;
                continue;
            }
        };
        let upstream = match request.prepare(&state, &account).await {
            Ok(upstream) => upstream,
            Err(answer) => return answer,
        };
        let mut call = CallGuard::new(state.ledger.clone(), &account, &request.model)
            .for_client(&state.clients, request.client.as_ref());
        let (response, received) = match send(&state, &request, &upstream, &mut account, idle).await
        {
            Ok(receipt) => receipt,
            Err((reason, received)) => {
                call.fail(reason);
                let refusal = crate::subscriptions::cooldown::Refusal::timed(
                    received,
                    10,
                    reason,
                    crate::subscriptions::cooldown::QuotaScope::Global,
                    false,
                );
                let _ = state.accounts.record_refusal(&account.id, refusal).await;
                continue;
            }
        };
        last_status = response.status();
        call.call.status = last_status.as_u16();
        state.observe_usage(&account.id).await;
        let passed = if upstream.native {
            passthrough_headers(response.headers())
        } else {
            HeaderMap::new()
        };
        if last_status.is_success() {
            return success(
                &state, &request, upstream, account, response, passed, call, idle,
            )
            .await;
        }
        match refusal(
            &state, &request, &upstream, &account, response, passed, &mut call, idle, received,
        )
        .await
        {
            Attempt::Finish(answer) => return answer,
            Attempt::Retry(native, reason) => {
                if native.is_some() {
                    last_native = native;
                }
                if reason.is_some() {
                    last_reason = reason;
                }
            }
        }
    }
    if let Some(refusal) = last_native {
        return refusal;
    }
    match last_reason {
        Some(reason) => error(
            last_status,
            &format!(
                "all accounts for this provider are unavailable; last upstream refusal: {reason}"
            ),
        ),
        None => error(
            last_status,
            "all accounts for this provider are unavailable; check their quotas and connection status",
        ),
    }
}

enum Attempt {
    Finish(Response),
    Retry(Option<Response>, Option<String>),
}

async fn send(
    state: &AppState,
    request: &RelayRequest,
    upstream: &UpstreamRequest,
    account: &mut AccountCredential,
    idle: Duration,
) -> Result<(reqwest::Response, OffsetDateTime), (&'static str, OffsetDateTime)> {
    let UpstreamRequest {
        url,
        bytes,
        request_headers,
        ..
    } = upstream;
    let headers = &request.headers;
    let mut retry_auth = account.provider != SubscriptionProvider::OpenaiCompatible;
    loop {
        let send = state
            .client
            .post(url)
            .headers({
                let mut sent = upstream_headers(account, headers);
                sent.extend(request_headers.clone());
                sent
            })
            .body(bytes.clone())
            .send();
        let result = tokio::time::timeout(idle, send).await;
        let received = OffsetDateTime::now_utc();
        let result = match result {
            Ok(Ok(response)) => {
                state
                    .quotas
                    .observe_response_headers_at(
                        account,
                        response.headers(),
                        received,
                        response.status().is_success(),
                    )
                    .await;
                Ok((response, received))
            }
            Ok(Err(_)) => Err(("upstream connection failed", received)),
            Err(_) => Err(("upstream request timed out", received)),
        };
        match result {
            Ok((r, received)) if r.status() == StatusCode::UNAUTHORIZED && retry_auth => {
                retry_auth = false;
                match state
                    .oauth
                    .renew_refused(&account.id, &account.access_token)
                    .await
                {
                    Ok(fresh) => *account = fresh,
                    Err(_) => break Ok((r, received)),
                }
            }
            result => break result,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn success(
    state: &Arc<AppState>,
    request: &RelayRequest,
    upstream: UpstreamRequest,
    account: AccountCredential,
    response: reqwest::Response,
    passed: HeaderMap,
    mut call: CallGuard,
    idle: Duration,
) -> Response {
    let RelayRequest { model, session, .. } = request;
    let (from, stream) = (request.from, request.stream);
    let UpstreamRequest {
        to,
        codex_sse,
        custom,
        ..
    } = upstream;
    if let Some(key) = session {
        state
            .affinity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .bind(account.pool(), key, &account.id);
    }
    // Codex was asked for a stream above, and its backend can answer one
    // without a content type.
    let is_sse = codex_sse
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
    if stream && is_sse {
        let mut streamed = stream_response(
            response,
            from,
            to,
            call,
            idle,
            custom,
            state.clone(),
            account,
        );
        streamed.headers_mut().extend(passed);
        return streamed;
    }
    let result = if is_sse {
        collect_response(response, to, &mut call, idle, state, &account).await
    } else {
        match read_bounded(response, 32 * 1024 * 1024, idle).await {
            Ok(b) => serde_json::from_slice::<Value>(&b)
                .map_err(|_| "upstream returned invalid JSON".into()),
            Err(e) => Err(e),
        }
    };
    let mut result = match result {
        Ok(v) => v,
        Err(e) => {
            call.fail(&e);
            return error(StatusCode::BAD_GATEWAY, &e);
        }
    };
    let refusal = if to == Dialect::Gemini {
        translate::gemini_refusal(&result)
    } else {
        None
    };
    if let Some(message) = refusal {
        if from == Dialect::Gemini {
            // A Gemini API client reads Google's refusal as Google wrote it.
            call.observe(&result);
            call.fail(message);
            let mut answer = Json(unwrap_gemini(result)).into_response();
            answer.headers_mut().extend(passed);
            return answer;
        }
        call.fail(message);
        return error(StatusCode::BAD_GATEWAY, message);
    }
    if to == Dialect::Gemini && !gemini_finished(&result) {
        call.fail("upstream response did not contain a Gemini completion");
        return error(
            StatusCode::BAD_GATEWAY,
            "upstream response did not contain a Gemini completion",
        );
    }
    observe_frame(
        state,
        &account,
        &mut call,
        &mut result,
        OffsetDateTime::now_utc(),
    )
    .await;
    if call.call.error.is_some() {
        return error(
            StatusCode::BAD_GATEWAY,
            "upstream reported an error in its response",
        );
    }
    let mut result = match translate::response(&result, to, from, model, &custom) {
        Ok(v) => v,
        Err(e) => {
            call.fail(&e);
            return error(StatusCode::BAD_GATEWAY, &e);
        }
    };
    if from == Dialect::Gemini {
        result = unwrap_gemini(result);
    }
    call.complete = true;
    state.accounts.clear_error_for(&account).await;
    let mut answer = Json(result).into_response();
    answer.headers_mut().extend(passed);
    return answer;
}

#[allow(clippy::too_many_arguments)]
async fn refusal(
    state: &AppState,
    request: &RelayRequest,
    upstream: &UpstreamRequest,
    account: &AccountCredential,
    response: reqwest::Response,
    passed: HeaderMap,
    call: &mut CallGuard,
    idle: Duration,
    received: OffsetDateTime,
) -> Attempt {
    let RelayRequest { model, owner, .. } = request;
    let (to, native) = (upstream.to, upstream.native);
    let last_status = response.status();
    let limit_headers = response.headers().clone();
    // Copilot refuses a request it cannot serve with 400; the account
    // is fine, and a model its plan lacks leaves the list.
    if account.provider == SubscriptionProvider::Copilot && last_status == StatusCode::BAD_REQUEST {
        let body = read_bounded(response, 64 * 1024, idle)
            .await
            .unwrap_or_default();
        let refusal: Option<Value> = serde_json::from_slice(&body).ok();
        let code = refusal
            .as_ref()
            .and_then(|v| v.pointer("/error/code"))
            .and_then(Value::as_str);
        if code == Some("model_not_supported") {
            const UNSERVED: &str =
                "GitHub Copilot does not serve this model on this account's plan";
            call.fail(UNSERVED);
            state
                .model_refused(account.provider, &account.id, model)
                .await;
            return Attempt::Retry(None, Some(UNSERVED.into()));
        }
        call.fail("upstream request refused");
        tracing::warn!(
            provider = account.pool(),
            model = %model,
            status = last_status.as_u16(),
            error = call.call.error.as_deref().unwrap_or_default(),
            "upstream refused the request"
        );
        if native && refusal.is_some() {
            let mut answer = (last_status, Body::from(body)).into_response();
            answer.headers_mut().extend(passed);
            answer.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("application/json"),
            );
            return Attempt::Finish(answer);
        }
        return Attempt::Finish(error(last_status, "upstream request refused"));
    }
    let limit = if to == Dialect::Gemini {
        64 * 1024
    } else {
        256 * 1024
    };
    let body = read_bounded(response, limit, idle).await.ok();
    let google = if to == Dialect::Gemini {
        body.as_deref().and_then(|body| google_refusal(body, model))
    } else {
        None
    };
    let parsed = body
        .as_deref()
        .and_then(|body| serde_json::from_slice::<Value>(body).ok());
    let diagnostic = parsed.as_ref().and_then(crate::refusal::image_limit);
    // An Antigravity plan answers NOT_FOUND for a model it lacks
    // (the newest Claude on a trial or partner plan) while another
    // account of the pool may serve it: the account itself is fine.
    if account.provider == SubscriptionProvider::Antigravity && last_status == StatusCode::NOT_FOUND
    {
        const UNSERVED: &str = "this Antigravity account does not serve this model";
        call.fail(UNSERVED);
        state
            .model_refused(account.provider, &account.id, model)
            .await;
        return Attempt::Retry(None, Some(UNSERVED.into()));
    }
    let summary = match last_status {
        StatusCode::PAYMENT_REQUIRED => "upstream API budget exhausted",
        StatusCode::TOO_MANY_REQUESTS => "upstream quota or rate limit reached",
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => "upstream authentication refused",
        _ => "upstream request refused",
    };
    if let Some(diagnostic) = &diagnostic {
        call.fail(&format!("{summary}: {diagnostic}"));
    } else {
        call.fail(summary);
    }
    let google_reason = google
        .as_ref()
        .filter(|google| !google.reason.is_empty())
        .map(|google| google.reason.clone());
    tracing::warn!(
        provider = account.pool(),
        model = %model,
        status = last_status.as_u16(),
        "upstream refused the request"
    );
    if let Some(mut refusal) = classify_http(
        account.provider,
        model,
        last_status.as_u16(),
        &limit_headers,
        parsed.as_ref(),
        google.as_ref(),
        received,
        Transport::Http,
    ) {
        refusal.budget_revision = Some(account.budget_revision);
        let _ = state.accounts.record_refusal(&account.id, refusal).await;
        let reason = google_reason
            .or(diagnostic)
            .or_else(|| Some(summary.into()));
        if owner.is_some() && last_status == StatusCode::TOO_MANY_REQUESTS {
            let current = state
                .accounts
                .get(&account.id)
                .await
                .unwrap_or_else(|| account.clone());
            return Attempt::Finish(owner_rate_limit(retry_seconds(&current, model)));
        }
        let native = if !matches!(
            last_status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
        ) && native
            && account.provider != SubscriptionProvider::Meta
        {
            body.and_then(|body| native_refusal(last_status, passed, body))
        } else {
            None
        };
        return Attempt::Retry(native, reason);
    }
    // A native refusal of the request itself (too long, invalid) is the
    // agent's to read and act on, compaction included; the account is fine.
    if account.provider == SubscriptionProvider::Meta {
        return Attempt::Finish(error(last_status, summary));
    }
    if let (true, Some(body)) = (native, body) {
        if let Some(refusal) = native_refusal(last_status, passed, body) {
            return Attempt::Finish(refusal);
        }
    } else {
        state.accounts.mark_error_for(account, summary).await;
    }
    Attempt::Finish(error(last_status, diagnostic.as_deref().unwrap_or(summary)))
}

/// Response headers an agent reads from its own provider: rate-limit state
/// (Claude's unified limits drive its fast-mode fallback and usage warnings,
/// Codex's drive its status line) and request ids. No cookie, no credential.
pub(crate) fn passthrough_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream {
        let name_text = name.as_str();
        if name_text.starts_with("anthropic-ratelimit-")
            || name_text.starts_with("x-codex-")
            || name_text.starts_with("x-ratelimit-")
            || matches!(name_text, "request-id" | "x-request-id" | "retry-after")
        {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// The provider's own JSON refusal with its status and limit headers, or
/// nothing when its body is not a JSON document.
pub(super) fn native_refusal(
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Option<Response> {
    serde_json::from_slice::<Value>(&body).ok()?;
    let mut refusal = (status, Body::from(body)).into_response();
    refusal.headers_mut().extend(headers);
    refusal.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Some(refusal)
}

fn retry_seconds(account: &AccountCredential, model: &str) -> u64 {
    account
        .retry_at(model)
        .map(|until| (until - OffsetDateTime::now_utc()).whole_seconds().max(0) as u64 + 1)
        .unwrap_or(30)
}

/// The upstream went quiet for longer than the request timeout.
pub(super) const STALLED: &str = "upstream stream stalled";
