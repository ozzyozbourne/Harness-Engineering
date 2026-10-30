//! The image's built-in mock scenarios (the `self-test` export). Each runs a
//! fresh kernel against `MemSys`: an in-memory journal and a scripted model,
//! so the self-test has no side effects. The hypervisor runs this before a
//! new image may go live, and then its own conformance scenario.

use crate::kernel::Kernel;
use crate::sys::{MemSys, SELFTEST_BASE, Sys};
use serde_json::{Value, json};
use std::rc::Rc;

fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!([{"type": "tool_use", "id": id, "name": name, "input": input}])
}

fn text(t: &str) -> Value {
    json!([{"type": "text", "text": t}])
}

const V2_TAGGED: &str = r#"
: count ( a u -- n )  0 >r begin bl skip dup 0> while r> 1+ >r bl scan repeat 2drop r> ;
: skip ( a u c -- a' u' ) drop begin dup 0> if over c@ bl <= else false then while 1 /string repeat ;
: scan ( a u c -- a' u' ) drop begin dup 0> if over c@ bl >  else false then while 1 /string repeat ;
: run ( json-a json-u -- a u )  s" text" json-get count  <b s" v2:" b+ bn+ b> ;
"#;

fn word_count_v2(source: &str, expect: &str) -> Value {
    json!({"name": "word_count", "kind": "tool", "source": source, "description": "self-test version",
        "tests": [{"input": {"text": "a b"}, "expect": expect}],
        "input_schema": {"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}})
}

/// The content of the last tool_result in a recorded request.
fn last_tool_result(req: &Value) -> String {
    req["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .rev()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result")
        .map(|b| b["content"].as_str().unwrap_or("").to_string())
        .unwrap_or_default()
}

fn check(cond: bool, what: &str) -> Result<(), String> {
    if cond { Ok(()) } else { Err(what.to_string()) }
}

fn open(sys: &Rc<MemSys>) -> Result<Kernel, String> {
    Kernel::open(sys.clone() as Rc<dyn Sys>)
}

/// Seeds install and pass their own tests.
fn seeds() -> Result<(), String> {
    let sys = Rc::new(MemSys::default());
    let mut k = open(&sys)?;
    for name in ["word_count", "compose_system", "select_lessons"] {
        let v = k.state.live(name).ok_or(format!("seed {name} missing"))?.clone();
        check(v.status == "kept", &format!("seed {name} is not kept"))?;
        for t in v.tests.as_array().into_iter().flatten() {
            let out = k.call_def(name, &t["input"])?;
            check(out.contains(t["expect"].as_str().unwrap_or("")), &format!("seed {name} failed its test: {out:?}"))?;
        }
    }
    Ok(())
}

/// A task where the model proposes a new word_count and the next call uses it.
fn late_binding_task() -> Result<(), String> {
    let sys = Rc::new(MemSys::with_script(vec![
        tool_use("a", "word_count", json!({"text": "x y z"})),
        tool_use("b", "propose_def", word_count_v2(V2_TAGGED, "v2:2")),
        tool_use("c", "word_count", json!({"text": "x y z"})),
        text("done"),
    ]));
    let mut k = open(&sys)?;
    let out = k.run_task("count words twice");
    check(out.success, &format!("task failed: {:?}", out.error))?;
    check(out.answer == "done", "wrong final answer")?;
    let reqs = sys.requests.borrow();
    check(reqs.len() == 4, &format!("expected 4 model requests, got {}", reqs.len()))?;
    check(last_tool_result(&reqs[1]) == "3", &format!("word_count v1 gave {:?}", last_tool_result(&reqs[1])))?;
    check(last_tool_result(&reqs[2]).starts_with("trial v2"), &format!("proposal: {:?}", last_tool_result(&reqs[2])))?;
    check(last_tool_result(&reqs[3]) == "v2:3", &format!("late binding: {:?}", last_tool_result(&reqs[3])))?;
    let system = &reqs[0]["system"];
    check(system[0] == SELFTEST_BASE, "the base prompt must be the first system block")?;
    let tools: Vec<&str> = reqs[0]["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
    for t in crate::tools::KERNEL_TOOLS.iter().chain(["word_count"].iter()) {
        check(tools.contains(t), &format!("tool {t} missing from the tool list"))?;
    }
    Ok(())
}

/// The gate rejects broken and failing sources and keeps the live version.
fn gate_rejection() -> Result<(), String> {
    let sys = Rc::new(MemSys::default());
    let mut k = open(&sys)?;
    let err = k.propose_def(&word_count_v2(": run ( a u -- a u ) s\" x\" ", "2")).err().unwrap_or_default();
    check(err.contains("compile-error"), &format!("broken source: {err:?}"))?;
    let err = k.propose_def(&word_count_v2(V2_TAGGED, "not in the output")).err().unwrap_or_default();
    check(err.contains("test 1 failed"), &format!("failing test: {err:?}"))?;
    check(k.state.live("word_count").map(|v| v.version) == Some(1), "live version changed")?;
    Ok(())
}

/// A trial that errors is reverted with a verdict; replay rebuilds the same state.
fn revert_and_replay() -> Result<(), String> {
    let sys = Rc::new(MemSys::default());
    let mut k = open(&sys)?;
    let src = r#": run ( a u -- a u ) s" text" json-get nip 100 swap / n>s ;"#;
    k.propose_def(&word_count_v2(src, "33"))?;
    check(k.call_def("word_count", &json!({"text": ""})).is_err(), "division by zero must fail")?;
    check(k.state.live("word_count").map(|v| v.version) == Some(1), "trial not reverted")?;
    check(k.state.verdicts.iter().any(|v| v.text.contains("division-by-zero")), "no verdict")?;
    k.remember("Self-test lesson.", "remember")?;
    let before = k.state.to_json();
    let k2 = open(&sys)?;
    check(k2.state.to_json() == before, "replay did not reproduce the state")?;
    Ok(())
}

type Scenario = (&'static str, fn() -> Result<(), String>);

/// Runs every scenario; all must pass.
pub fn run() -> Result<String, String> {
    let scenarios: [Scenario; 4] = [
        ("seeds", seeds),
        ("late-binding task", late_binding_task),
        ("gate rejection", gate_rejection),
        ("revert and replay", revert_and_replay),
    ];
    for (name, f) in scenarios {
        f().map_err(|e| format!("self-test scenario `{name}` failed: {e}"))?;
    }
    Ok(format!("{} self-test scenarios passed", scenarios.len()))
}
