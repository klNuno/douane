//! Chat Completions endpoints that refuse a request past a number of images.
//!
//! Mistral answers HTTP 503 to a Chat request carrying a 21st image, at any
//! image size (2026-11-07). Claude Code sends every image of the conversation
//! again on each turn, so a session that reads images one per turn reaches
//! that count and every later request fails the same way. The oldest images
//! give way to a short text part; the newest stay, since they are the ones the
//! current turn is most likely about.

use serde_json::{Value, json};

/// Images an `openai_compatible` Chat endpoint receives per request when its
/// account sets no `image_limit`.
pub const DEFAULT_LIMIT: u32 = 20;

/// What stands in for an image the endpoint cannot take.
pub const OMITTED: &str = "[image omitted: upstream image limit]";

/// Keeps the newest `limit` image parts of a Chat Completions request and
/// replaces each older one with an [`OMITTED`] text part, in place. Returns
/// how many were replaced; a request within the limit is left untouched.
pub fn cap(request: &mut Value, limit: u32) -> usize {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return 0;
    };
    let excess = images(messages).count().saturating_sub(limit as usize);
    // Message order is conversation order: the first images are the oldest.
    for part in images(messages).take(excess) {
        *part = json!({"type": "text", "text": OMITTED});
    }
    excess
}

/// Every image part of a Chat message list, oldest first.
fn images(messages: &mut [Value]) -> impl Iterator<Item = &mut Value> {
    messages
        .iter_mut()
        .filter_map(|message| message.get_mut("content").and_then(Value::as_array_mut))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image_url"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(n: usize) -> Value {
        json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{n}")}})
    }

    /// Claude Code reading 21 PNGs one per turn, translated to Chat: each tool
    /// result's image follows its tool message as a user message.
    fn session(images: usize) -> Value {
        let mut messages = vec![json!({"role":"system","content":"sys"})];
        for n in 0..images {
            messages.push(json!({"role":"assistant","content":null,"tool_calls":[
                {"id":format!("t{n}"),"type":"function","function":{"name":"Read","arguments":"{}"}}]}));
            messages.push(json!({"role":"tool","tool_call_id":format!("t{n}"),
                "content":"The tool returned an image, attached in the next message."}));
            messages.push(json!({"role":"user","content":[
                {"type":"text","text":format!("Image returned by tool call t{n}:")}, image(n)]}));
        }
        json!({"model":"deepseek-v4-1-flash-1017","messages":messages})
    }

    fn images(request: &Value) -> Vec<String> {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["content"].as_array())
            .flatten()
            .filter(|p| p["type"] == "image_url")
            .map(|p| p["image_url"]["url"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn twenty_one_images_keep_the_newest_twenty_and_a_placeholder_for_the_first() {
        let mut request = session(21);
        assert_eq!(cap(&mut request, 20), 1);
        let kept = images(&request);
        assert_eq!(kept.len(), 20);
        assert_eq!(kept[0], "data:image/png;base64,1");
        assert_eq!(kept[19], "data:image/png;base64,20");
        // The placeholder sits exactly where the oldest image was.
        assert_eq!(
            request["messages"][3]["content"],
            json!([{"type":"text","text":"Image returned by tool call t0:"},
                   {"type":"text","text":OMITTED}])
        );
        assert_eq!(request["messages"][6]["content"][1], image(1));
    }

    #[test]
    fn images_inside_one_message_give_way_oldest_first() {
        let mut request = json!({"messages":[{"role":"user","content":[
            image(0), {"type":"text","text":"compare"}, image(1), image(2)]}]});
        assert_eq!(cap(&mut request, 1), 2);
        assert_eq!(
            request["messages"][0]["content"],
            json!([{"type":"text","text":OMITTED}, {"type":"text","text":"compare"},
                   {"type":"text","text":OMITTED}, image(2)])
        );
    }

    #[test]
    fn a_request_within_the_limit_is_untouched() {
        let mut request = session(20);
        let before = request.clone();
        assert_eq!(cap(&mut request, 20), 0);
        assert_eq!(request, before);
        let mut plain = json!({"messages":[{"role":"user","content":"hi"}]});
        assert_eq!(cap(&mut plain, 0), 0);
    }

    #[test]
    fn a_zero_limit_sends_no_image() {
        let mut request = session(3);
        assert_eq!(cap(&mut request, 0), 3);
        assert!(images(&request).is_empty());
    }
}
