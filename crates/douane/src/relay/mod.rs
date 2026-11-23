use crate::{
    ledger::{Call, Ledger},
    state::AppState,
    subscriptions::{AccountCredential, SubscriptionProvider, antigravity_user_agent},
    translate::{self, Bridge, Dialect, Frames},
};
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

/// 429 when shares hold the client back, so it waits; 400 for an
/// Antigravity model outside the Gemini API, which no retry changes; 503
/// otherwise.
pub(crate) fn refusal_status(message: &str) -> StatusCode {
    if message.starts_with(crate::clients::SHARE_USED_UP) {
        StatusCode::TOO_MANY_REQUESTS
    } else if message == crate::state::ANTIGRAVITY_GEMINI_ONLY {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({"error":{"type":"proxy_error","message":message}})),
    )
        .into_response()
}
/// GitHub Copilot's API. Enterprise accounts carry their own base URL.
mod attempt;
mod ledger;
mod request;
mod sse;
#[cfg(test)]
mod tests;

pub use attempt::forward;
use attempt::*;
pub(crate) use ledger::CallGuard;
use request::*;
pub(crate) use request::{
    CLAUDE_USER_AGENT, CODEX_USER_AGENT, GEMINI_USER_AGENT, choose, upstream, upstream_headers,
    uses_codex_catalog,
};
pub use request::{COPILOT_API, COPILOT_API_VERSION, COPILOT_USER_AGENT, endpoint};
pub(crate) use sse::read_bounded;
use sse::*;
