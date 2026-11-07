//! What an upstream wrote when it refused a request, reduced to a short line
//! the call ledger and the log can keep.
//!
//! A bare status says nothing about the cause: a 503 can be an overloaded
//! upstream or a request it will never take (Mistral past 20 images). Google
//! refusals are not read here: their free text can name the project or the
//! account, so they keep their enum reason (`relay::google_refusal`).

/// How much of the refusal is kept.
const EXCERPT_CHARS: usize = 300;

/// The first [`EXCERPT_CHARS`] characters of an upstream error body on one
/// line, with `secrets` (the account's own credentials) and anything that
/// looks like a bearer token or an API key replaced by `[redacted]`, or
/// `None` when the body holds no text.
pub fn excerpt(body: &[u8], secrets: &[&str]) -> Option<String> {
    // Read a little past the excerpt so a token crossing its end is still
    // seen whole, and redacted, before the cut.
    let text = String::from_utf8_lossy(&body[..body.len().min(16 * 1024)]);
    let mut line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.is_empty() {
        return None;
    }
    for secret in secrets.iter().map(|s| s.trim()).filter(|s| s.len() >= 8) {
        line = line.replace(secret, "[redacted]");
    }
    let mut out = String::with_capacity(line.len());
    // Whether the last token was `Bearer` with only a space since.
    let mut after_bearer = false;
    let mut rest = line.as_str();
    while let Some(c) = rest.chars().next() {
        let token = token_char(c);
        let end = rest
            .find(|c: char| token_char(c) != token)
            .unwrap_or(rest.len());
        let (run, tail) = rest.split_at(end);
        if !token {
            out.push_str(run);
            after_bearer &= run == " ";
        } else if after_bearer || secret_like(run) {
            out.push_str("[redacted]");
            after_bearer = false;
        } else {
            out.push_str(run);
            after_bearer = run.eq_ignore_ascii_case("bearer");
        }
        rest = tail;
    }
    Some(out.chars().take(EXCERPT_CHARS).collect())
}

/// Characters API keys, JWTs and bearer tokens are made of.
fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
}

/// Prefixes API keys and tokens are published with.
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_",
    "rk_",
    "key-",
    "xai-",
    "gsk_",
    "csk-",
    "hf_",
    "nvapi-",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "glpat-",
    "AIza",
    "ya29.",
    "eyJ",
];

/// A run that reads as a credential: 20 or more characters behind a known
/// key prefix, or 32 or more mixing at least two of digits, lowercase and
/// uppercase letters. A UUID (a request id) is not one.
fn secret_like(run: &str) -> bool {
    if run.len() >= 20 && KEY_PREFIXES.iter().any(|p| run.starts_with(p)) {
        return true;
    }
    let classes = [
        run.bytes().any(|b| b.is_ascii_digit()),
        run.bytes().any(|b| b.is_ascii_lowercase()),
        run.bytes().any(|b| b.is_ascii_uppercase()),
    ];
    run.len() >= 32 && classes.iter().filter(|c| **c).count() >= 2 && !uuid(run)
}

fn uuid(run: &str) -> bool {
    let groups: Vec<&str> = run.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_refusal_is_kept_on_one_line() {
        let body = br#"{"object":"error",
  "message":"Too many images in request. Max is 20.","type":"invalid_request_error","code":"3051"}"#;
        assert_eq!(
            excerpt(body, &[]).unwrap(),
            r#"{"object":"error", "message":"Too many images in request. Max is 20.","type":"invalid_request_error","code":"3051"}"#
        );
    }

    #[test]
    fn the_excerpt_stops_at_three_hundred_characters() {
        let body = "é".repeat(1000);
        assert_eq!(excerpt(body.as_bytes(), &[]).unwrap().chars().count(), 300);
    }

    #[test]
    fn tokens_are_redacted() {
        let body = br#"{"error":"Authorization: Bearer abc.def-ghi rejected; key sk-proj-0123456789abcdefghijABCDEFGHIJ0123 unknown","request_id":"req_42","id":"3f2b8c1e-9a4d-4e7f-b6a1-2c3d4e5f6a7b"}"#;
        let text = excerpt(body, &[]).unwrap();
        assert!(!text.contains("abc.def-ghi"), "{text}");
        assert!(!text.contains("sk-proj-0123"), "{text}");
        assert!(text.contains("Bearer [redacted] rejected"), "{text}");
        assert!(text.contains("key [redacted] unknown"), "{text}");
        // Short identifiers and request ids stay readable.
        assert!(text.contains("req_42"), "{text}");
        assert!(
            text.contains("3f2b8c1e-9a4d-4e7f-b6a1-2c3d4e5f6a7b"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_body_has_no_excerpt() {
        assert_eq!(excerpt(b"", &[]), None);
        assert_eq!(excerpt(b" \n ", &[]), None);
    }

    /// Sol round 1: a key without a digit passed intact.
    #[test]
    fn a_key_without_digits_and_the_account_key_are_redacted() {
        let body = br#"{"error":"invalid key sk-abcdefghijklmnopqrstuvwxyzABCDEF","echo":"my-own-key-value"}"#;
        let text = excerpt(body, &["my-own-key-value"]).unwrap();
        assert_eq!(
            text,
            r#"{"error":"invalid key [redacted]","echo":"[redacted]"}"#
        );
        // Plain words stay, however long.
        let words = br#"{"message":"internationalization_configuration_unavailable"}"#;
        assert_eq!(
            excerpt(words, &[]).unwrap(),
            r#"{"message":"internationalization_configuration_unavailable"}"#
        );
    }
}
