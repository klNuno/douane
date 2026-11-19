//! Fixed `metadata` entries a custom endpoint account adds to each request.
//!
//! Mistral bills a request on a key to the organization's Vibe Code allowance
//! when its body carries a non-empty `metadata.call_type`, as Mistral Vibe
//! sends it, and to the API budget otherwise (probed 2026-11-19: the same key
//! answered 402 "API budget exhausted" without it and 200 with it). An
//! account's metadata lets one key of a route spend the Vibe allowance while
//! another keeps the API budget.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// Most entries an account's metadata holds.
pub const MAX_ENTRIES: usize = 16;

/// Keys of 1 to 64 ASCII letters, digits, `_`, `-` or `.`; values of 1 to 512
/// characters without control characters, since Mistral refuses an empty one.
pub fn valid(metadata: &BTreeMap<String, String>) -> bool {
    metadata.len() <= MAX_ENTRIES
        && metadata.iter().all(|(key, value)| {
            (1..=64).contains(&key.len())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                && (1..=512).contains(&value.chars().count())
                && !value.chars().any(char::is_control)
        })
}

pub const RULE: &str = "metadata takes at most 16 entries: keys of 1 to 64 letters, digits, _, - or ., values of 1 to 512 characters without control characters";

/// Adds each entry the request's `metadata` object lacks, in place: a key the
/// client set keeps the client's value. A request whose `metadata` is absent
/// or null gets a new object; any other `metadata` value is left alone.
/// Returns how many entries were added.
pub fn merge(request: &mut Value, metadata: &BTreeMap<String, String>) -> usize {
    if metadata.is_empty() {
        return 0;
    }
    let Some(body) = request.as_object_mut() else {
        return 0;
    };
    let target = body
        .entry("metadata")
        .or_insert_with(|| Value::Object(Map::new()));
    if target.is_null() {
        *target = Value::Object(Map::new());
    }
    let Some(target) = target.as_object_mut() else {
        return 0;
    };
    let mut added = 0;
    for (key, value) in metadata {
        if !target.contains_key(key) {
            target.insert(key.clone(), Value::String(value.clone()));
            added += 1;
        }
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn fills_missing_keys_and_keeps_the_clients_own() {
        let account = entries(&[("call_type", "main_call"), ("user_id", "douane")]);
        let mut request = json!({"model":"m","metadata":{"user_id":"client"}});
        assert_eq!(merge(&mut request, &account), 1);
        assert_eq!(
            request["metadata"],
            json!({"user_id":"client","call_type":"main_call"})
        );
        let mut bare = json!({"model":"m","metadata":null});
        assert_eq!(merge(&mut bare, &account), 2);
        assert_eq!(bare["metadata"]["call_type"], "main_call");
        let mut odd = json!({"model":"m","metadata":"text"});
        assert_eq!(merge(&mut odd, &account), 0);
        assert_eq!(odd["metadata"], "text");
        let mut untouched = json!({"model":"m"});
        assert_eq!(merge(&mut untouched, &BTreeMap::new()), 0);
        assert!(untouched.get("metadata").is_none());
    }

    #[test]
    fn rejects_empty_values_odd_keys_and_too_many_entries() {
        assert!(valid(&entries(&[("call_type", "main_call")])));
        assert!(!valid(&entries(&[("call_type", "")])));
        assert!(!valid(&entries(&[("call type", "x")])));
        assert!(!valid(&entries(&[("k", "a\nb")])));
        let many: BTreeMap<String, String> =
            (0..17).map(|i| (format!("k{i}"), "v".into())).collect();
        assert!(!valid(&many));
    }
}
