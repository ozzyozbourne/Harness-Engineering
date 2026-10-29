//! Model providers: the Anthropic Messages API and a scripted mock.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

pub struct ModelRequest<'a> {
    /// System blocks chosen by the kernel: base prompt, then supplemental.
    pub system: Vec<String>,
    pub tools: &'a [Value],
    pub messages: &'a [Value],
}

pub struct ModelResponse {
    pub content: Vec<Value>,
    pub stop_reason: String,
}

pub trait Provider {
    fn complete(&mut self, req: &ModelRequest) -> Result<ModelResponse, String>;
}

fn system_blocks(texts: &[String], oauth: bool) -> Vec<Value> {
    let mut blocks = Vec::new();
    if oauth {
        blocks.push(json!({"type": "text", "text": CLAUDE_CODE_IDENTITY}));
    }
    blocks.extend(texts.iter().filter(|t| !t.trim().is_empty()).map(|t| json!({"type": "text", "text": t})));
    blocks
}

/// Builds the exact JSON body sent to `/v1/messages`.
pub fn request_body(model: &str, req: &ModelRequest, oauth: bool) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": 32000,
        "system": system_blocks(&req.system, oauth),
        "messages": req.messages,
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(req.tools.to_vec());
    }
    body
}

enum Auth {
    ApiKey(String),
    Bearer(String),
    Missing,
}

pub struct Anthropic {
    model: String,
    auth: Auth,
    agent: ureq::Agent,
    url: String,
}

impl Anthropic {
    pub fn from_env(model: String) -> Anthropic {
        let nonempty = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let auth = match (nonempty("ANTHROPIC_API_KEY"), nonempty("CLAUDE_CODE_OAUTH_TOKEN")) {
            (Some(k), _) => Auth::ApiKey(k),
            (None, Some(t)) => Auth::Bearer(t),
            _ => Auth::Missing,
        };
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(600)).build();
        let base = nonempty("ANTHROPIC_BASE_URL").unwrap_or_else(|| "https://api.anthropic.com".into());
        Anthropic { model, auth, agent, url: messages_url(&base) }
    }
}

/// `$ANTHROPIC_BASE_URL/v1/messages`; plain `http://` bases are allowed.
pub fn messages_url(base: &str) -> String {
    format!("{}/v1/messages", base.trim().trim_end_matches('/'))
}

impl Provider for Anthropic {
    fn complete(&mut self, req: &ModelRequest) -> Result<ModelResponse, String> {
        let oauth = matches!(self.auth, Auth::Bearer(_));
        let body = request_body(&self.model, req, oauth);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut r = self
                .agent
                .post(&self.url)
                .set("anthropic-version", "2023-06-01")
                .set("content-type", "application/json");
            r = match &self.auth {
                Auth::ApiKey(k) => r.set("x-api-key", k),
                Auth::Bearer(t) => r
                    .set("authorization", &format!("Bearer {t}"))
                    .set("anthropic-beta", "oauth-2025-04-20"),
                Auth::Missing => return Err("no credentials: set ANTHROPIC_API_KEY or CLAUDE_CODE_OAUTH_TOKEN".into()),
            };
            match r.send_json(&body) {
                Ok(resp) => {
                    let v: Value = resp.into_json().map_err(|e| format!("bad API response: {e}"))?;
                    let content = v.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
                    let stop_reason = v.get("stop_reason").and_then(Value::as_str).unwrap_or("").to_string();
                    return Ok(ModelResponse { content, stop_reason });
                }
                Err(ureq::Error::Status(code, resp)) => {
                    let text = resp.into_string().unwrap_or_default();
                    let retryable = matches!(code, 429 | 500 | 502 | 503 | 504 | 529);
                    if retryable && attempt < 4 {
                        std::thread::sleep(Duration::from_secs(2u64.pow(attempt)));
                        continue;
                    }
                    let mut text = text;
                    text.truncate(text.char_indices().nth(1000).map_or(text.len(), |(i, _)| i));
                    return Err(format!("API error {code}: {text}"));
                }
                Err(e) => {
                    if attempt < 3 {
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    return Err(format!("API transport error: {e}"));
                }
            }
        }
    }
}

/// Replays a JSON array of assistant `content` arrays, one per request, and
/// records every request body it received.
pub struct Mock {
    script: Vec<Value>,
    next: usize,
    requests: Arc<Mutex<Vec<Value>>>,
    log: Option<PathBuf>,
}

impl Mock {
    pub fn new(script: Vec<Value>) -> Mock {
        Mock { script, next: 0, requests: Arc::default(), log: None }
    }

    pub fn from_file(path: &std::path::Path) -> Result<Mock, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("read script {}: {e}", path.display()))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("parse script: {e}"))?;
        match v {
            Value::Array(items) => Ok(Mock::new(items)),
            _ => Err("mock script must be a JSON array of content arrays".into()),
        }
    }

    /// Also append each recorded request to this JSONL file.
    pub fn with_log(mut self, path: PathBuf) -> Mock {
        self.log = Some(path);
        self
    }

    /// Shared handle to the recorded requests.
    pub fn requests(&self) -> Arc<Mutex<Vec<Value>>> {
        self.requests.clone()
    }
}

impl Provider for Mock {
    fn complete(&mut self, req: &ModelRequest) -> Result<ModelResponse, String> {
        let body = request_body("mock", req, false);
        if let Some(path) = &self.log {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{body}");
            }
        }
        self.requests.lock().unwrap_or_else(|e| e.into_inner()).push(body);
        let Some(entry) = self.script.get(self.next) else {
            return Err(format!("mock script exhausted after {} responses", self.next));
        };
        self.next += 1;
        let content = match entry {
            Value::Array(blocks) => blocks.clone(),
            Value::String(s) => vec![json!({"type": "text", "text": s})],
            other => return Err(format!("mock entry {} is not a content array: {other}", self.next)),
        };
        let uses_tool = content.iter().any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"));
        Ok(ModelResponse { content, stop_reason: if uses_tool { "tool_use" } else { "end_turn" }.into() })
    }
}
