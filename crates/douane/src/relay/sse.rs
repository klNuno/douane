use super::*;
use crate::subscriptions::cooldown::{Transport, classify_event};
use time::OffsetDateTime;
/// Unwrap Cloud Code’s native response envelope.
pub(super) fn unwrap_gemini(mut answer: Value) -> Value {
    match answer.get_mut("response") {
        Some(inner) if inner.is_object() => inner.take(),
        _ => answer,
    }
}

pub(super) fn gemini_finished(event: &Value) -> bool {
    let body = event.get("response").unwrap_or(event);
    body.pointer("/candidates/0/finishReason")
        .and_then(Value::as_str)
        .is_some_and(|reason| !reason.is_empty() && reason != "FINISH_REASON_UNSPECIFIED")
}

/// A silence longer than `idle` ends the upstream body as stalled.
pub(super) async fn next_chunk<S>(
    body: &mut S,
    idle: Duration,
    interrupted: &str,
) -> Option<Result<Bytes, String>>
where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin,
{
    match tokio::time::timeout(idle, body.next()).await {
        Ok(Some(Ok(chunk))) => Some(Ok(chunk)),
        Ok(Some(Err(_))) => Some(Err(interrupted.into())),
        Ok(None) => None,
        Err(_) => Some(Err(STALLED.into())),
    }
}

pub(crate) async fn read_bounded(
    response: reqwest::Response,
    cap: usize,
    idle: Duration,
) -> Result<Vec<u8>, String> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = next_chunk(&mut stream, idle, "upstream body interrupted").await {
        let chunk = chunk?;
        if bytes.len() + chunk.len() > cap {
            return Err("upstream response exceeds size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Subscription telemetry can follow a completed Muse answer. Finish the client
/// promptly, then read only the existing response for its private quota event.
pub(super) fn meta_tail<S>(
    mut upstream: S,
    mut frames: Frames,
    state: Arc<AppState>,
    account: String,
) where
    S: futures_util::Stream<Item = reqwest::Result<Bytes>> + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_secs(5), async move {
            loop {
                let chunk = upstream.next().await;
                let received = OffsetDateTime::now_utc();
                let eof = chunk.is_none();
                let events = match chunk {
                    Some(Ok(chunk)) => frames.feed(&chunk),
                    Some(Err(_)) => return,
                    None => frames.finish(),
                };
                let Ok(events) = events else {
                    return;
                };
                for event in events {
                    if crate::subscriptions::meta::subscription_event(&event) {
                        state
                            .quotas
                            .observe_meta_event_at(&account, &event, received)
                            .await;
                        state.observe_usage(&account).await;
                        return;
                    }
                }
                if eof {
                    return;
                }
                // A finished decoder has no further data; the timeout bounds
                // a provider keeping its transport open without telemetry.
            }
        })
        .await;
    });
}

pub(super) async fn collect_response(
    response: reqwest::Response,
    to: Dialect,
    call: &mut CallGuard,
    idle: Duration,
    state: &Arc<AppState>,
    account: &AccountCredential,
) -> Result<Value, String> {
    let mut decoder = Frames::default();
    let mut upstream = response.bytes_stream();
    let mut collected = Collected::default();
    loop {
        let next = next_chunk(&mut upstream, idle, "upstream stream interrupted").await;
        let received = OffsetDateTime::now_utc();
        let eof = next.is_none();
        let events = match next {
            Some(chunk) => decoder.feed(&chunk?)?,
            None => decoder.finish()?,
        };
        for mut frame in events {
            if observe_frame(state, account, call, &mut frame, received).await {
                if let Some(error) = &call.call.error {
                    return Err(error.clone());
                }
                collected.frame(&frame, to)?;
            }
        }
        if eof {
            break;
        }
        if account.provider == SubscriptionProvider::Meta && collected.result.is_some() {
            meta_tail(upstream, decoder, state.clone(), account.id.clone());
            break;
        }
    }
    let mut result = collected
        .result
        .ok_or("upstream stream closed before completing its response")?;
    // Codex sends its items as `response.output_item.done` and completes with
    // an empty `output`; the finished items are the answer.
    let empty = result["output"].as_array().is_none_or(Vec::is_empty);
    if empty && !collected.items.is_empty() {
        collected.items.sort_by_key(|(index, _)| *index);
        result["output"] =
            Value::Array(collected.items.into_iter().map(|(_, item)| item).collect());
    }
    Ok(result)
}

#[derive(Default)]
pub(super) struct Collected {
    result: Option<Value>,
    items: Vec<(u64, Value)>,
    gemini: Option<Value>,
    gemini_bytes: usize,
}

impl Collected {
    fn frame(&mut self, frame: &Value, to: Dialect) -> Result<(), String> {
        if frame["type"] == "response.failed" || frame["type"] == "error" {
            return Err("upstream reported a stream error".into());
        }
        if to == Dialect::Gemini {
            let body = frame.get("response").unwrap_or(frame);
            let refused = translate::gemini_refusal(body).is_some();
            if body.get("error").is_some() {
                return Err("upstream reported a stream error".into());
            }
            let result = self.gemini.get_or_insert_with(
                || json!({"candidates":[{"content":{"role":"model","parts":[]}}]}),
            );
            if let Some(parts) = body
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
            {
                self.gemini_bytes += serde_json::to_vec(parts)
                    .map_err(|_| "invalid native Gemini parts")?
                    .len();
                if self.gemini_bytes > 32 * 1024 * 1024 {
                    return Err("upstream response exceeds size limit".into());
                }
                result["candidates"][0]["content"]["parts"]
                    .as_array_mut()
                    .unwrap()
                    .extend(parts.iter().cloned());
            }
            if let Some(usage) = body.get("usageMetadata") {
                result["usageMetadata"] = usage.clone();
            }
            if let Some(feedback) = body.get("promptFeedback") {
                result["promptFeedback"] = feedback.clone();
            }
            if gemini_finished(frame) {
                result["candidates"][0]["finishReason"] =
                    body["candidates"][0]["finishReason"].clone();
            }
            if gemini_finished(result) || refused {
                self.result = Some(result.clone());
            }
            return Ok(());
        }
        if to != Dialect::Responses {
            return Ok(());
        }
        match frame["type"].as_str() {
            Some("response.output_item.done") if frame["item"].is_object() => {
                let index = frame["output_index"]
                    .as_u64()
                    .unwrap_or(self.items.len() as u64);
                self.items.push((index, frame["item"].clone()));
            }
            Some("response.completed" | "response.incomplete") => {
                self.result = Some(frame["response"].clone());
            }
            _ => {}
        }
        Ok(())
    }
}

/// Apply one decoded frame for both the streaming and collected paths.
pub(super) async fn observe_frame(
    state: &AppState,
    account: &AccountCredential,
    call: &mut CallGuard,
    frame: &mut Value,
    received: OffsetDateTime,
) -> bool {
    if account.provider == SubscriptionProvider::Meta {
        if crate::subscriptions::meta::subscription_event(frame) {
            state
                .quotas
                .observe_meta_event_at(&account.id, frame, received)
                .await;
            state.observe_usage(&account.id).await;
            return false;
        }
        crate::subscriptions::meta::sanitize_error_event(frame);
    }
    let body: &Value = if matches!(
        account.provider,
        SubscriptionProvider::Gemini | SubscriptionProvider::Antigravity
    ) {
        frame.get("response").unwrap_or(frame)
    } else {
        &*frame
    };
    if let Some(failure) = classify_event(
        account.provider,
        body,
        &call.call.model,
        received,
        Transport::Http,
    ) {
        let mut refusal = failure.refusal;
        refusal.budget_revision = Some(account.budget_revision);
        call.call.status = failure.status;
        call.fail(&refusal.reason);
        let _ = state.accounts.record_refusal(&account.id, refusal).await;
    } else {
        call.observe(frame);
    }
    true
}

type UpstreamBody =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>;

enum StreamFormat {
    Native,
    Meta,
    Gemini,
    Translated(Box<Bridge>),
}

struct StreamRelay {
    upstream: Option<UpstreamBody>,
    frames: Frames,
    format: StreamFormat,
    call: CallGuard,
    state: Arc<AppState>,
    account: AccountCredential,
    idle: Duration,
}

impl StreamRelay {
    async fn next_output(&mut self) -> Result<Option<Bytes>, String> {
        while let Some(upstream) = self.upstream.as_mut() {
            let next = next_chunk(upstream, self.idle, "upstream stream interrupted").await;
            let received = OffsetDateTime::now_utc();
            let (events, raw) = match next {
                Some(chunk) => {
                    let chunk = chunk?;
                    (self.frames.feed(&chunk)?, Some(chunk))
                }
                None => {
                    self.upstream.take();
                    (self.frames.finish()?, None)
                }
            };
            let mut output = self.encode(events, received).await?;
            match raw {
                Some(chunk) if matches!(self.format, StreamFormat::Native) => {
                    return Ok(Some(chunk));
                }
                Some(_) => self.detach_meta_tail(),
                None => self.finish(&mut output).await?,
            }
            if !output.is_empty() {
                return Ok(Some(output.into()));
            }
        }
        Ok(None)
    }

    async fn encode(
        &mut self,
        events: Vec<Value>,
        received: OffsetDateTime,
    ) -> Result<String, String> {
        let mut output = String::new();
        for mut event in events {
            if !observe_frame(
                &self.state,
                &self.account,
                &mut self.call,
                &mut event,
                received,
            )
            .await
            {
                continue;
            }
            match &mut self.format {
                StreamFormat::Native => {}
                StreamFormat::Meta => {
                    let kind = event["type"].as_str().unwrap_or("message");
                    output.push_str(&format!("event: {kind}\ndata: {event}\n\n"));
                }
                StreamFormat::Gemini => {
                    if let Some(message) = translate::gemini_refusal(&event) {
                        self.call.fail(message);
                    } else if gemini_finished(&event) {
                        self.call.complete = true;
                    }
                    let answer = unwrap_gemini(event);
                    output.push_str(&format!("data: {answer}\r\n\r\n"));
                }
                StreamFormat::Translated(bridge) => {
                    if let Some(error) = &self.call.call.error {
                        return Err(error.clone());
                    }
                    output.push_str(&bridge.feed(event)?);
                }
            }
        }
        Ok(output)
    }

    async fn finish(&mut self, output: &mut String) -> Result<(), String> {
        if self.call.call.error.is_some() {
            return Ok(());
        }
        if let StreamFormat::Translated(bridge) = &mut self.format {
            output.push_str(&bridge.finish()?);
            self.call.complete = true;
        }
        if !self.call.complete {
            return Err("upstream stream closed before completion".into());
        }
        self.state.accounts.clear_error_for(&self.account).await;
        Ok(())
    }

    fn detach_meta_tail(&mut self) {
        if self.account.provider != SubscriptionProvider::Meta
            || !self.call.complete
            || self.call.call.error.is_some()
        {
            return;
        }
        if let Some(upstream) = self.upstream.take() {
            meta_tail(
                upstream,
                std::mem::take(&mut self.frames),
                self.state.clone(),
                self.account.id.clone(),
            );
        }
    }
}

pub(super) fn stream_response(
    response: reqwest::Response,
    from: Dialect,
    to: Dialect,
    call: CallGuard,
    idle: Duration,
    custom: std::collections::BTreeSet<String>,
    state: Arc<AppState>,
    account: AccountCredential,
) -> Response {
    let format = match (from == to, to, account.provider) {
        (false, _, _) => StreamFormat::Translated(Box::new(
            Bridge::new(to, from, &call.call.model).with_custom_tools(custom),
        )),
        (true, Dialect::Gemini, _) => StreamFormat::Gemini,
        (true, _, SubscriptionProvider::Meta) => StreamFormat::Meta,
        _ => StreamFormat::Native,
    };
    let relay = StreamRelay {
        upstream: Some(Box::pin(response.bytes_stream())),
        frames: Frames::default(),
        format,
        call,
        state,
        account,
        idle,
    };
    let stream = futures_util::stream::unfold(relay, |mut relay| async move {
        let output = match relay.next_output().await {
            Ok(output) => output?,
            Err(error) => {
                // The bridge and frame readers return fixed diagnostic text.
                relay.call.fail(&error);
                relay.upstream.take();
                let body =
                    json!({"type":"error","error":{"type":"upstream_error","message":error}});
                Bytes::from(format!("event: error\ndata: {body}\n\n"))
            }
        };
        Some((Ok::<_, std::io::Error>(output), relay))
    });
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}
