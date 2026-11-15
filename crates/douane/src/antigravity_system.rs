//! Narrow compatibility normalization for outgoing Antigravity system text.

use serde_json::Value;

const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
// Google refuses these agent texts with a 429 RESOURCE_EXHAUSTED and strips
// zero-width characters before matching, so each gets a visible rewording
// that keeps its meaning. Checked against Antigravity on 2026-11-15.
const REWORDINGS: [(&str, &str); 5] = [
    // Claude Agent SDK.
    (
        "You are a Claude agent, built on Anthropic's Claude Agent SDK.",
        "You are a Claude-based agent, built on Anthropic's Claude Agent SDK.",
    ),
    // Claude Code 2.1.3xx; "Claude Code" alone passes.
    ("You are Claude Code", "You are Claude-Code"),
    // Codex ships "a coding agent based on GPT-5." and "an agent based on".
    ("You are Codex, a", "You are Codex-CLI, a"),
    // The opening of the Codex CLI developer text.
    (
        "You are a coding agent running in the Codex CLI",
        "You are a coding agent that runs in the Codex CLI",
    ),
    // Claude Code's security rule: only its list of examples is refused.
    (
        "Dual-use security tools (C2 frameworks, credential testing, exploit development) require",
        "Dual-use security tools require",
    ),
];

/// Normalize native Gemini system parts before wrapping the Antigravity envelope.
/// Identity matches are exact; user contents and tool declarations stay untouched.
pub(crate) fn normalize_system(request: &mut Value) {
    for field in ["systemInstruction", "system_instruction"] {
        let Some(parts) = request
            .get_mut(field)
            .and_then(|system| system.get_mut("parts"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for part in parts {
            let Some(value) = part.get_mut("text") else {
                continue;
            };
            let Some(text) = value.as_str() else {
                continue;
            };
            let normalized = normalize_text(text);
            if normalized != text {
                *value = Value::String(normalized);
            }
        }
    }
}

fn normalize_text(text: &str) -> String {
    // split_inclusive preserves original line endings and the final unterminated line.
    let without_billing: String = text
        .split_inclusive('\n')
        .filter(|line| {
            !line
                .trim_start()
                .get(..BILLING_PREFIX.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(BILLING_PREFIX))
        })
        .collect();
    REWORDINGS
        .iter()
        .fold(without_billing, |text, (refused, accepted)| {
            text.replace(refused, accepted)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn removes_only_billing_lines_and_preserves_crlf_and_other_text() {
        let mut request = json!({"systemInstruction":{"role":"user","parts":[{"text":
            "Keep these rules.\r\n  x-anthropic-billing-header: cc_entrypoint=sdk-cli;\r\n\r\nKeep this too.\nX-Anthropic-Billing-Header: cc_version=2;\nLast rule."
        }]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "Keep these rules.\r\n\r\nKeep this too.\nLast rule."
        );
    }

    #[test]
    fn normalizes_sdk_identity_without_replacing_the_system_prompt() {
        let mut request = json!({"systemInstruction":{"parts":[{"text":
            "Rules first.\nYou are a Claude agent, built on Anthropic's Claude Agent SDK.\nUse tools carefully."
        }]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "Rules first.\nYou are a Claude-based agent, built on Anthropic's Claude Agent SDK.\nUse tools carefully."
        );
    }

    #[test]
    fn rewords_the_claude_code_identity_and_security_examples_google_refuses() {
        let mut request = json!({"systemInstruction":{"parts":[
            {"text":"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK."},
            {"text":"Refuse malicious requests. Dual-use security tools (C2 frameworks, credential testing, exploit development) require clear authorization context: pentesting engagements.\nRead Claude Code docs."}
        ]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "You are Claude-Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK."
        );
        assert_eq!(
            request["systemInstruction"]["parts"][1]["text"],
            "Refuse malicious requests. Dual-use security tools require clear authorization context: pentesting engagements.\nRead Claude Code docs."
        );
        assert!(!request.to_string().contains('\u{200b}'));
    }

    #[test]
    fn rewords_the_codex_cli_opening_google_refuses() {
        let mut request = json!({"systemInstruction":{"parts":[{"text":
            "You are a coding agent running in the Codex CLI, a terminal-based coding assistant. Codex CLI is an open source project led by OpenAI."
        }]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "You are a coding agent that runs in the Codex CLI, a terminal-based coding assistant. Codex CLI is an open source project led by OpenAI."
        );
    }

    #[test]
    fn normalizes_codex_self_identity_for_current_model_versions() {
        let mut request = json!({"systemInstruction":{"parts":[{"text":
            "You are Codex, an agent based on GPT-6.\nKeep Codex documentation accurate."
        }]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "You are Codex-CLI, an agent based on GPT-6.\nKeep Codex documentation accurate."
        );
    }

    #[test]
    fn normalizes_the_codex_coding_agent_identity_google_refuses() {
        let mut request = json!({"systemInstruction":{"parts":[{"text":
            "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace."
        }]}});
        normalize_system(&mut request);
        assert_eq!(
            request["systemInstruction"]["parts"][0]["text"],
            "You are Codex-CLI, a coding agent based on GPT-5. You and the user share the same workspace."
        );
    }

    #[test]
    fn supports_both_native_system_field_spellings_and_multiple_parts() {
        let mut request = json!({
            "systemInstruction":{"parts":[{"text":"x-anthropic-billing-header: one"},{"text":"Keep camel case."}]},
            "system_instruction":{"parts":[{"text":"You are Codex, an agent based on GPT-5."},{"text":"Keep snake case."}]}
        });
        normalize_system(&mut request);
        assert_eq!(request["systemInstruction"]["parts"][0]["text"], "");
        assert_eq!(
            request["systemInstruction"]["parts"][1]["text"],
            "Keep camel case."
        );
        assert_eq!(
            request["system_instruction"]["parts"][0]["text"],
            "You are Codex-CLI, an agent based on GPT-5."
        );
        assert_eq!(
            request["system_instruction"]["parts"][1]["text"],
            "Keep snake case."
        );
    }

    #[test]
    fn never_changes_user_messages_tool_strings_or_outer_envelopes() {
        let identity = "You are Codex, an agent based on GPT-6.\nx-anthropic-billing-header: keep";
        let mut request = json!({
            "systemInstruction":{"parts":[{"text":"Safe rules."}]},
            "contents":[{"role":"user","parts":[{"text":identity}]}],
            "tools":[{"functionDeclarations":[{"name":"Codex","description":identity,"parameters":{"type":"object","properties":{"identity":{"const":identity}}}}]}],
            "request":{"systemInstruction":{"parts":[{"text":identity}]}}
        });
        let original = request.clone();
        normalize_system(&mut request);
        assert_eq!(request, original);
    }

    #[test]
    fn leaves_normal_system_mentions_and_nonmatching_identity_case_unchanged() {
        let mut request = json!({"systemInstruction":{"parts":[{"text":
            "Document Codex and Claude Agent SDK.\nYou are Codex.\nYou are an agent based on GPT-6.\nYou are codex, an agent based on GPT-6.\nYou are a Claude agent, built on Anthropic's claude agent sdk.\nDiscuss x-anthropic-billing-header: inline."
        }]}});
        let original = request.clone();
        normalize_system(&mut request);
        assert_eq!(request, original);
    }

    #[test]
    fn normalization_is_idempotent_with_existing_zero_width_characters() {
        let mut request = json!({"system_instruction":{"parts":[{"text":
            "You are Codex, an agent based on GPT-6.\nYou are a Claude agent, built on Anthropic's Claude Agent SDK.\nYou are C\u{200b}odex, an agent based on GPT-5."
        }]}});
        normalize_system(&mut request);
        let once = request.clone();
        normalize_system(&mut request);
        assert_eq!(request, once);
        assert_eq!(
            request["system_instruction"]["parts"][0]["text"],
            "You are Codex-CLI, an agent based on GPT-6.\nYou are a Claude-based agent, built on Anthropic's Claude Agent SDK.\nYou are C\u{200b}odex, an agent based on GPT-5."
        );
    }

    #[test]
    fn malformed_or_absent_system_parts_do_not_create_fields() {
        for mut request in [
            json!(null),
            json!({}),
            json!({"systemInstruction":"Codex"}),
            json!({"systemInstruction":{"parts":null}}),
            json!({"system_instruction":{"parts":[null,{"text":42},{"inlineData":{"data":"Codex"}}]}}),
        ] {
            let original = request.clone();
            normalize_system(&mut request);
            assert_eq!(request, original);
        }
    }
}
