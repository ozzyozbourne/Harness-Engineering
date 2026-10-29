//! The 8 required tests from SPEC.md, using the mock provider (offline after
//! the first fetch of serde_json). Userspace crates share one warm target dir.

use cmh::kernel::provider::Mock;
use cmh::{Config, Kernel};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<Value>>>;

fn target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/cmh-userspace")
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cmh-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn open(dir: &Path, script: Vec<Value>, soak: u32) -> (Kernel, Requests) {
    let mut cfg = Config::new(dir);
    cfg.target_dir = Some(target_dir());
    cfg.soak = soak;
    cfg.reflect = false;
    cfg.echo_text = false;
    let mock = Mock::new(script);
    let reqs = mock.requests();
    (Kernel::open(cfg, Box::new(mock)).expect("open kernel"), reqs)
}

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([{"type": "tool_use", "id": id, "name": name, "input": input}])
}

fn text(t: &str) -> Value {
    json!([{"type": "text", "text": t}])
}

fn status(k: &Kernel, name: &str) -> (Option<u32>, String) {
    let def = &k.state.defs[name];
    (def.live, def.live_version().map_or("none".into(), |v| v.status.clone()))
}

fn word_count_v2(source: &str, expect: &str) -> Value {
    json!({
        "name": "word_count", "kind": "tool", "source": source, "description": "test version",
        "tests": [{"input": {"text": "a b"}, "expect": expect}],
        "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}
    })
}

const V2_TAGGED: &str = r#"
pub fn run(input: &serde_json::Value) -> Result<String, String> {
    let t = input["text"].as_str().ok_or("text required")?;
    Ok(format!("v2:{}", t.split_whitespace().count()))
}"#;

/// Last tool_result content in a recorded request.
fn last_tool_result(req: &Value) -> String {
    let msgs = req["messages"].as_array().unwrap();
    msgs.iter()
        .rev()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

#[test]
fn t1_late_binding() {
    let dir = fresh_dir("late");
    let script = vec![
        tool_use("a", "word_count", json!({"text": "x y z"})),
        tool_use("b", "propose_def", word_count_v2(V2_TAGGED, "v2:2")),
        tool_use("c", "word_count", json!({"text": "x y z"})),
        text("done"),
    ];
    let (mut k, reqs) = open(&dir, script, 3);
    let out = k.run_task("count words twice");
    assert!(out.success, "{:?}", out.error);
    let reqs = reqs.lock().unwrap();
    assert_eq!(last_tool_result(&reqs[1]), "3");
    assert!(last_tool_result(&reqs[2]).starts_with("trial v2"), "{}", last_tool_result(&reqs[2]));
    assert_eq!(last_tool_result(&reqs[3]), "v2:3");
    assert_eq!(status(&k, "word_count"), (Some(2), "trial".into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t2_gate_rejection() {
    let dir = fresh_dir("reject");
    let (mut k, _) = open(&dir, vec![], 3);
    let bad_compile = word_count_v2("pub fn run(input: &serde_json::Value) -> Result<String, String> { 42 }", "2");
    let err = k.propose_def(&bad_compile).unwrap_err();
    assert!(err.contains("compile error") && err.contains("mismatched types"), "{err}");
    let bad_test = word_count_v2(V2_TAGGED, "this is not in the output");
    let err = k.propose_def(&bad_test).unwrap_err();
    assert!(err.contains("test 1 failed"), "{err}");
    let no_tests = json!({"name": "word_count", "kind": "tool", "source": V2_TAGGED, "description": "",
        "tests": [], "input_schema": {"type": "object"}});
    assert!(k.propose_def(&no_tests).is_err());
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    let out = k.call_def("word_count", &json!({"text": "a b c"}), |_| Ok(())).unwrap();
    assert_eq!(out, "3");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t3_soak_revert() {
    let dir = fresh_dir("revert");
    let (mut k, _) = open(&dir, vec![], 3);
    let src = r#"
pub fn run(input: &serde_json::Value) -> Result<String, String> {
    let t = input["text"].as_str().unwrap();
    if t.is_empty() { panic!("empty text"); }
    Ok(format!("v2:{}", t.split_whitespace().count()))
}"#;
    k.propose_def(&word_count_v2(src, "v2:2")).unwrap();
    assert_eq!(k.call_def("word_count", &json!({"text": "a"}), |_| Ok(())).unwrap(), "v2:1");
    let err = k.call_def("word_count", &json!({"text": ""}), |_| Ok(())).unwrap_err();
    assert!(err.contains("panic: empty text"), "{err}");
    assert_eq!(status(&k, "word_count"), (Some(1), "kept".into()));
    assert_eq!(k.state.defs["word_count"].get(2).unwrap().status, "reverted");
    assert_eq!(k.call_def("word_count", &json!({"text": "a b"}), |_| Ok(())).unwrap(), "2");
    let v = k.state.verdicts.iter().find(|v| v.name == "word_count" && v.version == 2).expect("verdict");
    assert_eq!(v.outcome, "reverted");
    assert!(v.text.contains("panic: empty text"), "{}", v.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t4_soak_keep() {
    let dir = fresh_dir("keep");
    let (mut k, _) = open(&dir, vec![], 3);
    k.propose_def(&word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    for i in 0..3 {
        assert_eq!(status(&k, "word_count"), (Some(2), "trial".into()), "before use {i}");
        k.call_def("word_count", &json!({"text": "a b"}), |_| Ok(())).unwrap();
    }
    assert_eq!(status(&k, "word_count"), (Some(2), "kept".into()));
    assert!(k.state.verdicts.iter().any(|v| v.version == 2 && v.outcome == "kept"));
    // Runtime errors from a kept version do not revert it.
    assert!(k.call_def("word_count", &json!({}), |_| Ok(())).is_err());
    assert_eq!(status(&k, "word_count"), (Some(2), "kept".into()));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t5_persistence() {
    let dir = fresh_dir("persist");
    let (mut k, _) = open(&dir, vec![], 3);
    k.propose_def(&word_count_v2(V2_TAGGED, "v2:2")).unwrap();
    k.call_def("word_count", &json!({"text": "a b"}), |_| Ok(())).unwrap();
    k.remember("Prefer small userspace changes.", "remember").unwrap();
    k.propose_def(&json!({"name": "style", "kind": "note", "source": "Answer tersely.", "description": "style"})).unwrap();
    let before = k.state.clone();
    assert_eq!(before.defs["word_count"].soak_uses, 1);
    drop(k);

    // Build artifacts are a cache: remove them and the fresh kernel rebuilds.
    std::fs::remove_dir_all(dir.join("build")).unwrap();
    let (mut k2, _) = open(&dir, vec![], 3);
    assert_eq!(k2.state, before);
    assert_eq!(k2.call_def("word_count", &json!({"text": "a b c"}), |_| Ok(())).unwrap(), "v2:3");
    assert_eq!(k2.state.defs["word_count"].soak_uses, 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t6_hook_redefinition() {
    let dir = fresh_dir("hook");
    let (mut k, reqs) = open(&dir, vec![text("first"), text("second")], 3);
    k.run_task("before");
    let src = r#"
pub fn compose_system(notes: &[String], lessons: &[String]) -> String {
    format!("CUSTOM-SUPPLEMENT notes={} lessons={}", notes.len(), lessons.len())
}"#;
    k.propose_def(&json!({"name": "compose_system", "kind": "hook", "source": src, "description": "custom",
        "tests": [{"input": {"notes": ["n"], "lessons": []}, "expect": "CUSTOM-SUPPLEMENT notes=1"}]}))
        .unwrap();
    k.run_task("after");
    let reqs = reqs.lock().unwrap();
    let system = |i: usize| -> Vec<String> {
        reqs[i]["system"].as_array().unwrap().iter().map(|b| b["text"].as_str().unwrap().to_string()).collect()
    };
    assert!(!system(0).join("\n").contains("CUSTOM-SUPPLEMENT"));
    let after = system(1);
    assert!(after[0].starts_with("You are cmh"), "base prompt must stay first");
    assert!(after[1].contains("CUSTOM-SUPPLEMENT notes=0 lessons=0"), "{after:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t7_lesson_scoring() {
    let dir = fresh_dir("score");
    // One successful reply; later tasks exhaust the script and fail.
    let (mut k, reqs) = open(&dir, vec![text("ok")], 3);
    k.remember("Parser changes need regression tests.", "remember").unwrap();
    let score = |k: &Kernel| (k.state.lessons[0].score, k.state.lessons[0].retired);
    assert!(k.run_task("fix the parser").success);
    assert_eq!(score(&k), (1, false));
    assert!(reqs.lock().unwrap()[0]["system"][1]["text"].as_str().unwrap().contains("Parser changes need"));
    for expected in [0, -1, -2] {
        assert!(!k.run_task("fix the parser").success);
        assert_eq!(score(&k), (expected, false));
    }
    assert!(!k.run_task("fix the parser").success);
    assert_eq!(score(&k), (-3, true));
    // Retired: no longer selected, so no further scoring.
    k.run_task("fix the parser");
    assert_eq!(score(&k), (-3, true));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn t8_end_to_end_run() {
    let dir = fresh_dir("e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let source = r#"
pub fn run(input: &serde_json::Value) -> Result<String, String> {
    let t = input.get("text").and_then(|v| v.as_str()).ok_or("text required")?;
    Ok(t.chars().rev().collect())
}"#;
    let script = json!([
        tool_use("p", "propose_def", json!({"name": "reverse", "kind": "tool", "source": source,
            "description": "Reverses a string", "tests": [{"input": {"text": "abc"}, "expect": "cba"}],
            "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}})),
        tool_use("r", "reverse", json!({"text": "hello"})),
        text("olleh"),
    ]);
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let state = dir.join("state");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cmh"))
        .env("CMH_TARGET_DIR", target_dir())
        .args(["--state", state.to_str().unwrap(), "--provider", "mock", "--script", script_path.to_str().unwrap()])
        .args(["--no-reflect", "run", "reverse hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stderr: {stderr}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "olleh\n", "stdout must be only the answer");
    assert!(stderr.contains("[tool] propose_def reverse -> trial v1"), "{stderr}");
    assert!(stderr.contains("[tool] reverse"), "{stderr}");
    let log = std::fs::read_to_string(state.join("mock-requests.jsonl")).unwrap();
    let reqs: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(reqs.len(), 3);
    let tools = |r: &Value| -> Vec<String> {
        r["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
    };
    assert!(!tools(&reqs[0]).contains(&"reverse".into()));
    assert!(tools(&reqs[1]).contains(&"reverse".into()));
    assert_eq!(last_tool_result(&reqs[2]), "olleh");

    // A failing run exits nonzero (script exhausted).
    std::fs::write(&script_path, "[]").unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_cmh"))
        .env("CMH_TARGET_DIR", target_dir())
        .args(["--state", state.to_str().unwrap(), "--provider", "mock", "--script", script_path.to_str().unwrap()])
        .args(["--no-reflect", "run", "anything"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
