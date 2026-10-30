//! Shared helpers for the integration tests (mock provider, offline).
#![allow(dead_code)]

use cmh::hv::provider::Mock;
use cmh::{Config, Hypervisor};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type Requests = Arc<Mutex<Vec<Value>>>;

/// A fresh temporary directory with `state/` and `work/`, removed when the
/// test ends (pass or fail).
pub struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new(name: &str) -> TestDir {
        let dir = std::env::temp_dir().join(format!("cmh-wasm-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work")).unwrap();
        TestDir(dir)
    }
    pub fn state(&self) -> PathBuf {
        self.0.join("state")
    }
    pub fn work(&self) -> PathBuf {
        self.0.join("work")
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn config(dir: &TestDir, soak: u32) -> Config {
    let mut cfg = Config::new(dir.state());
    cfg.work_dir = dir.work();
    cfg.soak = soak;
    cfg.reflect = false;
    cfg.echo_text = false;
    // Keep runaway guests in tests short.
    cfg.limits.slice = Duration::from_secs(3);
    cfg.limits.memory = 192 << 20;
    cfg
}

pub fn open(dir: &TestDir, script: Vec<Value>, soak: u32) -> (Hypervisor, Requests) {
    let mock = Mock::new(script);
    let reqs = mock.requests();
    (Hypervisor::open(config(dir, soak), Box::new(mock)).expect("open hypervisor"), reqs)
}

pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([{"type": "tool_use", "id": id, "name": name, "input": input}])
}

pub fn text(t: &str) -> Value {
    json!([{"type": "text", "text": t}])
}

pub fn state(hv: &mut Hypervisor) -> Value {
    serde_json::from_str(&hv.command("/state").expect("/state")).expect("state JSON")
}

/// (live version, status of the live version) of a definition.
pub fn status(hv: &mut Hypervisor, name: &str) -> (Option<u64>, String) {
    let s = state(hv);
    let def = &s["defs"][name];
    let live = def["live"].as_u64();
    let st = def["versions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| Some(v["version"].as_u64().unwrap()) == live)
        .map_or("none".to_string(), |v| v["status"].as_str().unwrap().to_string());
    (live, st)
}

pub fn verdicts(hv: &mut Hypervisor) -> Vec<Value> {
    state(hv)["verdicts"].as_array().unwrap().clone()
}

pub fn tool_def(name: &str, source: &str, input: Value, expect: &str) -> Value {
    json!({
        "name": name, "kind": "tool", "source": source, "description": "test version",
        "tests": [{"input": input, "expect": expect}],
        "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    })
}

pub fn word_count_v2(source: &str, expect: &str) -> Value {
    tool_def("word_count", source, json!({"text": "a b"}), expect)
}

pub fn propose(hv: &mut Hypervisor, def: &Value) -> Result<String, String> {
    hv.command(&format!("/propose {def}"))
}

pub fn call(hv: &mut Hypervisor, name: &str, input: Value) -> Result<String, String> {
    hv.command(&format!("/call {name} {input}"))
}

pub fn wc(hv: &mut Hypervisor, t: &str) -> Result<String, String> {
    call(hv, "word_count", json!({"text": t}))
}

/// word_count v2 that tags its output with "v2:".
pub const V2_TAGGED: &str = r#"
: count ( a u -- n )  0 >r begin bl skip dup 0> while r> 1+ >r bl scan repeat 2drop r> ;
: skip ( a u c -- a' u' ) drop begin dup 0> if over c@ bl <= else false then while 1 /string repeat ;
: scan ( a u c -- a' u' ) drop begin dup 0> if over c@ bl >  else false then while 1 /string repeat ;
: run ( json-a json-u -- a u )  s" text" json-get count  <b s" v2:" b+ bn+ b> ;
"#;

pub const REVERSE: &str = r#"
\ {"text": string} -> the text reversed
: rev ( a u -- a2 u2 ) <b dup 0 ?do 2dup + i - 1- c@ bc+ loop 2drop b> ;
: run ( json-a json-u -- a u ) s" text" json-get rev ;
"#;

/// Last tool_result content in a recorded request.
pub fn last_tool_result(req: &Value) -> String {
    let msgs = req["messages"].as_array().unwrap();
    msgs.iter()
        .rev()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

pub fn system_texts(req: &Value) -> Vec<String> {
    req["system"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect()
}

pub fn tool_names(req: &Value) -> Vec<String> {
    req["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

/// Runs the release-or-debug `cmh` binary in `dir.work()` with a mock script.
pub fn run_bin(dir: &TestDir, script: &Value, args: &[&str]) -> (i32, String, String) {
    let script_path = dir.0.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cmh"))
        .current_dir(dir.work())
        .args(["--state", dir.state().to_str().unwrap(), "--provider", "mock", "--script", script_path.to_str().unwrap()])
        .arg("--no-reflect")
        .args(args)
        .output()
        .unwrap();
    let s = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    (out.status.code().unwrap_or(-1), s(&out.stdout), s(&out.stderr))
}

pub fn mock_log(dir: &TestDir) -> Vec<Value> {
    let log = std::fs::read_to_string(dir.state().join("mock-requests.jsonl")).unwrap_or_default();
    log.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

pub fn journal(dir: &TestDir) -> Vec<Value> {
    let text = std::fs::read_to_string(dir.state().join("journal.jsonl")).unwrap();
    text.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}
