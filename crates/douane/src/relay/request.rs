use super::*;
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
/// Copilot identifies the integration by the OAuth app and these headers;
/// douane signs in through the OpenCode app and presents itself the same way.
pub const COPILOT_USER_AGENT: &str = "opencode/1.19.5";
pub const COPILOT_API_VERSION: &str = "2026-06-01";
pub(crate) const CODEX_USER_AGENT: &str = "codex_cli_rs/0.166.0";
pub(crate) const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.312 (external, cli)";
pub(crate) const GEMINI_USER_AGENT: &str = "GeminiCLI/0.66.0";

/// The wire format a custom endpoint was configured with.
pub(super) fn configured_dialect(value: &str) -> Option<Dialect> {
    match value {
        "chat" => Some(Dialect::Chat),
        "responses" => Some(Dialect::Responses),
        "messages" => Some(Dialect::Messages),
        _ => None,
    }
}

pub(super) fn prepare_mistral_chat(url: &str, body: &mut Value) {
    let Ok(url) = reqwest::Url::parse(url) else {
        return;
    };
    if url.host_str() != Some("api.mistral.ai") {
        return;
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    // Mistral rejects the empty reasoning_content sent in tool-call history.
    for message in messages {
        if message["role"] == "assistant"
            && message["reasoning_content"].as_str() == Some("")
            && let Some(message) = message.as_object_mut()
        {
            message.remove("reasoning_content");
        }
    }
}

/// A Gemini API call, `/v1beta/models/{model}:{method}`, as Antigravity's own
/// clients send it to a custom gateway: the model and whether a stream was
/// asked for.
pub(super) fn gemini_call(path: &str) -> Option<Result<(&str, bool), &'static str>> {
    let call = path.strip_prefix("/v1beta/models/")?;
    Some(match call.rsplit_once(':') {
        Some((model, "generateContent")) if !model.is_empty() => Ok((model, false)),
        Some((model, "streamGenerateContent")) if !model.is_empty() => Ok((model, true)),
        _ => Err("only generateContent and streamGenerateContent are offered"),
    })
}

/// Antigravity clients name a leveled model by its family and send the level
/// apart (`gemini-3.8-flash` or `gemini-3.1-pro-preview` with `thinkingLevel:
/// low`), where the catalog lists `gemini-3.8-flash-low` or
/// `gemini-3.1-pro-low`. The leveled variant wins over a listed family; an
/// unlisted id is kept as sent.
pub(super) async fn gemini_model(
    state: &AppState,
    client: &crate::clients::Client,
    owner: Option<&str>,
    model: &str,
    body: &Value,
) -> String {
    let models = state.antigravity_catalog_for(client, owner).await;
    let listed = |id: &str| models.iter().any(|m| m.upstream_id == id);
    let families: Vec<&str> = std::iter::once(model)
        .chain(model.strip_suffix("-preview"))
        .collect();
    let level = body
        .pointer("/generationConfig/thinkingConfig/thinkingLevel")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let leveled = level.iter().flat_map(|level| {
        families
            .iter()
            .map(move |family| format!("{family}-{level}"))
    });
    leveled
        .chain(families.iter().map(|family| family.to_string()))
        .find(|id| listed(id))
        .unwrap_or_else(|| model.to_owned())
}

/// A Gemini API client's signed history belongs to the account that signed
/// it: the one its session was last served by. A signed turn waits for that
/// account rather than move.
pub(super) fn gemini_owner(state: &AppState, headers: &HeaderMap, body: &Value) -> Option<String> {
    let signed = body["contents"].as_array().is_some_and(|turns| {
        turns.iter().any(|turn| {
            turn["parts"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|p| p.get("thoughtSignature").is_some()))
        })
    });
    if !signed {
        return None;
    }
    let session = crate::affinity::session_key(headers, body)?;
    state
        .affinity
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(SubscriptionProvider::Antigravity.as_str(), &session)
}

pub(super) fn copilot_path(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Messages => "/v1/messages",
        Dialect::Responses => "/responses",
        _ => "/chat/completions",
    }
}

/// Copilot serves each model on the paths its catalog lists. The client's
/// own format goes through untouched when the model accepts it; otherwise
/// Messages, then Responses, then Chat Completions.
pub(super) fn copilot_wire(client: Dialect, endpoints: &[String]) -> Dialect {
    if endpoints.is_empty() {
        return Dialect::Chat;
    }
    let accepts = |dialect: Dialect| endpoints.iter().any(|e| e == copilot_path(dialect));
    if client != Dialect::Gemini && accepts(client) {
        return client;
    }
    [Dialect::Messages, Dialect::Responses, Dialect::Chat]
        .into_iter()
        .find(|dialect| accepts(*dialect))
        .unwrap_or(Dialect::Chat)
}

/// Copilot bills a premium request for each turn its user starts; tool
/// results and other follow-ups are the agent's own and say so.
pub(crate) fn copilot_initiator(body: &Value, wire: Dialect) -> &'static str {
    let user_turn = match wire {
        Dialect::Messages => body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|last| {
                last["role"] == "user"
                    && (last["content"].is_string()
                        || last["content"].as_array().is_some_and(|parts| {
                            parts.iter().any(|part| part["type"] != "tool_result")
                        }))
            }),
        Dialect::Responses => {
            body["input"].is_string()
                || body["input"]
                    .as_array()
                    .and_then(|items| items.last())
                    .is_some_and(|last| last["role"] == "user")
        }
        _ => body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|last| last["role"] == "user"),
    };
    if user_turn { "user" } else { "agent" }
}

/// Copilot needs to be told when a request carries an image.
pub(crate) fn copilot_vision(body: &Value, wire: Dialect) -> bool {
    let image = |part: &Value| {
        matches!(
            part["type"].as_str(),
            Some("image" | "image_url" | "input_image")
        ) || (part["type"] == "tool_result"
            && part["content"]
                .as_array()
                .is_some_and(|inner| inner.iter().any(|p| p["type"] == "image")))
    };
    let items = match wire {
        Dialect::Responses => body["input"].as_array(),
        _ => body["messages"].as_array(),
    };
    items.is_some_and(|items| {
        items.iter().any(|item| {
            item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(image))
        })
    })
}

pub fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/v1") && path.starts_with("/v1/") {
        format!("{base}{}", &path[3..])
    } else {
        format!("{base}{path}")
    }
}

pub(crate) fn upstream(
    account: &AccountCredential,
    client: Dialect,
    model: &str,
    endpoints: &[String],
) -> (String, Dialect) {
    use SubscriptionProvider::*;
    let (base, wire, path) = match account.provider {
        Codex => (
            "https://chatgpt.com/backend-api/codex",
            Dialect::Responses,
            "/responses",
        ),
        Claude => (
            "https://api.anthropic.com",
            Dialect::Messages,
            "/v1/messages",
        ),
        Xai => (
            "https://cli-chat-proxy.grok.com/v1",
            Dialect::Chat,
            "/v1/chat/completions",
        ),
        OpencodeGo => {
            // Muse only serves Responses. Translate at the provider boundary
            // even when the caller uses Messages or Chat Completions.
            let wire = if model.starts_with("muse-") {
                Dialect::Responses
            } else {
                Dialect::Chat
            };
            (
                "https://opencode.ai/zen/go",
                wire,
                if wire == Dialect::Responses {
                    "/v1/responses"
                } else {
                    "/v1/chat/completions"
                },
            )
        }
        Kimi => (
            "https://api.kimi.com/coding",
            Dialect::Messages,
            "/v1/messages",
        ),
        Qwen => (
            "https://portal.qwen.ai",
            Dialect::Chat,
            "/v1/chat/completions",
        ),
        Meta => ("https://api.meta.ai", Dialect::Responses, "/v1/responses"),
        Copilot => {
            let wire = copilot_wire(client, endpoints);
            (COPILOT_API, wire, copilot_path(wire))
        }
        OpenaiCompatible => {
            let wire = account
                .dialect
                .as_deref()
                .and_then(configured_dialect)
                .unwrap_or(client);
            (
                "https://api.openai.com",
                wire,
                match wire {
                    Dialect::Responses => "/v1/responses",
                    Dialect::Messages => "/v1/messages",
                    _ => "/v1/chat/completions",
                },
            )
        }
        Gemini => (
            "https://cloudcode-pa.googleapis.com",
            Dialect::Gemini,
            "/v1internal:generateContent",
        ),
        Antigravity => (
            "https://daily-cloudcode-pa.googleapis.com",
            Dialect::Gemini,
            "/v1internal:generateContent",
        ),
    };
    (
        endpoint(account.base_url.as_deref().unwrap_or(base), path),
        wire,
    )
}

pub(crate) fn upstream_headers(account: &AccountCredential, incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming {
        if !matches!(
            name.as_str(),
            "host"
                | "authorization"
                | "x-api-key"
                | "x-goog-api-key"
                | "api-key"
                | "x-douane-admin"
                | "x-management-key"
                | "x-douane-provider"
                | "x-douane-model-catalog"
                | "proxy-authorization"
                | "connection"
                | "keep-alive"
                | "te"
                | "trailer"
                | "upgrade"
                | "content-length"
                | "transfer-encoding"
                | "accept-encoding"
                | "cookie"
                | "origin"
                | "referer"
        ) && !name.as_str().starts_with("sec-websocket-")
        {
            if name == "anthropic-beta" {
                out.append(name.clone(), value.clone());
            } else {
                out.insert(name.clone(), value.clone());
            }
        }
    }
    out.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    if let Ok(token) = format!("Bearer {}", account.access_token).parse() {
        out.insert(header::AUTHORIZATION, token);
    }
    match account.provider {
        SubscriptionProvider::Claude | SubscriptionProvider::Kimi => {
            out.entry("anthropic-version")
                .or_insert(header::HeaderValue::from_static("2023-06-01"));
            if account.provider == SubscriptionProvider::Claude {
                merge_anthropic_betas(&mut out);
                out.entry("user-agent")
                    .or_insert(header::HeaderValue::from_static(CLAUDE_USER_AGENT));
                out.entry("x-app")
                    .or_insert(header::HeaderValue::from_static("cli"));
            }
        }
        SubscriptionProvider::Codex => {
            if let Some(id) = &account.account_id {
                if let Ok(value) = id.parse() {
                    out.insert("chatgpt-account-id", value);
                }
            }
            out.entry("originator")
                .or_insert(header::HeaderValue::from_static("codex_cli_rs"));
            out.entry("user-agent")
                .or_insert(header::HeaderValue::from_static(CODEX_USER_AGENT));
        }
        SubscriptionProvider::Xai => {
            out.insert(
                "x-xai-token-auth",
                header::HeaderValue::from_static("xai-grok-cli"),
            );
            out.insert(
                "x-grok-client-version",
                header::HeaderValue::from_static("1.0.44"),
            );
            out.insert(
                "user-agent",
                header::HeaderValue::from_static(
                    "grok-pager/1.0.44 grok-shell/1.0.44 (linux; x86_64)",
                ),
            );
            if let Some(id) = &account.account_id {
                if let Ok(v) = id.parse() {
                    out.insert("x-userid", v);
                }
            }
        }
        SubscriptionProvider::OpencodeGo => {
            out.entry("user-agent")
                .or_insert(header::HeaderValue::from_static(concat!(
                    "douane/",
                    env!("CARGO_PKG_VERSION")
                )));
            if !out.contains_key("x-opencode-session") {
                if let Some(value) = ["session_id", "session-id", "thread-id"]
                    .iter()
                    .find_map(|h| incoming.get(*h))
                {
                    out.insert("x-opencode-session", value.clone());
                }
            }
        }
        SubscriptionProvider::Meta => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(crate::subscriptions::meta::USER_AGENT),
            );
            out.insert("x-client-id", header::HeaderValue::from_static("tbh:tui"));
            out.insert(
                header::ACCEPT,
                header::HeaderValue::from_static("text/event-stream"),
            );
            out.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            );
        }
        SubscriptionProvider::Copilot => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(COPILOT_USER_AGENT),
            );
            out.insert(
                "x-github-api-version",
                header::HeaderValue::from_static(COPILOT_API_VERSION),
            );
            out.insert(
                "openai-intent",
                header::HeaderValue::from_static("conversation-edits"),
            );
        }
        SubscriptionProvider::Gemini => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(GEMINI_USER_AGENT),
            );
        }
        SubscriptionProvider::Antigravity => {
            out.insert(
                header::USER_AGENT,
                header::HeaderValue::from_static(antigravity_user_agent()),
            );
            out.insert(
                "x-goog-api-client",
                header::HeaderValue::from_static("gl-node/22.21.1"),
            );
        }
        _ => {}
    }
    out
}

pub(crate) fn uses_codex_catalog(headers: &HeaderMap) -> bool {
    headers
        .get("x-douane-model-catalog")
        .is_some_and(|v| v == "codex")
}

fn merge_anthropic_betas(headers: &mut HeaderMap) {
    let mut betas: Vec<_> = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(str::trim))
        .filter(|beta| !beta.is_empty())
        .collect();
    if !betas.contains(&"oauth-2025-04-20") {
        betas.splice(..0, ["oauth-2025-04-20", "claude-code-20250219"]);
    }
    if let Ok(value) = betas.join(",").parse() {
        headers.insert("anthropic-beta", value);
    }
}

pub(super) async fn provider_hint<'a>(
    state: &AppState,
    body: &Value,
    headers: &'a HeaderMap,
    client: Option<&crate::clients::Client>,
) -> Option<&'a str> {
    let explicit = headers
        .get("x-douane-provider")
        .and_then(|v| v.to_str().ok());
    if explicit.is_some() {
        return explicit;
    }
    // A catalog hint applies only to bare IDs the requesting key can see.
    let client = client?;
    let model = body["model"].as_str().filter(|m| !m.contains('/'))?;
    match headers.get("x-douane-model-catalog")?.to_str().ok()? {
        "codex" if state.codex_catalog_contains(client, model).await => Some("codex"),
        "claude" if state.claude_catalog_contains(client, model).await => Some("claude"),
        _ => None,
    }
}

pub(crate) async fn choose(
    state: &AppState,
    body: &Value,
    headers: &HeaderMap,
    client: Option<&crate::clients::Client>,
    advance_rotation: bool,
) -> Result<(String, Vec<AccountCredential>, Option<String>), String> {
    let hint = provider_hint(state, body, headers, client).await;
    choose_with_owner(
        state,
        body,
        headers,
        client,
        hint,
        None,
        false,
        advance_rotation,
    )
    .await
}

/// `gemini` for a Gemini API request, the only one Antigravity accounts serve.
#[allow(clippy::too_many_arguments)]
pub(super) async fn choose_with_owner(
    state: &AppState,
    body: &Value,
    headers: &HeaderMap,
    client: Option<&crate::clients::Client>,
    provider_hint: Option<&str>,
    owner: Option<&str>,
    gemini: bool,
    advance_rotation: bool,
) -> Result<(String, Vec<AccountCredential>, Option<String>), String> {
    let model = body["model"]
        .as_str()
        .filter(|m| !m.is_empty())
        .ok_or("model is required")?;
    let session = crate::affinity::session_key(headers, body);
    let (upstream, accounts) = state
        .candidates_with_owner(
            model,
            provider_hint,
            session.as_deref(),
            client,
            owner,
            gemini,
            advance_rotation,
        )
        .await?;
    Ok((upstream, accounts, session))
}

pub(super) struct RelayRequest {
    pub client: Option<crate::clients::Client>,
    pub model: String,
    pub session: Option<String>,
    pub owner: Option<String>,
    pub from: Dialect,
    pub stream: bool,
    pub headers: HeaderMap,
    value: Value,
    count_tokens: bool,
    compact: bool,
}

pub(super) struct UpstreamRequest {
    pub url: String,
    pub to: Dialect,
    pub custom: std::collections::BTreeSet<String>,
    pub codex_sse: bool,
    pub native: bool,
    pub request_headers: HeaderMap,
    pub bytes: Bytes,
}

impl RelayRequest {
    pub async fn parse(
        state: &AppState,
        path: &str,
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<(Self, Vec<AccountCredential>), Response> {
        let client = match state.clients.authenticate(&headers) {
            Ok(client) => client,
            Err(refusal) => return Err(error(StatusCode::UNAUTHORIZED, refusal.message())),
        };
        let count_tokens = path.ends_with("/messages/count_tokens");
        let gemini = match gemini_call(path) {
            Some(Ok(call)) => Some(call),
            Some(Err(e)) => return Err(error(StatusCode::NOT_FOUND, e)),
            None => None,
        };
        let from = if gemini.is_some() {
            Dialect::Gemini
        } else if path.ends_with("/messages") || count_tokens {
            Dialect::Messages
        } else if path.ends_with("/chat/completions") {
            Dialect::Chat
        } else {
            Dialect::Responses
        };
        let mut value: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => return Err(error(StatusCode::BAD_REQUEST, "body must be valid JSON")),
        };
        if path.starts_with("/muse-code/") {
            // Native Muse uses bare model ids. Pin this endpoint to its own
            // subscription even when another provider serves the same model.
            headers.insert("x-douane-provider", "meta".parse().unwrap());
            if value["model"]
                .as_str()
                .is_some_and(|model| model.contains('/') && !model.starts_with("meta/"))
            {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "Muse Code requires a Meta model",
                ));
            }
        }
        let mut owner = None;
        if let Some((model, streamed)) = gemini {
            if !value.is_object() {
                return Err(error(StatusCode::BAD_REQUEST, "body must be a JSON object"));
            }
            // Only Antigravity accounts serve the Gemini API natively. The model
            // and stream choice travel in the path; both leave the body again
            // before it goes upstream.
            // A prefix names a provider; this route has only one.
            let model = model.strip_prefix("antigravity/").unwrap_or(model);
            if model.is_empty() || model.contains('/') {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    "the Gemini API serves Antigravity models only",
                ));
            }
            headers.insert("x-douane-provider", "antigravity".parse().unwrap());
            value["stream"] = json!(streamed);
            // The client's own session id keeps its turns, and the signatures
            // they carry, on one account.
            if let Some(session) = value["sessionId"].as_str().filter(|s| !s.is_empty()) {
                value["prompt_cache_key"] = json!(session);
            }
            owner = gemini_owner(state, &headers, &value);
            value["model"] =
                json!(gemini_model(state, &client, owner.as_deref(), model, &value).await);
        }
        let stream = value["stream"].as_bool().unwrap_or(false);
        let hint = provider_hint(state, &value, &headers, Some(&client)).await;
        let (model, candidates, session) = match choose_with_owner(
            state,
            &value,
            &headers,
            Some(&client),
            hint,
            owner.as_deref(),
            gemini.is_some(),
            true,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => return Err(error(refusal_status(&e), &e)),
        };
        value["model"] = json!(model);
        // The Grok CLI repeats its model in a header, which gets the provider's id
        // as the body does: xAI does not know the gateway's prefix.
        if headers.contains_key("x-grok-model-override") {
            if let Ok(named) = model.parse() {
                headers.insert("x-grok-model-override", named);
            }
        }

        Ok((
            Self {
                client: Some(client),
                model,
                session,
                owner,
                from,
                stream,
                headers,
                value,
                count_tokens,
                compact: path.ends_with("/responses/compact"),
            },
            candidates,
        ))
    }

    pub async fn prepare(
        &self,
        state: &AppState,
        account: &AccountCredential,
    ) -> Result<UpstreamRequest, Response> {
        let Self {
            model,
            session,
            headers,
            value,
            ..
        } = self;
        let (from, stream, count_tokens) = (self.from, self.stream, self.count_tokens);
        let endpoints: Vec<String> = if account.provider == SubscriptionProvider::Copilot {
            state
                .models
                .read()
                .await
                .iter()
                .find(|m| m.provider == account.provider && m.upstream_id == *model)
                .map(|m| m.endpoints.clone())
                .unwrap_or_default()
        } else {
            vec![]
        };
        let (mut url, to) = upstream(account, from, model, &endpoints);
        if self.compact {
            if to != Dialect::Responses {
                return Err(error(
                    StatusCode::BAD_REQUEST,
                    "this provider does not offer native compaction",
                ));
            }
            url.push_str("/compact");
        }
        if count_tokens {
            if account.provider != SubscriptionProvider::Claude {
                return Err(error(
                    StatusCode::NOT_FOUND,
                    "token counting is offered only by Claude accounts",
                ));
            }
            url.push_str("/count_tokens");
        }
        let custom = translate::custom_tools(value, from);
        let mut translated = match translate::request(value, from, to) {
            Ok(v) => v,
            Err(e) => return Err(error(StatusCode::BAD_REQUEST, &e)),
        };
        if from == Dialect::Gemini {
            if let Some(request) = translated.as_object_mut() {
                request.remove("model");
                request.remove("stream");
                request.remove("prompt_cache_key");
            }
        }
        // Chat Completions has no `output_config`: a Messages client's effort
        // reaches a custom endpoint only as the `reasoning_effort` its model
        // lists.
        if account.provider == SubscriptionProvider::OpenaiCompatible
            && from == Dialect::Messages
            && to == Dialect::Chat
        {
            let thinking = state
                .models
                .read()
                .await
                .iter()
                .find(|m| {
                    m.provider == account.provider
                        && m.upstream_id == *model
                        && m.route == account.route
                })
                .and_then(|m| m.thinking.clone());
            crate::openai_reasoning::configure(&mut translated, value, thinking.as_ref());
        }
        // Mistral refuses a Chat request past 20 images with a 503 no retry
        // can fix: the oldest images give way to a placeholder instead.
        if account.provider == SubscriptionProvider::OpenaiCompatible && to == Dialect::Chat {
            prepare_mistral_chat(&url, &mut translated);
            let limit = account.image_limit();
            let omitted = crate::chat_images::cap(&mut translated, limit);
            if omitted > 0 {
                tracing::info!(
                    provider = account.pool(),
                    model = %model,
                    omitted,
                    limit,
                    "older images replaced by a placeholder to fit the endpoint's image limit"
                );
            }
        }
        if account.provider == SubscriptionProvider::OpenaiCompatible {
            // The client's value takes precedence even if translation dropped it.
            let client_metadata = &value["metadata"];
            let metadata = account
                .metadata
                .iter()
                .map(|(key, default)| {
                    let entry = client_metadata[key.as_str()].as_str().unwrap_or(default);
                    (key.clone(), entry.to_owned())
                })
                .collect();
            crate::endpoint_metadata::merge(&mut translated, &metadata);
        }
        if account.provider == SubscriptionProvider::Meta {
            crate::subscriptions::meta::prepare_request(&mut translated);
        }
        let codex_sse = account.provider == SubscriptionProvider::Codex && !self.compact;
        // Same dialect: the agent talks to its own provider and reads its
        // answers, limits and refusals exactly as a direct subscription would.
        let native = from == to;
        if codex_sse {
            translated["stream"] = json!(true);
            translated["store"] = json!(false);
            if translated.get("instructions").is_none() {
                translated["instructions"] = json!("");
            }
            // Codex's backend refuses a plain text input: "Input must be a list".
            if let Some(text) = translated["input"].as_str() {
                translated["input"] =
                    json!([{"role":"user","content":[{"type":"input_text","text":text}]}]);
            }
            for key in ["max_output_tokens", "max_tokens", "temperature", "top_p"] {
                translated.as_object_mut().map(|m| m.remove(key));
            }
            if translated.get("prompt_cache_key").is_none() {
                if let Some(key) = session {
                    translated["prompt_cache_key"] = json!(key);
                }
            }
        }
        if to == Dialect::Gemini {
            if stream {
                url = url.replace(":generateContent", ":streamGenerateContent?alt=sse");
            }
            translated = json!({"model":model,"project":account.project_id,"request":translated});
            if account.provider == SubscriptionProvider::Antigravity {
                translated["userAgent"] = json!("antigravity");
                translated["requestType"] = json!("agent");
                translated["requestId"] = json!(format!("douane-{:016x}", rand::random::<u64>()));
            }
        }
        // Headers that depend on the request itself, not on the account.
        let mut request_headers = HeaderMap::new();
        if account.provider == SubscriptionProvider::Copilot {
            request_headers.insert(
                "x-initiator",
                header::HeaderValue::from_static(copilot_initiator(&translated, to)),
            );
            if copilot_vision(&translated, to) {
                request_headers.insert(
                    "copilot-vision-request",
                    header::HeaderValue::from_static("true"),
                );
            }
            if let Some(id) = session.as_deref().and_then(|s| s.parse().ok()) {
                request_headers.insert("x-interaction-id", id);
            }
            if to == Dialect::Messages {
                request_headers.insert(
                    "anthropic-version",
                    headers
                        .get("anthropic-version")
                        .cloned()
                        .unwrap_or(header::HeaderValue::from_static("2023-06-01")),
                );
            }
        }
        let bytes = match serde_json::to_vec(&translated) {
            Ok(v) => Bytes::from(v),
            Err(_) => return Err(error(StatusCode::BAD_REQUEST, "cannot serialize request")),
        };

        Ok(UpstreamRequest {
            url,
            to,
            custom,
            codex_sse,
            native,
            request_headers,
            bytes,
        })
    }
}
