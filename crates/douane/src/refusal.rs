//! Diagnostics derived from known provider error shapes.

/// Persist only the documented shape of the Mistral image-limit error.
pub fn image_limit(value: &serde_json::Value) -> Option<String> {
    let message = value["message"].as_str()?;
    let limit = message
        .strip_prefix("Too many images in request. Max is ")?
        .strip_suffix('.')?
        .parse::<u32>()
        .ok()?;
    Some(format!("Too many images in request. Max is {limit}."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn image_limit_accepts_only_the_known_numeric_diagnostic() {
        let message = "Too many images in request. Max is 20.";
        assert_eq!(
            image_limit(&json!({"message": message})).as_deref(),
            Some(message)
        );
        assert!(
            image_limit(&json!({"message":format!("{message} echoed request content")})).is_none()
        );
    }
}
