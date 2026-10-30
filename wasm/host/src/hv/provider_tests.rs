//! Unit tests for the request shape (OAuth system blocks, base URL).

use super::provider::{CLAUDE_CODE_IDENTITY, messages_url, request_body};
use serde_json::json;

fn system() -> Vec<String> {
    vec!["BASE".into(), "SUPP".into()]
}

#[test]
fn oauth_identity_is_first_system_block() {
    let tools = [json!({"name": "bash"})];
    let b = request_body("m", &system(), &json!([]), &tools, true);
    assert_eq!(b["system"][0]["text"], CLAUDE_CODE_IDENTITY);
    assert_eq!(b["system"][1]["text"], "BASE");
    assert_eq!(b["system"][2]["text"], "SUPP");
    assert_eq!(b["max_tokens"], 32000);
}

#[test]
fn api_key_has_base_prompt_first() {
    let b = request_body("m", &system(), &json!([]), &[], false);
    assert_eq!(b["system"][0]["text"], "BASE");
    assert!(b.get("tools").is_none());
}

#[test]
fn base_url() {
    assert_eq!(messages_url("http://127.0.0.1:8080/"), "http://127.0.0.1:8080/v1/messages");
    assert_eq!(messages_url("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
}
