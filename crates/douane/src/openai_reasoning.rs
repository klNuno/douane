//! Reasoning effort for OpenAI-compatible Chat endpoints.
//!
//! A custom endpoint's `/models` row can say a model reasons
//! (`capabilities.reasoning`, as Mistral does) without saying which
//! `reasoning_effort` values it takes. The upstream knows: a value it lacks is
//! refused with the list (`supported values: ['none', 'low', 'high']`). The
//! catalog asks once per account and model with a request that cannot run
//! inference, and the levels it reads let a Messages client's effort reach the
//! upstream as `reasoning_effort`.

use serde_json::Value;
use std::time::Duration;

/// Every effort name seen on an upstream, lowest first.
const ORDER: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// Values tried in turn: a model refusing one names what it takes. Two
/// rarely offered ones, so most models answer the first.
const PROBES: [&str; 2] = ["minimal", "medium"];

/// How long one probe may take before the catalog goes on without it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Probes in flight at once for one account.
pub(crate) const PROBE_CONCURRENCY: usize = 4;

/// Whether a `/models` row says the model reasons but not how.
pub(crate) fn needs_probe(row: &Value) -> bool {
    row.pointer("/capabilities/reasoning")
        .and_then(Value::as_bool)
        == Some(true)
        && row.get("thinking").is_none_or(Value::is_null)
}

/// The levels a refusal names, in effort order. `None` when the message
/// carries no `supported values` list.
pub(crate) fn parse_supported(message: &str) -> Option<Vec<String>> {
    let lower = message.to_ascii_lowercase();
    let start = lower.find("supported values")?;
    let rest = &message[start..];
    let open = rest.find('[')?;
    let close = rest[open..].find(']')? + open;
    let inside = &rest[open + 1..close];
    // Both list shapes seen on Mistral quote each value once:
    // `['none', 'low']` and `[<ReasoningEffort.high: 'high'>]`.
    let mut found: Vec<String> = inside
        .split(['\'', '"'])
        .skip(1)
        .step_by(2)
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .map(str::to_owned)
        .collect();
    found.dedup();
    if found.is_empty() {
        return None;
    }
    let rank = |level: &str| ORDER.iter().position(|known| *known == level);
    let mut known: Vec<String> = found
        .iter()
        .filter(|l| rank(l).is_some())
        .cloned()
        .collect();
    known.sort_by_key(|l| rank(l));
    known.dedup();
    known.extend(found.into_iter().filter(|l| rank(l).is_none()));
    Some(known)
}

/// What one probe learned.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// The upstream named the levels.
    Levels(Vec<String>),
    /// The upstream answered without a list: the model gets no levels.
    Silent,
    /// No usable answer (network, timeout, 5xx, 429): ask again next refresh.
    Retry,
}

/// Asks the endpoint which efforts `model` takes. The body has no messages,
/// so an upstream accepting the effort still refuses the request before any
/// inference, and `max_tokens: 1` bounds one that would not.
pub(crate) async fn probe(
    client: &reqwest::Client,
    url: &str,
    headers: reqwest::header::HeaderMap,
    model: &str,
) -> Probe {
    for effort in PROBES {
        let body = serde_json::json!({
            "model": model,
            "max_tokens": 1,
            "messages": [],
            "reasoning_effort": effort,
        });
        let response = match client
            .post(url)
            .headers(headers.clone())
            .json(&body)
            .timeout(PROBE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(model, error = %e, "reasoning level probe failed");
                return Probe::Retry;
            }
        };
        let status = response.status();
        if status.is_server_error() || status.as_u16() == 429 {
            tracing::warn!(model, %status, "reasoning level probe failed");
            return Probe::Retry;
        }
        let text = response.text().await.unwrap_or_default();
        if let Some(levels) = parse_supported(&error_message(&text)) {
            return Probe::Levels(levels);
        }
    }
    Probe::Silent
}

/// The upstream's error message, wherever its envelope keeps it.
fn error_message(text: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return text.to_owned();
    };
    for pointer in ["/message", "/error/message", "/error", "/detail"] {
        if let Some(message) = v.pointer(pointer).and_then(Value::as_str) {
            return message.to_owned();
        }
    }
    text.to_owned()
}

/// The effort a Messages request asks for. `Some("none")` for thinking
/// switched off; `None` when it asks for nothing.
fn requested(original: &Value) -> Option<&str> {
    if let Some(effort) = original
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
    {
        return Some(effort);
    }
    (original.pointer("/thinking/type").and_then(Value::as_str) == Some("disabled"))
        .then_some("none")
}

/// The level sent for `wanted`: itself when listed, otherwise the nearest one
/// above, otherwise the highest. Thinking switched off maps only to `none`.
fn choose(levels: &[String], wanted: &str) -> Option<String> {
    if levels.iter().any(|l| l == wanted) {
        return Some(wanted.to_owned());
    }
    if wanted == "none" {
        return None;
    }
    let rank = |level: &str| ORDER.iter().position(|known| *known == level);
    let target = rank(wanted)?;
    let ranked = || {
        levels
            .iter()
            .filter_map(|l| rank(l).map(|r| (r, l)))
            .filter(|(r, _)| *r > 0)
    };
    ranked()
        .filter(|(r, _)| *r >= target)
        .min_by_key(|(r, _)| *r)
        .or_else(|| ranked().max_by_key(|(r, _)| *r))
        .map(|(_, l)| l.clone())
}

/// Sets `reasoning_effort` on a Chat request translated from Messages, from
/// the effort the client asked for and the levels the model lists. A model
/// without levels, or a request asking for nothing, is left as it was.
pub(crate) fn configure(translated: &mut Value, original: &Value, thinking: Option<&Value>) {
    let Some(levels) = thinking
        .and_then(|t| t.get("levels"))
        .and_then(Value::as_array)
    else {
        return;
    };
    let levels: Vec<String> = levels
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect();
    let Some(wanted) = requested(original) else {
        return;
    };
    if let Some(level) = choose(&levels, wanted) {
        translated["reasoning_effort"] = Value::String(level);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn levels(list: &[&str]) -> Vec<String> {
        list.iter().map(|l| (*l).to_owned()).collect()
    }

    #[test]
    fn both_mistral_refusal_shapes_give_their_levels_in_order() {
        assert_eq!(
            parse_supported(
                "reasoning_effort 'minimal' is not supported for this model; supported values: ['none', 'low', 'high', 'xhigh', 'max']"
            ),
            Some(levels(&["none", "low", "high", "xhigh", "max"]))
        );
        assert_eq!(
            parse_supported(
                "reasoning_effort minimal is not supported for this model, supported values: [<ReasoningEffort.high: 'high'>, <ReasoningEffort.low: 'low'>, <ReasoningEffort.medium: 'medium'>]"
            ),
            Some(levels(&["low", "medium", "high"]))
        );
        assert_eq!(
            parse_supported("Conversation must have at least one message"),
            None
        );
        assert_eq!(parse_supported("supported values: []"), None);
    }

    #[test]
    fn the_error_message_is_read_from_its_envelope() {
        let body = json!({"object":"error","message":"supported values: ['low']","type":"invalid_request_invalid_args"});
        assert_eq!(
            error_message(&body.to_string()),
            "supported values: ['low']"
        );
        let openai = json!({"error":{"message":"supported values: ['high']"}});
        assert_eq!(
            error_message(&openai.to_string()),
            "supported values: ['high']"
        );
    }

    #[test]
    fn a_missing_effort_takes_the_nearest_level_above() {
        let deepseek = levels(&["none", "low", "high", "xhigh", "max"]);
        assert_eq!(choose(&deepseek, "medium").as_deref(), Some("high"));
        assert_eq!(choose(&deepseek, "max").as_deref(), Some("max"));
        assert_eq!(choose(&deepseek, "minimal").as_deref(), Some("low"));
        assert_eq!(choose(&deepseek, "none").as_deref(), Some("none"));
        let large = levels(&["low", "medium", "high"]);
        assert_eq!(choose(&large, "xhigh").as_deref(), Some("high"));
        assert_eq!(choose(&large, "none"), None);
        assert_eq!(choose(&large, "turbo"), None);
        // `none` is never the answer to an effort that asks for some.
        assert_eq!(choose(&levels(&["none"]), "low"), None);
    }

    #[test]
    fn only_a_requested_effort_on_a_model_with_levels_is_sent() {
        let thinking = json!({"levels":["none","low","high","xhigh","max"]});
        let mut out = json!({"model":"m"});
        configure(
            &mut out,
            &json!({"output_config":{"effort":"medium"},"thinking":{"type":"adaptive"}}),
            Some(&thinking),
        );
        assert_eq!(out["reasoning_effort"], "high");

        let mut out = json!({"model":"m"});
        configure(
            &mut out,
            &json!({"thinking":{"type":"disabled"}}),
            Some(&thinking),
        );
        assert_eq!(out["reasoning_effort"], "none");

        let mut out = json!({"model":"m"});
        configure(
            &mut out,
            &json!({"thinking":{"type":"adaptive"}}),
            Some(&thinking),
        );
        assert_eq!(out, json!({"model":"m"}));

        let mut out = json!({"model":"m"});
        configure(&mut out, &json!({"output_config":{"effort":"max"}}), None);
        assert_eq!(out, json!({"model":"m"}));
    }

    #[test]
    fn only_reasoning_rows_without_levels_are_probed() {
        assert!(needs_probe(
            &json!({"id":"a","capabilities":{"reasoning":true}})
        ));
        assert!(!needs_probe(
            &json!({"id":"a","capabilities":{"reasoning":false}})
        ));
        assert!(!needs_probe(&json!({"id":"a"})));
        assert!(!needs_probe(
            &json!({"id":"a","capabilities":{"reasoning":true},"thinking":{"levels":["low"]}})
        ));
    }
}
